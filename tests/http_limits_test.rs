//! Response bodies are capped by their decoded size and redirects are bounded.
//! Every network peer is a loopback server started by the test.

mod common;

use artfetch::http::{HttpEngine, HttpLimits, check_redirect};
use common::{FakeServer, Reply, gzip_zeros};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::Url;

const MIB: usize = 1 << 20;

#[tokio::test]
async fn get_text_refuses_a_body_that_inflates_past_the_default_limit() {
    // About 260 KB on the wire, 40 MiB once decoded.
    let srv = FakeServer::start(vec![(
        "/bomb.json",
        Reply::ok(gzip_zeros(40 * MIB)).header("Content-Encoding", "gzip"),
    )]);
    let eng = HttpEngine::new(2, 30).unwrap();
    let res = eng.get_text(&srv.url("/bomb.json")).await;
    let err = res
        .map(|body| format!("accepted {} decoded bytes", body.len()))
        .unwrap_err();
    assert!(err.contains("limit"), "{err}");
}

#[tokio::test]
async fn download_refuses_a_body_that_inflates_past_the_download_limit() {
    let srv = FakeServer::start(vec![
        (
            "/bomb.jpg",
            Reply::ok(gzip_zeros(8 * MIB)).header("Content-Encoding", "gzip"),
        ),
        ("/big.jpg", Reply::ok(vec![7u8; 2 * MIB])),
        ("/small.jpg", Reply::ok(vec![7u8; 1000])),
    ]);
    let limits = HttpLimits {
        max_download_bytes: MIB as u64,
        ..HttpLimits::default()
    };
    let eng = HttpEngine::with_limits(2, 30, limits).unwrap();
    let dir = tempfile::tempdir().unwrap();

    for name in ["bomb.jpg", "big.jpg"] {
        let dest = dir.path().join(name);
        let err = eng
            .download_atomic(&srv.url(&format!("/{name}")), None, &dest)
            .await
            .unwrap_err();
        assert!(err.contains("limit"), "{name}: {err}");
        assert!(!dest.exists(), "{name} was kept");
    }
    // Nothing is left behind, not even a partial file.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    // A refused body is not fetched again.
    assert_eq!(
        srv.hits()
            .iter()
            .filter(|h| h.as_str() == "/bomb.jpg")
            .count(),
        1
    );

    let dest = dir.path().join("small.jpg");
    eng.download_atomic(&srv.url("/small.jpg"), None, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), 1000);
}

#[tokio::test]
async fn redirect_chains_are_capped() {
    let mut routes: Vec<(String, Reply)> = (0..8)
        .map(|i| (format!("/r{i}"), Reply::redirect(&format!("/r{}", i + 1))))
        .collect();
    routes.push(("/r8".to_string(), Reply::ok(b"{}".to_vec())));
    let srv = FakeServer::start(
        routes
            .iter()
            .map(|(p, r)| (p.as_str(), r.clone()))
            .collect(),
    );
    let eng = HttpEngine::new(2, 30).unwrap();

    let res = eng.get_text(&srv.url("/r0")).await;
    assert!(
        res.is_err(),
        "an 8-hop redirect chain was followed: {res:?}"
    );
    assert!(!srv.hits().contains(&"/r8".to_string()), "{:?}", srv.hits());
}

#[tokio::test]
async fn a_short_redirect_on_the_same_host_still_works() {
    let srv = FakeServer::start(vec![
        ("/a.jpg", Reply::redirect("/b.jpg")),
        ("/b.jpg", Reply::ok(vec![1u8; 10])),
    ]);
    let eng = HttpEngine::new(2, 30).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.jpg");
    eng.download_atomic(&srv.url("/a.jpg"), None, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), vec![1u8; 10]);
}

fn u(s: &str) -> Url {
    Url::parse(s).unwrap()
}

/// `http://<addr>/x` for a typed address, so the test names address ranges
/// rather than spelling out literals.
fn at(ip: impl Into<IpAddr>) -> String {
    match ip.into() {
        IpAddr::V4(v4) => format!("http://{v4}/x"),
        IpAddr::V6(v6) => format!("http://[{v6}]/x"),
    }
}

