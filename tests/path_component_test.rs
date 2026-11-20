//! Hostile path components (`--prefix`, remote ids) must never leave the output directory. Every value is synthetic and every network peer is a
//! loopback server or stub started by the test.

mod common;

use artfetch::stage::execute_stage;
use common::{FakeServer, Reply, artfetch_cmd, files_under, stage_cfg, stdout_json, write_png};
use std::path::{Path, PathBuf};
#[test]
fn stage_refuses_a_prefix_that_is_not_one_plain_file_name() {
    let root = tempfile::tempdir().unwrap();
    let in_dir = root.path().join("in");
    std::fs::create_dir_all(&in_dir).unwrap();
    write_png(&in_dir.join("a.png"), 64, 48, [200, 10, 10]);

    let absolute = root.path().join("abs_victim").display().to_string();
    let bad = [
        "../escaped",
        "../../escaped",
        "sub/name",
        absolute.as_str(),
        "",
        "..",
        ".",
        "a\\b",
        "x..y",
    ];
    for (i, prefix) in bad.iter().enumerate() {
        let out_dir = root.path().join(format!("out{i}"));
        let res = execute_stage(&stage_cfg(&in_dir, &out_dir, prefix, true));
        assert!(res.is_err(), "prefix {prefix:?} was accepted: {res:?}");
    }
    // Nothing was written anywhere but the input image.
    assert_eq!(files_under(root.path()), vec![in_dir.join("a.png")]);

    // A plain prefix still works.
    let ok_out = root.path().join("ok");
    execute_stage(&stage_cfg(&in_dir, &ok_out, "exa26", true)).unwrap();
    assert!(ok_out.join("exa26_001.jpg").is_file());
}

#[cfg(unix)]
#[test]
fn fetch_refuses_a_remote_id_that_would_leave_the_out_dir() {
    let srv = FakeServer::start(vec![("/img.jpg", Reply::ok(b"IMAGE_BYTES".to_vec()))]);
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let img = srv.url("/img.jpg");
    let payload = format!(
        r#"{{"data":[
          {{"id":"sub/../../escaped","media":[{{"type":"photo","url":"{img}","width":2000,"height":2000}}]}},
          {{"id":"/abs","media":[{{"type":"photo","url":"{img}","width":2000,"height":2000}}]}},
          {{"id":"1234567890","media":[{{"type":"photo","url":"{img}","width":2000,"height":2000}}]}}
        ]}}"#
    );
    std::fs::write(root.path().join("tw.json"), payload).unwrap();
    common::write_stub(
        &bin,
        "twitter",
        &format!("cat '{}'", root.path().join("tw.json").display()),
    );
    let out_dir = root.path().join("out");
    // The pre-existing directory the traversal needs on its way out.
    std::fs::create_dir_all(out_dir.join("twitter_sub")).unwrap();

    let out = artfetch_cmd(&bin)
        .args(["fetch", "--source", "twitter", "--target", "demo"])
        .args(["--allow-mature", "--run", "--out"])
        .arg(&out_dir)
        .output()
        .unwrap();
    let summary = stdout_json(&out);
    assert!(out.status.success(), "{summary}");

    assert!(!root.path().join("escaped_0.jpg").exists());
    assert!(!Path::new("/abs_0.jpg").exists());
    let written: Vec<PathBuf> = files_under(root.path())
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .collect();
    assert_eq!(written, vec![out_dir.join("twitter_1234567890_0.jpg")]);
    assert_eq!(summary["downloaded"], 1, "{summary}");
    assert_eq!(summary["failed_downloads"], 2, "{summary}");
}
