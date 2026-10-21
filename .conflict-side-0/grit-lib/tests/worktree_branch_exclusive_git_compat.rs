//! Linked worktrees must not share a branch checkout (Git `worktree` rules).
//!
//! Setup uses system `git worktree add`; grit switch/delete must refuse like Git.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_test_support::{git, grit_cmd};

fn grit_ok(dir: &Path, args: &[&str]) {
    let out = grit_cmd(args).in_dir(dir).exec();
    assert!(
        out.ok(),
        "grit {args:?} in {} failed:\n{}{}",
        dir.display(),
        out.stdout,
        out.stderr
    );
}

fn grit_switch(dir: &Path, branch: &str) -> (bool, String) {
    let out = grit_cmd(&["switch", branch]).in_dir(dir).exec();
    let ok = out.ok();
    let mut combined = out.stdout;
    combined.push_str(&out.stderr);
    (ok, combined)
}

fn grit_branch_delete(dir: &Path, branch: &str, force: bool) -> (bool, String) {
    let mut args = vec!["branch"];
    if force {
        args.push("-D");
    } else {
        args.push("-d");
    }
    args.push(branch);
    let out = grit_cmd(&args).in_dir(dir).exec();
    let ok = out.ok();
    let mut combined = out.stdout;
    combined.push_str(&out.stderr);
    (ok, combined)
}

fn git_fsck_clean(repo_root: &Path) -> bool {
    let out = Command::new("git")
        .args(["-C", &repo_root.display().to_string(), "fsck"])
        .output()
        .expect("git fsck");
    out.status.success()
}

#[test]
fn grit_switch_refuses_branch_checked_out_in_linked_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).expect("main dir");
    grit_ok(root, &["init", "main"]);
    git(&main, &["config", "user.name", "T"]);
    git(&main, &["config", "user.email", "t@t.com"]);
    std::fs::write(main.join("a.txt"), "a\n").expect("write");
    grit_ok(&main, &["commit", "-m", "base"]);
    grit_ok(&main, &["branch", "other"]);

    let linked = root.join("lw");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "feat",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(linked.join("a.txt"))
        .expect("append")
        .write_all(b"b\n")
        .expect("write");
    grit_ok(&linked, &["commit", "-m", "linked"]);

    let (ok, msg) = grit_switch(&main, "feat");
    assert!(
        !ok,
        "grit switch must refuse when linked worktree has feat, got ok: {msg}"
    );
    assert!(
        msg.contains("feat") && msg.contains("checked out"),
        "expected branch/worktree error, got: {msg}"
    );

    let list = git(&main, &["worktree", "list"]);
    assert!(
        list.contains("feat") && list.matches("[feat]").count() <= 1,
        "worktree list must not show feat on two worktrees:\n{list}"
    );
    assert!(
        git_fsck_clean(&main),
        "git fsck must stay clean on main repo"
    );
}

#[test]
fn grit_branch_delete_refuses_branch_checked_out_in_linked_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).expect("main dir");
    grit_ok(root, &["init", "main"]);
    git(&main, &["config", "user.name", "T"]);
    git(&main, &["config", "user.email", "t@t.com"]);
    std::fs::write(main.join("a.txt"), "a\n").expect("write");
    grit_ok(&main, &["commit", "-m", "base"]);
    grit_ok(&main, &["branch", "other"]);

    let linked = root.join("lw");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "feat",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    grit_ok(&main, &["switch", "other"]);

    let (ok, msg) = grit_branch_delete(&main, "feat", true);
    assert!(
        !ok,
        "grit branch -D must refuse when feat is checked out in linked worktree, got ok: {msg}"
    );
    assert!(
        msg.contains("feat") && msg.contains("checked out"),
        "expected branch/worktree error, got: {msg}"
    );

    let status = git(&linked, &["status", "-sb"]);
    assert!(
        status.contains("feat"),
        "linked worktree must still be on feat, got:\n{status}"
    );
    assert!(
        git(&main, &["show-ref", "--verify", "refs/heads/feat"]).contains("refs/heads"),
        "branch ref must still exist after refused delete"
    );
    assert!(git_fsck_clean(&main), "git fsck must stay clean");
}
