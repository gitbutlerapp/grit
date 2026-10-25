//! Issue #1964: `ident` attribute — expanded `$Id:` in the worktree must clean to `$Id$`.

use std::fs;
use std::io::Write as _;

use grit_test_support::{git_cmd, grit_cmd};
use tempfile::TempDir;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(args: &[&str], root: &std::path::Path) -> grit_test_support::Output {
    git_cmd(args)
        .in_dir(root)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc()
}

fn grit(args: &[&str], root: &std::path::Path) -> grit_test_support::Output {
    grit_cmd(args)
        .in_dir(root)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc()
}

fn init_ident_repo(root: &std::path::Path, autocrlf: bool) {
    git(&["init", "-q"], root);
    git(&["config", "user.email", "t@e.com"], root);
    git(&["config", "user.name", "T"], root);
    if autocrlf {
        git(&["config", "core.autocrlf", "true"], root);
    }
    fs::write(root.join(".gitattributes"), "id.txt ident\n").expect("gitattributes");
    fs::write(root.join("id.txt"), "$Id$\nfoo\n").expect("id.txt");
    git(&["add", "-A"], root);
    git(&["commit", "-qm", "i"], root);
    fs::remove_file(root.join("id.txt")).expect("remove id.txt");
    git(&["checkout", "-q", "id.txt"], root);
}

#[test]
fn ident_checkout_status_and_diff_clean() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    init_ident_repo(root, false);

    assert_eq!(git(&["status", "--short"], root).stdout.trim(), "");
    assert_eq!(grit(&["status", "--short"], root).stdout.trim(), "");
    let diff_json = grit(&["diff", "--json"], root).stdout;
    assert!(
        diff_json.contains("\"changed\":false"),
        "grit diff should report no changes after ident checkout: {diff_json}"
    );
}

#[test]
fn ident_commit_stores_keyword_not_expansion() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    init_ident_repo(root, false);

    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(root.join("id.txt"))
        .expect("open id.txt");
    writeln!(f, "bar").expect("append bar");

    grit(&["commit", "-m", "two"], root);

    let blob = git(&["cat-file", "-p", "HEAD:id.txt"], root).stdout;
    assert_eq!(
        blob, "$Id$\nfoo\nbar\n",
        "committed blob must keep $Id$ keyword, not expanded id"
    );
}

#[test]
fn ident_checkout_clean_with_core_autocrlf_true() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    init_ident_repo(root, true);

    assert_eq!(grit(&["status", "--short"], root).stdout.trim(), "");
}

#[test]
fn ident_stash_pop_after_real_edit() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    init_ident_repo(root, false);

    assert_eq!(grit(&["status", "--short"], root).stdout.trim(), "");

    fs::write(root.join("id.txt"), "edited\n").expect("edit");
    grit(&["stash", "push", "-m", "hold"], root);
    grit(&["stash", "pop"], root);
}
