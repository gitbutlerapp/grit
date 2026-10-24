//! Clone checkout applies `.gitattributes` `eol=crlf` smudge like `git clone`.

use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::Repository;

fn git(dir: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn tree_of_head(dir: &std::path::Path) -> ObjectId {
    let hex = git_out(dir, &["rev-parse", "HEAD^{tree}"]);
    ObjectId::from_hex(&hex).expect("tree oid")
}

fn init_eol_crlf_source(dir: &std::path::Path) {
    assert!(git(dir, &["init", "-q", "-b", "main", "."]));
    std::fs::write(dir.join(".gitattributes"), b"*.txt text eol=crlf\n").expect("attrs");
    std::fs::write(dir.join("f.txt"), b"a\nb\n").expect("file");
    assert!(git(dir, &["add", "."]));
    assert!(git(
        dir,
        &[
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@e.invalid",
            "commit",
            "-qm",
            "x"
        ]
    ));
}

fn grit_clone_and_checkout(source: &std::path::Path, dest: &std::path::Path) {
    let opts = CloneOptions {
        url: source.to_string_lossy().into_owned(),
        dest: dest.to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(source.to_string_lossy(), dest.to_path_buf())
    };
    let outcome = clone_repo(opts, &mut NoProgress, &NeverCancel, None).expect("grit clone");
    let commit = outcome
        .repo
        .odb()
        .read(&outcome.checkout_oid)
        .expect("commit");
    let parsed = grit_lib::objects::parse_commit(&commit.data).expect("parse commit");
    checkout_between_trees(&outcome.repo, None, &parsed.tree).expect("grit checkout");
}

#[test]
fn clone_checkout_writes_crlf_for_eol_gitattributes() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_eol_crlf_source(upstream.path());

    let grit_dest = tempfile::tempdir().expect("grit dest");
    grit_clone_and_checkout(upstream.path(), grit_dest.path());

    let git_dest = tempfile::tempdir().expect("git dest");
    assert!(git(
        git_dest.path().parent().unwrap(),
        &[
            "clone",
            "-q",
            upstream.path().to_str().unwrap(),
            git_dest.path().file_name().unwrap().to_str().unwrap(),
        ]
    ));

    let grit_bytes = std::fs::read(grit_dest.path().join("f.txt")).expect("grit f.txt");
    let git_bytes = std::fs::read(git_dest.path().join("f.txt")).expect("git f.txt");
    assert_eq!(
        grit_bytes, git_bytes,
        "grit clone worktree bytes must match git clone"
    );
    assert_eq!(grit_bytes, b"a\r\nb\r\n", "expected CRLF smudge on clone");

    let eol = git_out(grit_dest.path(), &["ls-files", "--eol", "f.txt"]);
    assert!(
        eol.contains("w/crlf"),
        "git ls-files --eol in grit clone should report w/crlf, got: {eol:?}"
    );
}

fn assert_worktree_crlf(repo_root: &std::path::Path) {
    let bytes = std::fs::read(repo_root.join("f.txt")).expect("f.txt");
    assert_eq!(
        bytes, b"a\r\nb\r\n",
        "checkout must smudge eol=crlf per .gitattributes"
    );
}

#[test]
fn empty_index_checkout_populated_tree_applies_eol_crlf() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_eol_crlf_source(upstream.path());

    let dest = tempfile::tempdir().expect("dest");
    grit_clone_and_checkout(upstream.path(), dest.path());
    let tree = tree_of_head(dest.path());
    let repo =
        Repository::discover(Some(dest.path()), &RepositoryOptions::empty()).expect("open clone");

    let mut index = Index::new();
    repo.index_handle().write(&mut index).expect("clear index");
    std::fs::remove_file(dest.path().join("f.txt")).expect("remove f.txt");
    std::fs::remove_file(dest.path().join(".gitattributes")).expect("remove attrs");

    checkout_between_trees(&repo, None, &tree).expect("populate from tree");
    assert_worktree_crlf(dest.path());
}

#[test]
fn checkout_populated_tree_smudge_when_gitattributes_sorts_before_target() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_eol_crlf_source(upstream.path());

    let dest = tempfile::tempdir().expect("dest");
    grit_clone_and_checkout(upstream.path(), dest.path());
    let tree = tree_of_head(dest.path());
    let repo =
        Repository::discover(Some(dest.path()), &RepositoryOptions::empty()).expect("open clone");

    let mut index = Index::new();
    repo.index_handle().write(&mut index).expect("clear index");
    std::fs::remove_file(dest.path().join("f.txt")).expect("remove f.txt");
    std::fs::remove_file(dest.path().join(".gitattributes")).expect("remove attrs");

    // Tree walk uses a stack (deepest/last sibling first). Root entries are sorted so
    // `.gitattributes` precedes `f.txt`, meaning `f.txt` is often written before attributes
    // would appear in a naïve incremental index — prospective rules must still smudge CRLF.
    checkout_between_trees(&repo, None, &tree).expect("checkout");
    assert_worktree_crlf(dest.path());
}
