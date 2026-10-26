//! Linked-worktree commit surfaces git-dir path I/O errors in JSON (issue #1970).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static OsStr {
    OsStr::new("/dev/null")
}

fn grit_json_fail(repo: &Path, args: &[&str]) -> (i32, Value) {
    let mut cmd_args: Vec<&str> = args.to_vec();
    if !cmd_args.contains(&"--json") {
        cmd_args.push("--json");
    }
    let out = Command::new(GRIT)
        .args(&cmd_args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("grit");
    let code = out.status.code().unwrap_or(-1);
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "json parse ({e}): stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (code, value)
}

fn chmod_dir_readonly(path: &Path) {
    let meta = fs::metadata(path).expect("metadata");
    let mut perms = meta.permissions();
    perms.set_mode(meta.permissions().mode() & !0o222);
    fs::set_permissions(path, perms).expect("chmod");
}

#[test]
fn linked_worktree_commit_cli_json_names_git_dir_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");

    let init = Command::new(GRIT)
        .args(["init"])
        .current_dir(&main)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("init");
    assert!(init.status.success());

    fs::write(main.join("a"), "base\n").expect("write");
    assert!(Command::new(GRIT)
        .args(["add", "a"])
        .current_dir(&main)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("add")
        .success());
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "base"])
        .current_dir(&main)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .expect("commit")
        .success());

    let linked = tmp.path().join("linked");
    fs::create_dir_all(&linked).expect("mkdir linked parent");
    assert!(Command::new(GRIT)
        .args([
            "worktree",
            "add",
            linked.to_str().expect("utf8"),
            "--detach"
        ])
        .current_dir(&main)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("worktree add")
        .success());

    fs::write(linked.join("a"), "change\n").expect("write");
    assert!(Command::new(GRIT)
        .args(["add", "a"])
        .current_dir(&linked)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("stage")
        .success());

    let common = fs::canonicalize(main.join(".git")).expect("common git dir");
    let wt_name = fs::read_dir(common.join("worktrees"))
        .expect("worktrees")
        .next()
        .expect("one worktree")
        .expect("entry")
        .file_name();
    chmod_dir_readonly(&common.join("worktrees").join(wt_name));

    let (code, json) = grit_json_fail(&linked, &["commit", "--staged", "-m", "linked"]);
    assert_eq!(code, 1);
    assert_eq!(json["kind"].as_str(), Some("git_dir_path_io"));
    let path = json["paths"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .expect("paths");
    assert!(
        path.contains(".git/worktrees/"),
        "expected linked worktree path, got {path}"
    );
    let msg = json["error"].as_str().expect("error message");
    assert!(
        !msg.starts_with("I/O error:"),
        "expected contextual error, got {msg}"
    );
}
