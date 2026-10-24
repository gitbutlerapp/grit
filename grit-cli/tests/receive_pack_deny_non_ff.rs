//! System `git push --receive-pack='grit receive-pack'` into a repository with
//! `receive.denyNonFastForwards=true`: fast-forwards land, rewrites are refused.

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
fn git_push_through_grit_receive_pack_honors_deny_non_fast_forwards() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = tmp.path().join("srv.git");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&srv, &["config", "receive.denyNonFastForwards", "true"]);

    let w = tmp.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    git(&w, &["init", "-q", "-b", "main", "."]);
    std::fs::write(w.join("a"), "1\n").unwrap();
    git(&w, &["add", "a"]);
    git(&w, &["commit", "-qm", "one"]);

    let receive_pack = format!("--receive-pack={GRIT} receive-pack");
    let srv_arg = srv.to_str().unwrap();
    git(&w, &["push", "-q", &receive_pack, srv_arg, "main"]);

    std::fs::write(w.join("a"), "1\n2\n").unwrap();
    git(&w, &["commit", "-qam", "two"]);
    git(&w, &["push", "-q", &receive_pack, srv_arg, "main"]);
    assert_eq!(rev_parse(&srv, "main"), rev_parse(&w, "main"));
    git(&srv, &["fsck", "--strict"]);

    let advanced = rev_parse(&srv, "main");
    git(&w, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(w.join("b"), "rewrite\n").unwrap();
    git(&w, &["add", "b"]);
    git(&w, &["commit", "-qm", "rewrite"]);
    let forced = git_cmd(&w, &["push", "--force", &receive_pack, srv_arg, "main"]);
    assert!(!forced.status.success(), "forced rewrite must be refused");
    assert!(
        String::from_utf8_lossy(&forced.stderr).contains("non-fast-forward"),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert_eq!(rev_parse(&srv, "main"), advanced);
}
