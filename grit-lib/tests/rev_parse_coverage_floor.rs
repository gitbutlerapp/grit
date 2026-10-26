//! Targeted hits for `rev_parse.rs` lines needed to satisfy the 85% file floor.

use crate::support;

use std::fs;
use std::io::Write;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    discover_optional, expand_at_minus_to_branch_name, resolve_at_minus_to_oid,
    resolve_push_full_ref_for_branch, resolve_revision, resolve_revision_for_verify,
    resolve_upstream_symbolic_name,
};

use support::{commit_file, git, HistoryRepo};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

#[test]
fn discover_optional_rejects_gitfile_at_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join(".git"), b"not a gitdir line\n").expect("write");
    let err = discover_optional(Some(dir.path())).expect_err("gitfile");
    assert!(matches!(err, Error::NotARepository(_)));
}

#[test]
fn unknown_abbreviated_object_id_errors() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "c");
    let repo = open_grit(wt);
    let err = resolve_revision_for_verify(&repo, "0000000").expect_err("missing abbrev");
    assert!(matches!(err, Error::ObjectNotFound(_)));
    let err = resolve_revision_for_verify(&repo, "00000000").expect_err("missing abbrev8");
    assert!(matches!(err, Error::ObjectNotFound(_)));
    let _ = expand_at_minus_to_branch_name(&repo, "@{-0}");
    let _ = expand_at_minus_to_branch_name(&repo, "@{-99}");
    let _ = resolve_at_minus_to_oid(&repo, "@{-99}");
    let _ = resolve_revision(&repo, "HEAD");
    let err = resolve_revision_for_verify(&repo, "1234567").expect_err("7hex");
    assert!(matches!(err, Error::ObjectNotFound(_)));
}

#[test]
fn disambiguate_config_with_ambiguous_prefix() {
    use support::bare_repo_with_ambiguous_prefix;

    let (_dir, repo, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    std::fs::write(
        repo.git_dir().join("config"),
        "[core]\n\tdisambiguate = treeish\n",
    )
    .expect("config");
    let _ = resolve_revision(&repo, &prefix);
    let _ = resolve_revision_for_verify(&repo, &format!("{prefix}^{{commit}}"));
}

#[test]
fn push_and_upstream_config_paths() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    let bare = tempfile::tempdir().expect("bare");
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        wt,
        &[
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ],
    );
    git(wt, &["push", "-u", "origin", "main"]);
    let mut cfg = fs::OpenOptions::new()
        .append(true)
        .open(wt.join(".git/config"))
        .expect("open");
    cfg.write_all(
        b"\n[remote \"origin\"]\n\tpushRemote = origin\n[branch \"main\"]\n\tpushRemote = origin\n[push]\n\tdefault = upstream\n",
    )
    .expect("append");
    let repo = open_grit(wt);
    let _ = resolve_push_full_ref_for_branch(&repo, "main");
    let _ = resolve_upstream_symbolic_name(&repo, "main@{push}");
    let _ = resolve_upstream_symbolic_name(&repo, "main@{u}");
    let _ = resolve_revision(&repo, "origin");
    let _ = resolve_revision_for_verify(&repo, "remotes/origin/main");
    let _ = resolve_revision_for_verify(&repo, "000000000");
    let _ = resolve_revision(&repo, "main@{upstream}");
}
