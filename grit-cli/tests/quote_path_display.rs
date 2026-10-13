//! Control characters in paths must not reach stdout raw (terminal escape injection).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn init_repo(dir: &std::path::Path) {
    assert!(Command::new(GRIT)
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("grit init")
        .success());
}

#[test]
fn status_and_diff_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());

    let evil = "evil\x1b[31mRED\x1b[0m.txt";
    let bell = "bell\x07.txt";
    let ctl = "ctl\x01char.txt";
    std::fs::File::create(dir.path().join(evil)).unwrap();
    std::fs::File::create(dir.path().join(bell)).unwrap();
    std::fs::File::create(dir.path().join(ctl)).unwrap();

    for args in [&["status"][..], &["status", "--markdown"][..]] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit status");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = out.stdout;
        assert!(
            !stdout.iter().any(|&b| b == 0x1b),
            "raw ESC in status output for {args:?}"
        );
        let text = String::from_utf8_lossy(&stdout);
        assert!(
            text.contains("\\033"),
            "expected C-style ESC quoting in {args:?}:\n{text}"
        );
        assert!(
            text.contains("\\a"),
            "expected BEL quoting in {args:?}:\n{text}"
        );
        assert!(
            text.contains("\\001"),
            "expected control-byte quoting in {args:?}:\n{text}"
        );
    }

    let git_short = Command::new("git")
        .args(["status", "--short"])
        .current_dir(dir.path())
        .output()
        .expect("git status");
    assert!(git_short.status.success());
    let git_line = String::from_utf8_lossy(&git_short.stdout);
    assert!(git_line.contains("\\033"));

    assert!(Command::new(GRIT)
        .args(["add", "."])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "funny names"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    std::fs::write(dir.path().join(evil), b"touch\n").unwrap();
    let diff_out = Command::new(GRIT)
        .args(["diff", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("grit diff");
    assert!(diff_out.status.success());
    assert!(
        !diff_out.stdout.iter().any(|&b| b == 0x1b),
        "raw ESC in diff output"
    );
}
