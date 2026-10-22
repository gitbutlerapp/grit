//! `grit add --json` with FIFOs in the work tree (issue #1400).

#[cfg(unix)]
#[test]
fn add_json_staged_count_with_fifo_present() {
    use std::fs;
    use std::process::Command;

    let grit = env!("CARGO_BIN_EXE_grit");
    let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", null_config)
            .env("GIT_CONFIG_SYSTEM", null_config)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run_git(&["init", "-q", "-b", "main"]);
    run_git(&["config", "user.email", "t@example.com"]);
    run_git(&["config", "user.name", "Test"]);
    fs::write(root.join("ok.txt"), "ok\n").expect("write ok");
    run_git(&["add", "ok.txt"]);
    run_git(&["commit", "-qm", "base"]);

    assert!(Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .expect("mkfifo")
        .success());
    fs::write(root.join("new.txt"), "new\n").expect("write new");

    let out = Command::new(grit)
        .current_dir(root)
        .args(["add", "--json"])
        .env("GIT_CONFIG_GLOBAL", null_config)
        .env("GIT_CONFIG_SYSTEM", null_config)
        .output()
        .expect("grit add");
    assert!(
        out.status.success(),
        "grit add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(
        v.get("staged")
            .and_then(|n| n.as_u64())
            .expect("add --json must expose staged count"),
        1,
        "only new.txt is staged when a fifo is present: {v}"
    );
}
