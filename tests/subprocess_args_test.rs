//! Values handed to external CLIs must not turn into options or foreign URLs. Every value is synthetic and every network peer is a
//! loopback server or stub started by the test.

mod common;

use common::{artfetch_cmd, artfetch_cmd_offline};
use std::path::Path;
#[cfg(unix)]
fn argv_logging_stub(bin: &Path, name: &str, log: &Path, reply: &str) {
    common::write_stub(
        bin,
        name,
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\necho '{reply}'",
            log.display()
        ),
    );
}

#[cfg(unix)]
#[test]
fn twitter_handle_and_since_cannot_become_cli_options() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = root.path().join("argv.log");
    argv_logging_stub(&bin, "twitter", &log, r#"{"data":[]}"#);

    for args in [
        vec!["--target", "@--injected-flag"],
        vec!["--target", "-x"],
        vec!["--target", "demo", "--since=--evil-flag"],
    ] {
        let out = artfetch_cmd(&bin)
            .args(["probe", "--source", "twitter"])
            .args(&args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?} was accepted");
        assert!(
            !log.exists(),
            "{args:?} reached the twitter CLI: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }

    let out = artfetch_cmd(&bin)
        .args(["probe", "--source", "twitter", "--target", "@good_handle"])
        .args(["--since", "2026-01-01"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(argv.lines().any(|l| l == "good_handle"), "{argv}");
}

#[cfg(unix)]
#[test]
fn ehentai_probe_only_hands_e_hentai_urls_to_gallery_dl() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = root.path().join("argv.log");
    argv_logging_stub(&bin, "gallery-dl", &log, "[]");

    for target in [
        "http://127.0.0.1:8188/internal",
        "https://example.invalid/g/1/abc/",
        "https://e-hentai.org.example.invalid/x",
    ] {
        let out = artfetch_cmd_offline(&bin)
            .args([
                "probe",
                "--source",
                "ehentai",
                "--allow-mature",
                "--target",
                target,
            ])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{target} was accepted");
        assert!(!log.exists(), "{target} reached gallery-dl");
    }

    let out = artfetch_cmd_offline(&bin)
        .args(["probe", "--source", "ehentai", "--allow-mature"])
        .args(["--target", "https://e-hentai.org/tag/artist:someone"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("e-hentai.org")
    );
}
