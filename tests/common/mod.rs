//! Shared helpers for the integration tests: a loopback HTTP server, a
//! hand-rolled gzip encoder, stub executables and an isolated command builder.
//! Nothing here talks to the internet.
#![allow(dead_code)]

use artfetch::model::RatingPolicy;
use artfetch::stage::StageConfig;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Declare the full `Content-Length` but close the socket after this many bytes.
    pub truncate_to: Option<usize>,
}

impl Reply {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            truncate_to: None,
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_string(), location.to_string())],
            body: Vec::new(),
            truncate_to: None,
        }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

/// A tiny HTTP/1.1 server on 127.0.0.1 that answers each path (query ignored)
/// with a canned reply and records every request path it saw.
pub struct FakeServer {
    pub addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    pub fn start(routes: Vec<(&str, Reply)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let routes: Arc<HashMap<String, Reply>> = Arc::new(
            routes
                .into_iter()
                .map(|(p, r)| (p.to_string(), r))
                .collect(),
        );
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_srv = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let routes = routes.clone();
                let hits = hits_srv.clone();
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &routes, &hits);
                });
            }
        });
        Self { addr, hits }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn serve_one(
    stream: TcpStream,
    routes: &HashMap<String, Reply>,
    hits: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut discard = vec![0u8; content_length];
    reader.read_exact(&mut discard)?;

    let path = target.split('?').next().unwrap_or("/").to_string();
    hits.lock().unwrap().push(path.clone());
    let reply = routes.get(&path).cloned().unwrap_or(Reply {
        status: 404,
        headers: Vec::new(),
        body: b"not found".to_vec(),
        truncate_to: None,
    });

    let mut out = stream;
    let mut head = format!("HTTP/1.1 {} X\r\n", reply.status);
    for (k, v) in &reply.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    out.write_all(head.as_bytes())?;
    let send = reply.truncate_to.unwrap_or(reply.body.len());
    out.write_all(&reply.body[..send.min(reply.body.len())])?;
    out.flush()?;
    Ok(())
}

fn crc32(data_len: usize, byte: u8) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for _ in 0..data_len {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            self.acc |= ((value >> i) & 1) << self.nbits;
            self.nbits += 1;
            if self.nbits == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.nbits = 0;
            }
        }
    }
    /// Huffman codes go most-significant bit first.
    fn code(&mut self, code: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits((code >> i) & 1, 1);
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// A valid gzip member holding `n` zero bytes, compressed about 160 times with
/// fixed-Huffman length 258 / distance 1 back-references.
pub fn gzip_zeros(n: usize) -> Vec<u8> {
    assert!(n >= 1);
    let mut w = BitWriter {
        out: Vec::new(),
        acc: 0,
        nbits: 0,
    };
    w.bits(1, 1); // BFINAL
    w.bits(1, 2); // BTYPE = fixed Huffman
    w.code(0x30, 8); // literal 0x00
    let mut remaining = n - 1;
    while remaining >= 258 {
        w.code(0xC0 + 5, 8); // length symbol 285 = 258 bytes
        w.code(0, 5); // distance code 0 = distance 1
        remaining -= 258;
    }
    for _ in 0..remaining {
        w.code(0x30, 8);
    }
    w.code(0, 7); // end of block (256)
    let deflate = w.finish();

    let mut gz = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];
    gz.extend_from_slice(&deflate);
    gz.extend_from_slice(&crc32(n, 0).to_le_bytes());
    gz.extend_from_slice(&(n as u32).to_le_bytes());
    gz
}

/// Writes an executable `#!/bin/sh` stub into `dir`.
#[cfg(unix)]
pub fn write_stub(dir: &Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const PROXY_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// The artfetch binary with `stub_dir` first on PATH (then only the system
/// directories, so no real `twitter` or `gallery-dl` is found), no proxy, and a credentials
/// file path that does not exist, so no real credential is ever read.
pub fn artfetch_cmd(stub_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_artfetch"));
    let path = std::env::join_paths([stub_dir, Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    cmd.env("PATH", path)
        .env("HOME", stub_dir)
        .env("ARTFETCH_ENV_FILE", stub_dir.join("no-credentials.env"))
        .env_remove("GELBOORU_API_KEY")
        .env_remove("GELBOORU_USER_ID");
    for v in PROXY_VARS {
        cmd.env_remove(v);
    }
    cmd
}

/// Like [`artfetch_cmd`], but every outbound request is sent to a dead local
/// proxy so that hard-coded public hosts are never contacted.
pub fn artfetch_cmd_offline(stub_dir: &Path) -> Command {
    let mut cmd = artfetch_cmd(stub_dir);
    for v in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        cmd.env(v, "http://127.0.0.1:1");
    }
    cmd
}

/// A solid-colour PNG of the given size.
pub fn write_png(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
    img.save(path).unwrap();
}

pub fn stage_cfg(in_dir: &Path, out_dir: &Path, prefix: &str, run: bool) -> StageConfig {
    StageConfig {
        in_dirs: vec![in_dir.to_path_buf()],
        out_dir: out_dir.to_path_buf(),
        policy: RatingPolicy::resolve(Some("all"), true).unwrap(),
        exclude_censored: false,
        allow_unpolished: true,
        allow_unverified_sfw: true,
        min_long_edge: 1,
        min_short_edge: 1,
        max_edge: 1536,
        jpeg_quality: 90,
        dhash_max_dist: 6,
        prefix: prefix.to_string(),
        trigger: None,
        tagger_json: None,
        run,
    }
}

/// Every regular file under `root`, without following links.
pub fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

pub fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}); status {:?}; stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}
