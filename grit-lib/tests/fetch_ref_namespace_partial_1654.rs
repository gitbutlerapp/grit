//! Regression for GitHub issue #1654: fetch must apply non-conflicting tracking
//! ref updates when a stale nested ref blocks another, and hint at `--prune`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use grit_lib::objects::ObjectId;
use grit_lib::remote::transfer::{FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn init_bare_with_branches(tmp: &Path) -> (std::path::PathBuf, ObjectId) {
    let bare = tmp.join("remote.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    let work = tmp.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    fs::write(work.join("README"), "x\n").unwrap();
    git(&work, &["add", "README"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", "gone/x"]);
    fs::write(work.join("gone-x"), "y\n").unwrap();
    git(&work, &["add", "gone-x"]);
    git(&work, &["commit", "-q", "-m", "gone/x"]);
    git(&work, &["push", "-q", "origin", "gone/x:gone/x"]);
    let main: ObjectId = git(&work, &["rev-parse", "main"]).trim().parse().unwrap();
    (bare, main)
}

#[test]
fn fetch_applies_other_refs_when_stale_nested_ref_blocks_without_prune() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _main) = init_bare_with_branches(tmp.path());

    let clone = tmp.path().join("clone");
    fs::create_dir_all(&clone).unwrap();
    git(&clone, &["init", "-q", "-b", "main", "."]);
    git(&clone, &["remote", "add", "origin", bare.to_str().unwrap()]);
    let clone_git = clone.join(".git");

    let mut first_opts = FetchOptions::new();
    first_opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    first_opts.remote_name = Some("origin".to_owned());
    first_opts.tags = TagMode::None;
    grit_lib::repo::Repository::open(
        &clone_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&first_opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &mut first_opts.clone(),
        &mut grit_lib::remote::fetch::NoProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("initial fetch");

    assert!(
        grit_test_support::refs::at(&clone_git)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/gone/x")
                    .expect("valid ref name")
            )
            .is_ok(),
        "stale nested tracking ref should exist after first fetch"
    );

    let work = tmp.path().join("reshape");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["clone", "-q", bare.to_str().unwrap(), "."]);
    git(&work, &["push", "origin", "--delete", "gone/x"]);
    git(&work, &["checkout", "-q", "-b", "gone"]);
    fs::write(work.join("gone"), "g\n").unwrap();
    git(&work, &["add", "gone"]);
    git(&work, &["commit", "-q", "-m", "gone"]);
    git(&work, &["push", "-q", "origin", "gone"]);
    git(&work, &["checkout", "-q", "-b", "other"]);
    fs::write(work.join("other"), "o\n").unwrap();
    git(&work, &["add", "other"]);
    git(&work, &["commit", "-q", "-m", "other"]);
    git(&work, &["push", "-q", "origin", "other"]);

    let outcome = grit_lib::repo::Repository::open(
        &clone_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&first_opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &mut first_opts.clone(),
        &mut grit_lib::remote::fetch::NoProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("fetch without prune");
    assert!(
        outcome.has_rejections(),
        "namespace conflict must be reported as rejection, not hard error"
    );
    assert!(
        grit_test_support::refs::at(&clone_git)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/other")
                    .expect("valid ref name")
            )
            .is_ok(),
        "origin/other must be created despite gone conflict"
    );
    assert!(
        grit_test_support::refs::at(&clone_git)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/gone")
                    .expect("valid ref name")
            )
            .is_err(),
        "origin/gone must not be created while gone/x remains"
    );

    let gone_update = outcome
        .updates
        .iter()
        .find(|u| u.local_ref.as_deref() == Some("refs/remotes/origin/gone"));
    assert_eq!(
        gone_update.map(|u| u.mode),
        Some(UpdateMode::RefLockRejected),
        "gone update should be rejected: {:?}",
        outcome.updates
    );
    let note = gone_update
        .and_then(|u| u.note.as_deref())
        .unwrap_or_default();
    assert!(
        note.contains("gone/x") && note.contains("prune"),
        "rejection should explain the conflict and suggest prune: {note}"
    );

    let git_clone = tmp.path().join("git-clone");
    fs::create_dir_all(&git_clone).unwrap();
    git(&git_clone, &["clone", "-q", bare.to_str().unwrap(), "."]);
    git(&git_clone, &["fetch", "origin"]);
    assert!(
        grit_test_support::refs::at(&git_clone.join(".git"))
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/other")
                    .expect("valid ref name")
            )
            .is_ok(),
        "system git fetch should also create origin/other"
    );
}

#[test]
fn fetch_with_prune_resolves_namespace_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _) = init_bare_with_branches(tmp.path());
    let clone = tmp.path().join("clone");
    fs::create_dir_all(&clone).unwrap();
    git(&clone, &["init", "-q", "-b", "main", "."]);
    git(&clone, &["remote", "add", "origin", bare.to_str().unwrap()]);
    let clone_git = clone.join(".git");

    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.remote_name = Some("origin".to_owned());
    opts.tags = TagMode::None;
    grit_lib::repo::Repository::open(
        &clone_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &mut opts.clone(),
        &mut grit_lib::remote::fetch::NoProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("initial fetch");

    let work = tmp.path().join("reshape");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["clone", "-q", bare.to_str().unwrap(), "."]);
    git(&work, &["push", "origin", "--delete", "gone/x"]);
    git(&work, &["checkout", "-q", "-b", "gone"]);
    fs::write(work.join("gone"), "g\n").unwrap();
    git(&work, &["add", "gone"]);
    git(&work, &["commit", "-q", "-m", "gone"]);
    git(&work, &["push", "-q", "origin", "gone"]);

    let mut pruned = FetchOptions::new();
    pruned.prune = true;
    let outcome = grit_lib::repo::Repository::open(
        &clone_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&pruned).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &mut pruned.clone(),
        &mut grit_lib::remote::fetch::NoProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("fetch with prune");
    assert!(
        !outcome.has_rejections(),
        "prune should remove stale gone/x so gone can be created: {:?}",
        outcome.updates
    );
    assert!(grit_test_support::refs::at(&clone_git)
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/gone")
                .expect("valid ref name")
        )
        .is_ok());
    assert!(grit_test_support::refs::at(&clone_git)
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/gone/x")
                .expect("valid ref name")
        )
        .is_err());
}
