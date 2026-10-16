//! `grit add --executable` / `--non-executable` with `core.filemode=false` (issue #1043).

use std::fs;
use std::process::Command;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit")
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn grit_add_executable_flags_with_filemode_false() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(
        root.join(".git").join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n[user]\n\tname = Test\n\temail = t@example.com\n",
    )
    .expect("config");

    fs::write(root.join("tool.sh"), "#!/bin/sh\necho hi\n").expect("write");
    assert!(run_grit(grit, root, &["add", "--executable", "tool.sh"])
        .status
        .success());
    let staged = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(
        staged.starts_with("100755 "),
        "index after --executable: {staged}"
    );

    assert!(run_grit(grit, root, &["commit", "-m", "script"])
        .status
        .success());
    let tree = git_out(root, &["ls-tree", "HEAD", "tool.sh"]);
    assert!(tree.starts_with("100755 blob"), "tree after commit: {tree}");

    assert!(
        run_grit(grit, root, &["add", "--non-executable", "tool.sh"])
            .status
            .success()
    );
    let cleared = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(
        cleared.starts_with("100644 "),
        "index after --non-executable: {cleared}"
    );

    assert!(run_grit(grit, root, &["commit", "-m", "non-exec"])
        .status
        .success());
    let tree2 = git_out(root, &["ls-tree", "HEAD", "tool.sh"]);
    assert!(
        tree2.starts_with("100644 blob"),
        "tree after clearing executable: {tree2}"
    );
}
