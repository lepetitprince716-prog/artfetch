//! Error messages must not carry credentials from request URLs. Every value is synthetic and every network peer is a
//! loopback server or stub started by the test.

mod common;

use artfetch::http::HttpEngine;
use common::{FakeServer, Reply};
fn synthetic_secret() -> String {
    // Built at run time so no literal credential sits in the source.
    ["zq", "x7", "fake", "cred", "9314"].concat()
}

#[tokio::test]
async fn error_messages_never_carry_credentials() {
    let secret = synthetic_secret();
    let srv = FakeServer::start(vec![
        (
            "/trunc.jpg",
            Reply {
                truncate_to: Some(5),
                ..Reply::ok(vec![b'x'; 1000])
            },
        ),
        (
            "/trunc.json",
            Reply {
                truncate_to: Some(5),
                ..Reply::ok(vec![b'x'; 1000])
            },
        ),
    ]);
    let eng = HttpEngine::new(2, 5).unwrap();
    let dir = tempfile::tempdir().unwrap();

    let dead_q = format!("http://127.0.0.1:1/dead.jpg?api_key={secret}&user_id=4242");
    let dead_userinfo = format!("http://someone:{secret}@127.0.0.1:1/dead.jpg");
    let trunc_dl = format!("{}?api_key={secret}", srv.url("/trunc.jpg"));
    let trunc_txt = format!("{}?api_key={secret}", srv.url("/trunc.json"));
    let d1 = dir.path().join("d1.jpg");
    let d2 = dir.path().join("d2.jpg");
    let d3 = dir.path().join("d3.jpg");
    let body = serde_json::json!({"k": 1});

    let (a, b, c, d, e, f) = tokio::join!(
        eng.download_atomic(&dead_q, None, &d1),
        eng.download_atomic(&dead_userinfo, None, &d2),
        eng.download_atomic(&trunc_dl, None, &d3),
        eng.post_json(&dead_q, &body),
        eng.get_text(&dead_userinfo),
        eng.get_text(&trunc_txt),
    );
    let errors = [
        a.unwrap_err(),
        b.unwrap_err(),
        c.unwrap_err(),
        d.unwrap_err(),
        e.unwrap_err(),
        f.unwrap_err(),
    ];
    for msg in &errors {
        assert!(!msg.contains(&secret), "credential leaked: {msg}");
        assert!(!msg.contains("4242"), "user id leaked: {msg}");
    }
    let red = HttpEngine::redact_url(&dead_userinfo);
    assert!(!red.contains(&secret), "{red}");
    assert!(red.contains("127.0.0.1:1/dead.jpg"), "{red}");
}
