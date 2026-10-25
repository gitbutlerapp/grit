//! System `git push --receive-pack='grit receive-pack'` into a repository with
//! `receive.fsckObjects` / `transfer.fsckObjects` (issue #1818): a malformed
//! commit is refused like `git receive-pack` refuses it, no ref is created, and
//! `git fsck` on the server stays clean; clean pushes still land.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    cmd
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
    git_cmd(dir, args).output().expect("git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
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

fn bad_commit(w: &Path) -> String {
    let body = format!(
        "tree {}\nparent {}\nauthor Bad Guy no-email 1700000000 +0000\ncommitter Bad <b@c> 1700000000 +0000\n\nbad\n",
        rev_parse(w, "HEAD^{tree}"),
        rev_parse(w, "HEAD")
    );
    let mut child = git_cmd(
        w,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "--stdin",
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .expect("hash-object");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn has_ref(repo: &Path, refname: &str) -> bool {
    git_out(repo, &["rev-parse", "-q", "--verify", refname])
        .status
        .success()
}

#[test]
fn git_push_of_malformed_commit_is_refused_like_git_receive_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    git(&w, &["init", "-q", "-b", "main", "."]);
    std::fs::write(w.join("f"), "x\n").unwrap();
    git(&w, &["add", "."]);
    git(&w, &["commit", "-qm", "base"]);
    let bad = bad_commit(&w);
    let refspec = format!("{bad}:refs/heads/bad");

    for (label, receive_pack, key) in [
        ("git", "git-receive-pack".to_owned(), "receive.fsckObjects"),
        (
            "grit",
            format!("{GRIT} receive-pack"),
            "receive.fsckObjects",
        ),
        (
            "grit-transfer",
            format!("{GRIT} receive-pack"),
            "transfer.fsckObjects",
        ),
    ] {
        let srv = tmp.path().join(format!("{label}.git"));
        std::fs::create_dir_all(&srv).unwrap();
        git(&srv, &["init", "-q", "--bare", "-b", "main", "."]);
        git(&srv, &["config", key, "true"]);
        let rp = format!("--receive-pack={receive_pack}");
        let srv_arg = srv.to_str().unwrap();
        git(&w, &["push", "-q", &rp, srv_arg, "main"]);

        let push = git_out(&w, &["push", &rp, srv_arg, &refspec]);
        let stderr = String::from_utf8_lossy(&push.stderr);
        assert!(!push.status.success(), "{label}: push must fail: {stderr}");
        assert!(
            stderr.contains("[remote rejected]") && stderr.contains("unpacker error"),
            "{label}: {stderr}"
        );
        assert!(!has_ref(&srv, "refs/heads/bad"), "{label}: ref created");
        assert!(
            !git_out(&srv, &["cat-file", "-e", &bad]).status.success(),
            "{label}: malformed commit stored"
        );
        git(&srv, &["fsck", "--strict"]);

        std::fs::write(w.join(label), "clean\n").unwrap();
        git(&w, &["add", "."]);
        git(&w, &["commit", "-qm", label]);
        git(&w, &["push", "-q", &rp, srv_arg, "main"]);
        assert_eq!(rev_parse(&srv, "main"), rev_parse(&w, "main"), "{label}");
        git(&srv, &["fsck", "--strict"]);
    }
}

#[test]
fn receive_fsck_msg_override_lets_the_push_through() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    git(&w, &["init", "-q", "-b", "main", "."]);
    std::fs::write(w.join("f"), "x\n").unwrap();
    git(&w, &["add", "."]);
    git(&w, &["commit", "-qm", "base"]);
    let bad = bad_commit(&w);

    let srv = tmp.path().join("srv.git");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&srv, &["config", "receive.fsckObjects", "true"]);
    git(&srv, &["config", "receive.fsck.missingEmail", "warn"]);
    let rp = format!("--receive-pack={GRIT} receive-pack");
    git(
        &w,
        &[
            "push",
            "-q",
            &rp,
            srv.to_str().unwrap(),
            &format!("{bad}:refs/heads/bad"),
        ],
    );
    assert_eq!(rev_parse(&srv, "refs/heads/bad"), bad);
}
