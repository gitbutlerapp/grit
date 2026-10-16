//! CLI integration: ambiguous ref warnings appear on stderr.

use std::process::Command;
use tempfile::TempDir;

fn grit(args: &[&str], cwd: &TempDir) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(args)
        .current_dir(cwd.path())
        .output()
        .expect("run grit");
    let code = out.status.code().unwrap_or(-1);
    (
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(repo: &TempDir, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(repo.path())
        .status()
        .expect("git")
        .success());
}

#[test]
fn show_emits_ambiguous_ref_warning_on_stderr() {
    let tmp = TempDir::new().expect("tempdir");
    git(&tmp, &["init"]);
    git(&tmp, &["config", "user.email", "a@b.c"]);
    git(&tmp, &["config", "user.name", "A"]);
    std::fs::write(tmp.path().join("f"), "x").unwrap();
    git(&tmp, &["add", "f"]);
    git(&tmp, &["commit", "-m", "c1"]);
    std::fs::write(tmp.path().join("g"), "y").unwrap();
    git(&tmp, &["add", "g"]);
    git(&tmp, &["commit", "-m", "c2"]);
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .expect("rev-parse");
    let head_hex = String::from_utf8(head.stdout).unwrap();
    let prefix = head_hex.trim()[..7].to_string();
    let first = Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(tmp.path())
        .output()
        .expect("parent");
    let first_hex = String::from_utf8(first.stdout).unwrap();
    git(&tmp, &["branch", "-f", &prefix, first_hex.trim()]);

    let (_code, _stdout, stderr) = grit(&["show", &prefix], &tmp);
    assert!(
        stderr.contains("matches more than one"),
        "expected ambiguous ref warning on stderr, got:\n{stderr}"
    );
}
