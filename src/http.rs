use futures_util::StreamExt;
use reqwest::redirect::Policy;
use reqwest::{Client, Response, StatusCode};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use url::{Host, Url};

const DEFAULT_UA: &str = "artfetch/0.1 (multi-source illustration dataset tool)";

pub fn expand_tilde(path_str: &str) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path_str)
}

pub fn load_env_file_defaults() -> HashMap<String, String> {
    let target = std::env::var("ARTFETCH_ENV_FILE")
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|_| expand_tilde("~/.config/artfetch/credentials.env"));
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(target) else {
        return map;
    };
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(stripped) = line.strip_prefix("export ") {
            line = stripped.trim();
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let mut val = v.trim();
        if val.len() >= 2
            && ((val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\'')))
        {
            val = &val[1..val.len() - 1];
        }
        if !key.is_empty() {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

pub fn gelbooru_credentials() -> (Option<String>, Option<String>) {
    let defaults = load_env_file_defaults();
    let api_key = std::env::var("GELBOORU_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            defaults
                .get("GELBOORU_API_KEY")
                .cloned()
                .filter(|s| !s.is_empty())
        });
    let user_id = std::env::var("GELBOORU_USER_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            defaults
                .get("GELBOORU_USER_ID")
                .cloned()
                .filter(|s| !s.is_empty())
        });
    (api_key, user_id)
}

/// What one response may cost. Body sizes count decoded bytes, after any
/// `Content-Encoding` has been undone, so a small compressed reply cannot
/// expand without bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpLimits {
    pub max_text_bytes: u64,
    pub max_download_bytes: u64,
    pub max_redirects: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 32 << 20,
            max_download_bytes: 256 << 20,
            max_redirects: 5,
        }
    }
}

/// Query keys whose values never appear in a log line or an error.
const SECRET_QUERY_KEYS: [&str; 11] = [
    "api_key",
    "apikey",
    "user_id",
    "token",
    "key",
    "access_token",
    "auth",
    "password",
    "passwd",
    "secret",
    "session",
];

fn is_local_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || a == 0
        // RFC 6598 shared address space (carrier-grade NAT, overlay networks).
        || (a == 100 && (64..128).contains(&b))
}

fn is_local_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_local_v4(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        // RFC 4193 unique-local and RFC 4291 link-local ranges.
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
}

/// True for `localhost` names and for loopback, private, link-local and
/// unspecified address literals. Names are not resolved.
fn is_local_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        Some(Host::Ipv4(ip)) => is_local_v4(ip),
        Some(Host::Ipv6(ip)) => is_local_v6(ip),
        None => true,
    }
}

/// The rule for one redirect hop from `from` to `to`: http(s) only, never
/// from https down to http, and never from a public host to a local or
/// private address. Redirects between public hosts (an image host handing
/// off to its CDN) and between local hosts are allowed.
pub fn check_redirect(from: &Url, to: &Url) -> Result<(), String> {
    if !matches!(to.scheme(), "http" | "https") {
        return Err(format!("redirect to a {} URL refused", to.scheme()));
    }
    if from.scheme() == "https" && to.scheme() == "http" {
        return Err("redirect from https to http refused".to_string());
    }
    if !is_local_host(from) && is_local_host(to) {
        return Err(
            "redirect from a public host to a local or private address refused".to_string(),
        );
    }
    Ok(())
}

fn redirect_policy(max_redirects: usize) -> Policy {
    Policy::custom(move |attempt| {
        if attempt.previous().len() > max_redirects {
            return attempt.error(format!("more than {max_redirects} redirects"));
        }
        let verdict = match attempt.previous().last() {
            Some(from) => check_redirect(from, attempt.url()),
            None => Ok(()),
        };
        match verdict {
            Ok(()) => attempt.follow(),
            Err(why) => attempt.error(why),
        }
    })
}

/// A reqwest error as text without its URL, plus the chain of causes (which
/// carries the redirect refusal reasons above).
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        text.push_str(": ");
        text.push_str(&s.to_string());
        src = s.source();
    }
    text
}

