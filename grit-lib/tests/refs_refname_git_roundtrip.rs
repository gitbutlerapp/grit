//! Round-trip ref names through [`Repository::references`] / [`Repository::reflogs`] vs system `git`.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{RawRef, RefName, RefNameRef, RefStore};
use grit_lib::repo::Repository;

use crate::support::{
    assert_fsck_when_git_interop, assert_git_fsck_strict, files_repo, git_empty_commit_oid,
    git_for_each_ref, git_init_reftable_repo, git_interop_available, git_reflog_lines,
    require_reftable_git, Backend,
};

fn open_repo(worktree: &Path) -> Repository {
    Repository::discover(Some(worktree), &RepositoryOptions::from_process()).expect("open")
}

/// Update refs through the typed [`grit_lib::refs::References`] handle (not raw transactions).
fn grit_write_refs_via_references(repo: &Repository, tip: ObjectId) {
    for name in ["refs/heads/main", "refs/heads/typed-a", "refs/tags/v1.0"] {
        let rn = RefName::try_from(name).expect("valid ref");
        repo.references()
            .update(&rn, &tip)
            .expect("references().update");
    }
}

fn grit_ref_map(repo: &Repository) -> BTreeMap<String, ObjectId> {
    let store = repo.refs();
    let mut map = BTreeMap::new();
    store
        .for_each_ref("refs/", &mut |entry| {
            let oid = match &entry.value {
                RawRef::Direct(o) => *o,
                RawRef::Symbolic(t) => store
                    .resolve(RefNameRef::from_trusted(t))
                    .expect("resolve symref"),
            };
            map.insert(entry.name.as_str().to_owned(), oid);
            ControlFlow::Continue(())
        })
        .expect("for_each_ref");
    map
}

fn git_ref_map(worktree: &Path) -> BTreeMap<String, ObjectId> {
    git_for_each_ref(worktree, "refs/").into_iter().collect()
}

fn grit_reflog_new_message(repo: &Repository, refname: &str) -> Vec<(ObjectId, String)> {
    let rn = RefName::try_from(refname).expect("valid ref");
    repo.reflogs()
        .entries(&rn)
        .expect("reflog entries")
        .iter()
        .map(|row| (row.entry.new_oid, row.entry.message.clone()))
        .collect()
}

fn git_reflog_new_message(worktree: &Path, refname: &str) -> Vec<(ObjectId, String)> {
    git_reflog_lines(worktree, refname)
        .into_iter()
        .map(|(_selector, oid, msg)| (oid, msg))
        .collect()
}

fn assert_reflog_parity(repo: &Repository, worktree: &Path, refs: &[&str]) {
    for r in refs {
        let grit = grit_reflog_new_message(repo, r);
        let git = git_reflog_new_message(worktree, r);
        assert_eq!(grit, git, "reflog new_oid + message for {r}");
    }
}

fn exercise_grit_then_git(worktree: &Path, fsck: bool) {
    let repo = open_repo(worktree);
    let tip = git_empty_commit_oid(worktree);
    grit_write_refs_via_references(&repo, tip);
    if fsck {
        assert_git_fsck_strict(worktree);
    }
    assert_eq!(
        grit_ref_map(&repo),
        git_ref_map(worktree),
        "git for-each-ref after grit write"
    );
    let listed = repo.references().list("refs/heads/").expect("list heads");
    assert!(listed
        .iter()
        .any(|(n, _)| n.as_str() == "refs/heads/typed-a"));
    // Skip `refs/heads/main`: updating it through `references()` while `HEAD` points there
    // appends both branch and `HEAD` reflog lines; `git reflog show refs/heads/main` only lists
    // the branch log, so parity is checked on the refs that are not also `HEAD`'s peeled target.
    assert_reflog_parity(&repo, worktree, &["refs/heads/typed-a", "refs/tags/v1.0"]);
}

fn exercise_git_then_grit(worktree: &Path, fsck: bool) {
    let tip = git_empty_commit_oid(worktree);
    crate::support::git(
        worktree,
        &["update-ref", "refs/heads/from-git", &tip.to_hex()],
    );
    crate::support::git(
        worktree,
        &[
            "update-ref",
            "refs/tags/from-git-tag",
            &tip.to_hex(),
            "-m",
            "git side",
        ],
    );
    if fsck {
        assert_git_fsck_strict(worktree);
    }
    let repo = open_repo(worktree);
    assert_eq!(
        grit_ref_map(&repo),
        git_ref_map(worktree),
        "grit list after git write"
    );
    assert_reflog_parity(
        &repo,
        worktree,
        &["refs/heads/from-git", "refs/tags/from-git-tag"],
    );
}

#[test]
fn refname_roundtrip_files_backend() {
    if !git_interop_available(Backend::Files) {
        eprintln!("skip refname_roundtrip_files_backend: git interop unavailable");
        return;
    }
    let test = files_repo();
    exercise_grit_then_git(test.worktree(), true);
    exercise_git_then_grit(test.worktree(), true);
    assert_fsck_when_git_interop(&test);
}

#[test]
fn refname_roundtrip_reftable_backend() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let worktree: PathBuf = root.path().to_path_buf();
    exercise_grit_then_git(&worktree, true);
    exercise_git_then_grit(&worktree, true);
}
