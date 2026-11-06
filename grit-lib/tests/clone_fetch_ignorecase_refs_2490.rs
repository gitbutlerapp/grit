//! Issue #2490: clone and fetch must tolerate remote refs that differ only by ASCII case
//! when `core.ignorecase` is enabled (macOS APFS / Windows NTFS).

#[allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::progress::NeverCancel;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn commit_oid_in_bare(bare: &Path) -> ObjectId {
    let tree_out = git(bare, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let tree = tree_out.trim();
    let out = Command::new("git")
        .current_dir(bare)
        .args(["commit-tree", tree, "-m", "c"])
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("commit-tree");
    assert!(out.status.success(), "commit-tree failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .parse()
        .expect("commit oid")
}

fn bare_with_casefold_packed_refs(tmp: &Path) -> (std::path::PathBuf, ObjectId) {
    let bare = tmp.join("remote.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    let c = commit_oid_in_bare(&bare);
    let packed = format!(
        "# pack-refs with: peeled fully-peeled sorted\n\
         {c} refs/heads/Foo\n\
         {c} refs/heads/foo\n\
         {c} refs/heads/main\n\
         {c} refs/tags/V1\n\
         {c} refs/tags/v1\n"
    );
    fs::write(bare.join("packed-refs"), packed).expect("packed-refs");
    (bare, c)
}

fn git_for_each_ref_names(git_dir: &Path, prefix: &str) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(git_dir.parent().unwrap_or(git_dir))
        .args([
            "-C",
            git_dir.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)",
            prefix,
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("for-each-ref");
    assert!(out.status.success(), "for-each-ref failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn clone_stores_casefold_remote_refs_in_packed_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _c) = bare_with_casefold_packed_refs(tmp.path());
    let dest = tmp.path().join("grit-clone");
    let url = bare.to_str().unwrap().to_owned();

    let mut opts = CloneOptions::new(url, dest.clone());
    opts.config
        .push(("core.ignorecase".to_owned(), "true".to_owned()));
    opts.checkout = false;
    clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone with casefold remote refs");

    assert!(dest.join(".git").is_dir(), "clone destination must remain");
    let git_dir = dest.join(".git");
    for name in [
        "refs/remotes/origin/Foo",
        "refs/remotes/origin/foo",
        "refs/tags/V1",
        "refs/tags/v1",
        "refs/remotes/origin/main",
    ] {
        resolve_ref(&git_dir, name).unwrap_or_else(|_| panic!("missing {name}"));
    }

    let mut names = git_for_each_ref_names(&git_dir, "refs/");
    names.sort();
    assert!(
        names.iter().any(|n| n == "refs/remotes/origin/Foo")
            && names.iter().any(|n| n == "refs/remotes/origin/foo")
            && names.iter().any(|n| n == "refs/tags/V1")
            && names.iter().any(|n| n == "refs/tags/v1"),
        "git must see every spelling: {names:?}"
    );
    let fsck = git(&git_dir, &["fsck", "--strict", "--no-progress"]);
    assert!(
        !fsck.contains("error"),
        "git fsck should accept packed casefold refs: {fsck}"
    );
}

#[test]
fn fetch_rejects_second_case_twin_and_keeps_first() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _c) = bare_with_casefold_packed_refs(tmp.path());

    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    git(&repo, &["config", "core.ignorecase", "true"]);
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    let git_dir = repo.join(".git");

    let mut opts = FetchOptions::new();
    opts.refspecs = vec![
        "+refs/heads/*:refs/remotes/origin/*".to_owned(),
        "+refs/tags/*:refs/tags/*".to_owned(),
    ];
    opts.remote_name = Some("origin".to_owned());
    opts.tags = TagMode::None;

    let outcome = fetch_local(&git_dir, &bare, &opts).expect("fetch continues with rejections");
    assert!(
        outcome.has_rejections(),
        "case twin must be reported as rejection"
    );

    resolve_ref(&git_dir, "refs/remotes/origin/main").expect("main stored");
    resolve_ref(&git_dir, "refs/remotes/origin/Foo").expect("first branch spelling stored");
    assert!(
        resolve_ref(&git_dir, "refs/remotes/origin/foo").is_err(),
        "second branch spelling must not be stored as loose ref"
    );
    resolve_ref(&git_dir, "refs/tags/V1").expect("first tag spelling stored");

    let rejected = outcome
        .updates
        .iter()
        .filter(|u| u.mode == UpdateMode::RefLockRejected)
        .collect::<Vec<_>>();
    assert!(
        !rejected.is_empty(),
        "expected at least one rejected ref update: {:?}",
        outcome.updates
    );
    let note = rejected
        .iter()
        .find_map(|u| u.note.as_deref())
        .unwrap_or_default();
    assert!(
        note.contains("ignorecase") || note.contains("case"),
        "rejection should mention case: {note}"
    );
    assert!(
        note.contains("reftable"),
        "rejection should suggest reftable backend: {note}"
    );
}