enum BodyError {
    TooLarge,
    Read(reqwest::Error),
}

/// Reads a whole body, refusing it once more than `max` decoded bytes arrive.
async fn read_capped(resp: Response, max: u64) -> Result<Vec<u8>, BodyError> {
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(BodyError::TooLarge);
    }
    let mut buf = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyError::Read)?;
        if buf.len() as u64 + chunk.len() as u64 > max {
            return Err(BodyError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn mib(n: u64) -> String {
    format!("{} MiB", n.div_ceil(1 << 20))
}

#[derive(Clone)]
pub struct HttpEngine {
    client: Client,
    limits: HttpLimits,
    per_host_limit: usize,
    host_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl HttpEngine {
    pub fn new(per_host_limit: usize, timeout_secs: u64) -> Result<Self, String> {
        Self::with_limits(per_host_limit, timeout_secs, HttpLimits::default())
    }

    pub fn with_limits(
        per_host_limit: usize,
        timeout_secs: u64,
        limits: HttpLimits,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .user_agent(DEFAULT_UA)
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .redirect(redirect_policy(limits.max_redirects))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {}", describe(e)))?;
        Ok(Self {
            client,
            limits,
            per_host_limit: per_host_limit.max(1),
            host_semaphores: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn host_semaphore(&self, url: &str) -> Arc<Semaphore> {
        let host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_else(|| "default".to_string());
        let mut guard = self.host_semaphores.lock().expect("host mutex");
        guard
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_host_limit)))
            .clone()
    }

    fn should_retry(status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
            || status.is_server_error()
    }

    /// The one way a URL is written into a log line or an error: user info
    /// and the values of credential-like query keys are replaced, and a URL
    /// that does not parse is not echoed at all.
    pub fn redact_url(raw: &str) -> String {
        let Ok(mut parsed) = Url::parse(raw) else {
            return "<unparseable URL>".to_string();
        };
        if !parsed.username().is_empty() || parsed.password().is_some() {
            let _ = parsed.set_password(None);
            let _ = parsed.set_username("REDACTED");
        }
        let is_secret = |k: &str| SECRET_QUERY_KEYS.contains(&k.to_ascii_lowercase().as_str());
        if parsed.query_pairs().any(|(k, _)| is_secret(&k)) {
            let clean_pairs: Vec<(String, String)> = parsed
                .query_pairs()
                .map(|(k, v)| {
                    let v = if is_secret(&k) {
                        "REDACTED".to_string()
                    } else {
                        v.into_owned()
                    };
                    (k.into_owned(), v)
                })
                .collect();
            parsed.query_pairs_mut().clear().extend_pairs(clean_pairs);
        }
        parsed.into()
    }

    pub async fn get_text(&self, url: &str) -> Result<String, String> {
        let sem = self.host_semaphore(url);
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let safe_url = Self::redact_url(url);
        let max = self.limits.max_text_bytes;
        let mut last_err = String::new();

        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(350 * (1 << attempt))).await;
            }
            match self
                .client
                .get(url)
                .header("Accept", "application/json, text/html;q=0.9")
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return match read_capped(resp, max).await {
                            Ok(body) => Ok(String::from_utf8_lossy(&body).into_owned()),
                            Err(BodyError::TooLarge) => Err(format!(
                                "response for {safe_url} exceeds the {} limit",
                                mib(max)
                            )),
                            Err(BodyError::Read(e)) => {
                                Err(format!("body read failed for {safe_url}: {}", describe(e)))
                            }
                        };
                    }
                    last_err = format!("HTTP {} for {safe_url}", status.as_u16());
                    if !Self::should_retry(status) {
                        return Err(last_err);
                    }
                }
                Err(e) => {
                    let redirect = e.is_redirect();
                    last_err = format!("request failed for {safe_url}: {}", describe(e));
                    if redirect {
                        return Err(last_err);
                    }
                }
            }
        }
        Err(last_err)
    }

    pub async fn post_json(
        &self,
        url: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let sem = self.host_semaphore(url);
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let safe_url = Self::redact_url(url);
        let max = self.limits.max_text_bytes;
        let mut last_err = String::new();

        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * (1 << attempt))).await;
            }
            match self.client.post(url).json(payload).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return match read_capped(resp, max).await {
                            Ok(body) => serde_json::from_slice(&body)
                                .map_err(|e| format!("JSON decode failed for {safe_url}: {e}")),
                            Err(BodyError::TooLarge) => Err(format!(
                                "response for {safe_url} exceeds the {} limit",
                                mib(max)
                            )),
                            Err(BodyError::Read(e)) => {
                                Err(format!("body read failed for {safe_url}: {}", describe(e)))
                            }
                        };
                    }
                    last_err = format!("HTTP {} for {safe_url}", status.as_u16());
                    if !Self::should_retry(status) {
                        return Err(last_err);
                    }
                }
                Err(e) => {
                    let redirect = e.is_redirect();
                    last_err = format!("POST error for {safe_url}: {}", describe(e));
                    if redirect {
                        return Err(last_err);
                    }
                }
            }
        }
        Err(last_err)
    }

    pub async fn download_atomic(
        &self,
        url: &str,
        referer: Option<&str>,
        dest: &Path,
    ) -> Result<String, String> {
        if dest.is_file() {
            let bytes = tokio::fs::read(dest)
                .await
                .map_err(|e| format!("failed reading existing {}: {e}", dest.display()))?;
            if !bytes.is_empty() {
                return Ok(hex::encode(Sha256::digest(&bytes)));
            }
        }

        let sem = self.host_semaphore(url);
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let part_name = format!(
            ".{}.part",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download")
        );
        let part_path = dest.with_file_name(part_name);
        let max = self.limits.max_download_bytes;
        let too_large = |safe_url: &str| {
            format!(
                "download of {safe_url} exceeds the {} limit (--max-download-mib)",
                mib(max)
            )
        };
        let mut last_err = String::new();

        let safe_url = Self::redact_url(url);
        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
            }
            let mut req = self.client.get(url);
            if let Some(r) = referer.filter(|s| !s.is_empty()) {
                req = req.header("Referer", r);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        last_err = format!("HTTP {} downloading {}", status.as_u16(), safe_url);
                        if !Self::should_retry(status) {
                            return Err(last_err);
                        }
                        continue;
                    }
                    if resp.content_length().is_some_and(|n| n > max) {
                        return Err(too_large(&safe_url));
                    }
                    let mut file = tokio::fs::File::create(&part_path)
                        .await
                        .map_err(|e| format!("create {} failed: {e}", part_path.display()))?;
                    let mut hasher = Sha256::new();
                    let mut written = 0u64;
                    let mut stream = resp.bytes_stream();
                    let mut stream_ok = true;
                    while let Some(chunk_res) = stream.next().await {
                        match chunk_res {
                            Ok(chunk) => {
                                written += chunk.len() as u64;
                                if written > max {
                                    drop(file);
                                    let _ = tokio::fs::remove_file(&part_path).await;
                                    return Err(too_large(&safe_url));
                                }
                                hasher.update(&chunk);
                                if let Err(e) = file.write_all(&chunk).await {
                                    last_err = format!("write failed: {e}");
                                    stream_ok = false;
                                    break;
                                }
                            }
                            Err(e) => {
                                last_err = format!("stream error on {safe_url}: {}", describe(e));
                                stream_ok = false;
                                break;
                            }
                        }
                    }
                    if stream_ok && file.flush().await.is_ok() {
                        drop(file);
                        tokio::fs::rename(&part_path, dest)
                            .await
                            .map_err(|e| format!("atomic rename failed: {e}"))?;
                        return Ok(hex::encode(hasher.finalize()));
                    }
                }
                Err(e) => {
                    let redirect = e.is_redirect();
                    last_err = format!("download request failed for {safe_url}: {}", describe(e));
                    if redirect {
                        return Err(last_err);
                    }
                }
            }
        }
        Err(last_err)
    }
}
