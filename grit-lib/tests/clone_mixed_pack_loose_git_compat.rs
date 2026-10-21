//! Local clone when commit/tree are packed but a reachable blob stays loose (#1036).

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn clone_local_mixed_packed_tree_and_loose_blob() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("p1");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("a"), b"one\n").unwrap();
    git(&remote, &["add", "a"]);
    git(&remote, &["commit", "-qm", "one"]);

    let head = git_stdout(&remote, &["rev-parse", "HEAD"]);
    let tree = git_stdout(&remote, &["rev-parse", "HEAD^{tree}"]);
    let pack_base = remote.join(".git/objects/pack/pack");
    let pack_input = format!("{head}\n{tree}\n");
    let out = Command::new("git")
        .current_dir(&remote)
        .args(["pack-objects", "-q", ".git/objects/pack/pack"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("pack-objects spawn");
    // pack-objects reads oids from stdin; write after spawn.
    use std::io::Write;
    let mut child = out;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(pack_input.as_bytes()).expect("stdin");
    }
    let finished = child.wait_with_output().expect("pack-objects wait");
    assert!(
        finished.status.success(),
        "pack-objects: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    let _ = pack_base;

    git(&remote, &["prune-packed"]);
    git(&remote, &["fsck", "--strict"]);

    let dest = tmp.path().join("pc");
    let outcome = clone_repo(
        CloneOptions {
            url: remote.to_string_lossy().into_owned(),
            dest: dest.clone(),
            environment: Environment::capture_process(),
            remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),
            initial_branch: "main".to_owned(),
            thread_policy: grit_lib::parallel::ThreadPolicy::default(),
            depth: None,
        },
        &mut NoProgress,
        None,
    )
    .expect("grit clone must succeed when blob stays loose");

    git(&dest, &["fsck", "--strict"]);

    let commit = outcome
        .repo
        .odb()
        .read(&outcome.checkout_oid)
        .expect("read tip commit");
    let parsed = parse_commit(&commit.data).expect("parse commit");
    checkout_between_trees(&outcome.repo, None, &parsed.tree).expect("checkout");
    let contents = std::fs::read_to_string(dest.join("a")).expect("worktree file");
    assert_eq!(contents, "one\n");
}
