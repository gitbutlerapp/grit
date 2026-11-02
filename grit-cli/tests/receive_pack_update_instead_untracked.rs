//! `receive.denyCurrentBranch=updateInstead` must refuse a push that would
//! overwrite an untracked file, matching system `git receive-pack`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_cmd(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args);
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    git(dir, &["rev-parse", rev]).trim().to_owned()
}

#[test]
fn update_instead_refuses_untracked_collision_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = tmp.path().join("srv");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "-b", "main", "."]);
    std::fs::write(srv.join("a"), "a\n").unwrap();
    git(&srv, &["add", "a"]);
    git(&srv, &["commit", "-q", "-m", "a"]);
    git(
        &srv,
        &["config", "receive.denyCurrentBranch", "updateInstead"],
    );

    let work = tmp.path().join("work");
    assert!(Command::new("git")
        .current_dir(tmp.path())
        .args(["clone", "-q", "srv", "work"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap()
        .success());
    std::fs::write(work.join("collision"), "incoming\n").unwrap();
    git(&work, &["add", "collision"]);
    git(&work, &["commit", "-q", "-m", "add collision"]);
    let tip = rev_parse(&work, "HEAD");

    std::fs::write(srv.join("collision"), "keep me\n").unwrap();
    let main_before = rev_parse(&srv, "refs/heads/main");

    let receive_pack = format!("--receive-pack={GRIT} receive-pack");
    let push = git_cmd(&work, &["push", &receive_pack, "../srv", "HEAD:main"]);
    assert!(
        !push.status.success(),
        "push should fail: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        combined.contains("Could not update working tree to new HEAD"),
        "expected checkout refusal in push output, got: {combined}"
    );
    assert_eq!(
        rev_parse(&srv, "refs/heads/main"),
        main_before,
        "ref must not move"
    );
    assert_eq!(
        std::fs::read_to_string(srv.join("collision")).unwrap(),
        "keep me\n"
    );
    assert_ne!(tip, main_before);
}
