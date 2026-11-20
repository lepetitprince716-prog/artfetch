//! Non-ASCII text from sources and the command line must not panic on byte slicing. Every value is synthetic and every network peer is a
//! loopback server or stub started by the test.

mod common;

use common::{artfetch_cmd_offline, stdout_json};
#[cfg(unix)]
#[test]
fn non_ascii_dates_from_gallery_dl_do_not_crash_probe() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let date = "2026\u{5e74}01\u{6708}01\u{65e5}";
    let pixiv = format!(
        r#"[[3, "https://img.example.invalid/a.jpg", {{"id": 1, "num": 0, "create_date": "{date}", "width": 2000, "height": 2000}}]]"#
    );
    let artstation = format!(
        r#"[[3, "https://img.example.invalid/b.jpg", {{"hash_id": "abc", "created_at": "{date}", "asset": {{"id": 7, "asset_type": "image", "has_image": true, "width": 2000, "height": 2000}}}}]]"#
    );
    let pix_file = root.path().join("pixiv.json");
    let art_file = root.path().join("art.json");
    std::fs::write(&pix_file, pixiv).unwrap();
    std::fs::write(&art_file, artstation).unwrap();
    common::write_stub(
        &bin,
        "gallery-dl",
        &format!(
            "case \"$*\" in *artstation*) cat '{}';; *) cat '{}';; esac",
            art_file.display(),
            pix_file.display()
        ),
    );
    for (source, target) in [("pixiv", "12345"), ("artstation", "someone")] {
        let out = artfetch_cmd_offline(&bin)
            .args(["probe", "--source", source, "--target", target])
            .args(["--since", "2026-01-01"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked"), "{source}: {stderr}");
        assert!(out.status.success(), "{source}: {stderr}");
        assert_eq!(stdout_json(&out)["count"], 1, "{source}");
    }
}

#[test]
fn resolve_with_a_non_ascii_name_does_not_crash() {
    let root = tempfile::tempdir().unwrap();
    // Four characters (twelve bytes), and six characters, which also takes the
    // "drop the first character" search path.
    for name in [
        "\u{7d75}\u{5e2b}\u{540d}\u{524d}",
        "\u{7d75}\u{5e2b}\u{540d}\u{524d}\u{7d75}\u{5e2b}",
    ] {
        let out = artfetch_cmd_offline(root.path())
            .args(["resolve", name])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked"), "{name}: {stderr}");
        assert!(out.status.success(), "{name}: {stderr}");
    }
}