#[test]
fn redirect_rules() {
    let public = "http://img.example.invalid/a".to_string();
    let lan = Ipv4Addr::new(192, 168, 0, 9);
    // Image hosts hand off to other CDN hosts all the time.
    let allowed = [
        (
            "https://img.example.invalid/a.jpg".to_string(),
            "https://cdn.example.invalid/x/a.jpg".to_string(),
        ),
        (public.clone(), "https://img.example.invalid/a".to_string()),
        (
            "http://127.0.0.1:8080/a".to_string(),
            "http://127.0.0.1:8080/b".to_string(),
        ),
        ("http://localhost:8080/a".to_string(), at(lan)),
    ];
    for (from, to) in &allowed {
        assert!(check_redirect(&u(from), &u(to)).is_ok(), "{from} -> {to}");
    }
    let mut refused = vec![
        (
            "https://img.example.invalid/a".to_string(),
            "http://img.example.invalid/a".to_string(),
        ),
        (
            "https://img.example.invalid/a".to_string(),
            "http://127.0.0.1:8188/x".to_string(),
        ),
        (public.clone(), "http://localhost/x".to_string()),
        (public.clone(), "http://app.localhost/x".to_string()),
        (public.clone(), "http://[::ffff:127.0.0.1]/x".to_string()),
        (public.clone(), "ftp://img.example.invalid/x".to_string()),
    ];
    let local_v4 = [
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::new(10, 1, 2, 3),
        Ipv4Addr::new(172, 16, 0, 1),
        lan,
        Ipv4Addr::new(169, 254, 169, 254),
        Ipv4Addr::new(100, 64, 0, 1),
    ];
    let local_v6 = [
        Ipv6Addr::LOCALHOST,
        Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
        Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
    ];
    refused.extend(local_v4.into_iter().map(|ip| (public.clone(), at(ip))));
    refused.extend(local_v6.into_iter().map(|ip| (public.clone(), at(ip))));
    for (from, to) in &refused {
        assert!(check_redirect(&u(from), &u(to)).is_err(), "{from} -> {to}");
    }
}

/// The whole path through `fetch --run`: a candidate on a public host whose
/// answer redirects into a loopback service. The public host is reached through
/// a loopback proxy, so nothing leaves the machine.
#[cfg(unix)]
#[test]
fn fetch_does_not_follow_a_public_redirect_into_a_local_address() {
    let internal = FakeServer::start(vec![("/secret", Reply::ok(b"INTERNAL".to_vec()))]);
    let public_img = "http://img.example.invalid/a.jpg";
    let proxy = FakeServer::start(vec![(
        public_img,
        Reply::redirect(&internal.url("/secret")),
    )]);

    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let payload = format!(
        r#"{{"data":[{{"id":"1","media":[{{"type":"photo","url":"{public_img}","width":2000,"height":2000}}]}}]}}"#
    );
    std::fs::write(root.path().join("tw.json"), payload).unwrap();
    common::write_stub(
        &bin,
        "twitter",
        &format!("cat '{}'", root.path().join("tw.json").display()),
    );
    let out_dir = root.path().join("out");
    let out = common::artfetch_cmd(&bin)
        .args(["fetch", "--source", "twitter", "--target", "demo"])
        .args(["--allow-mature", "--run", "--out"])
        .arg(&out_dir)
        .env("http_proxy", proxy.url(""))
        .env("HTTP_PROXY", proxy.url(""))
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .output()
        .unwrap();
    let summary = common::stdout_json(&out);
    assert_eq!(proxy.hits(), vec![public_img.to_string()], "{summary}");
    assert!(
        internal.hits().is_empty(),
        "followed into loopback: {summary}"
    );
    assert_eq!(summary["downloaded"], 0, "{summary}");
    assert_eq!(summary["failed_downloads"], 1, "{summary}");
    let err = summary["download_errors"][0].as_str().unwrap_or_default();
    assert!(err.contains("local or private"), "{summary}");
}

#[cfg(unix)]
#[test]
fn fetch_max_download_mib_caps_each_file() {
    let srv = FakeServer::start(vec![
        ("/big.jpg", Reply::ok(vec![7u8; 2 * MIB])),
        ("/small.jpg", Reply::ok(vec![7u8; 1000])),
    ]);
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (big, small) = (srv.url("/big.jpg"), srv.url("/small.jpg"));
    let payload = format!(
        r#"{{"data":[
          {{"id":"1","media":[{{"type":"photo","url":"{big}","width":2000,"height":2000}}]}},
          {{"id":"2","media":[{{"type":"photo","url":"{small}","width":2000,"height":2000}}]}}
        ]}}"#
    );
    std::fs::write(root.path().join("tw.json"), payload).unwrap();
    common::write_stub(
        &bin,
        "twitter",
        &format!("cat '{}'", root.path().join("tw.json").display()),
    );
    let out_dir = root.path().join("out");
    let out = common::artfetch_cmd(&bin)
        .args(["fetch", "--source", "twitter", "--target", "demo"])
        .args([
            "--allow-mature",
            "--run",
            "--max-download-mib",
            "1",
            "--out",
        ])
        .arg(&out_dir)
        .output()
        .unwrap();
    let summary = common::stdout_json(&out);
    assert_eq!(summary["downloaded"], 1, "{summary}");
    assert_eq!(summary["failed_downloads"], 1, "{summary}");
    assert!(out_dir.join("twitter_2_0.jpg").is_file(), "{summary}");
    assert!(!out_dir.join("twitter_1_0.jpg").exists(), "{summary}");
}
