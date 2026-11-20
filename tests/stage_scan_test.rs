//! The stage scan must terminate on link cycles and stay inside its input trees. Every value is synthetic and every network peer is a
//! loopback server or stub started by the test.

mod common;

use artfetch::stage::execute_stage;
use common::{stage_cfg, write_png};
use std::time::Duration;
#[cfg(unix)]
#[test]
fn stage_scan_survives_symlink_cycles() {
    let root = tempfile::tempdir().unwrap();
    let in_dir = root.path().join("in");
    std::fs::create_dir_all(&in_dir).unwrap();
    write_png(&in_dir.join("a.png"), 64, 48, [10, 200, 10]);
    std::os::unix::fs::symlink(".", in_dir.join("loop_a")).unwrap();
    std::os::unix::fs::symlink(".", in_dir.join("loop_b")).unwrap();
    let out_dir = root.path().join("out");

    let cfg = stage_cfg(&in_dir, &out_dir, "img", false);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(execute_stage(&cfg));
    });
    let res = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("stage scan did not finish within 20 s on a two-link cycle")
        .unwrap();
    assert_eq!(res["scanned_files"], 1, "{res}");
}

#[cfg(unix)]
#[test]
fn stage_scan_does_not_read_images_outside_the_in_dir() {
    let root = tempfile::tempdir().unwrap();
    let in_dir = root.path().join("in");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::create_dir_all(outside.join("deep")).unwrap();
    write_png(&in_dir.join("real.png"), 64, 48, [10, 10, 200]);
    write_png(&outside.join("secret.png"), 64, 48, [250, 250, 0]);
    write_png(&outside.join("deep/other.png"), 64, 48, [0, 250, 250]);
    std::os::unix::fs::symlink(outside.join("secret.png"), in_dir.join("link.png")).unwrap();
    std::os::unix::fs::symlink(&outside, in_dir.join("linked_dir")).unwrap();
    // A link that stays inside the input tree is still followed.
    std::fs::create_dir_all(in_dir.join("sub")).unwrap();
    write_png(&in_dir.join("sub/inner.png"), 64, 48, [120, 0, 120]);
    std::os::unix::fs::symlink(in_dir.join("sub/inner.png"), in_dir.join("alias.png")).unwrap();

    let res = execute_stage(&stage_cfg(&in_dir, &root.path().join("out"), "img", false)).unwrap();
    // real.png and sub/inner.png (reached directly and through alias.png).
    assert_eq!(res["scanned_files"], 2, "{res}");
}
