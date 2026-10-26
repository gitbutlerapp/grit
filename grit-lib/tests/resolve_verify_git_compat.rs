//! `Repository::verify_object` vs `git rev-parse --verify -q` where Git defines behavior.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::object_inspect::ResolveError;
use grit_lib::repo::Repository;

mod support;

use support::{
    bare_repo_with_ambiguous_prefix, commit_file, git, git_rev_parse_verify_exit, open_grit_repo,
    HistoryRepo,
};

fn assert_verify_exit_parity(repo: &Repository, wt: &std::path::Path, spec: &str) {
    let git_ec = git_rev_parse_verify_exit(wt, spec);
    let grit_ok = repo.verify_object(spec, None).is_ok();
    assert_eq!(
        grit_ok,
        git_ec == 0,
        "spec `{spec}`: git exit {git_ec}, grit ok={grit_ok}"
    );
}

#[test]
fn resolve_verify_git_compat() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "seed");
    git(wt, &["tag", "-a", "v-ann", "-m", "ann"]);
    let tree = git(wt, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    git(wt, &["tag", "v-tree", &tree]);
    let repo = open_grit_repo(wt);

    for spec in ["HEAD", "main", "v-ann", "v-ann^{commit}"] {
        assert_verify_exit_parity(&repo, wt, spec);
    }

    assert_verify_exit_parity(&repo, wt, "no-such-ref");

    let missing = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    assert!(
        repo.verify_object(missing, None).is_err(),
        "missing full hex must not verify without ODB object"
    );

    let (_dir, _bare, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    let bare_wt = _dir.path();
    let bare_repo = Repository::open(
        bare_wt,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open bare");
    assert_verify_exit_parity(&bare_repo, bare_wt, &prefix);

    assert_verify_exit_parity(&repo, wt, "main..HEAD");
    assert_verify_exit_parity(&repo, wt, "main...HEAD");

    match repo.verify_object("main..HEAD", None) {
        Err(ResolveError::RevisionRange { spec }) => assert_eq!(spec, "main..HEAD"),
        other => panic!("expected RevisionRange for range, got {other:?}"),
    }

    match repo.verify_object("^main", None) {
        Err(ResolveError::RevisionRange { spec }) => assert_eq!(spec, "^main"),
        other => panic!("expected RevisionRange for ^main, got {other:?}"),
    }

    assert_verify_exit_parity(&repo, wt, "v-tree^{commit}");
}
