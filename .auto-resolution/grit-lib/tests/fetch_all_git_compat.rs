//! `RepositoryRemotes::fetch_all` matches `git fetch --all` on the same remotes.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::refs::list_refs;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn remote_tracking_refs(repo: &Path) -> BTreeMap<String, String> {
    let git_dir = repo.join(".git");
    list_refs(&git_dir, "refs/remotes")
        .expect("list refs")
        .into_iter()
        .filter(|(name, _)| !name.ends_with("/HEAD"))
        .map(|(name, oid)| (name, oid.to_hex()))
        .collect()
}

fn seed_bare(base: &Path, name: &str, commit_msg: &str) -> PathBuf {
    let bare = base.join(format!("{name}.git"));
    git(
        base,
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let wt = base.join(format!("{name}-wt"));
    git(
        base,
        &["clone", "-q", bare.to_str().unwrap(), wt.to_str().unwrap()],
    );
    git(&wt, &["commit", "-q", "--allow-empty", "-m", commit_msg]);
    git(&wt, &["push", "-q", "origin", "main"]);
    bare
}

#[test]
fn fetch_all_remotes_matches_git_fetch_all() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();
    let upstream_a = seed_bare(base, "upstream-a", "commit-a");
    let upstream_b = seed_bare(base, "upstream-b", "commit-b");

    let grit_repo = base.join("grit-consumer");
    let git_repo = base.join("git-consumer");
    for consumer in [&grit_repo, &git_repo] {
        git(
            base,
            &["init", "-q", "-b", "main", consumer.to_str().unwrap()],
        );
        git(
            consumer,
            &["remote", "add", "first", upstream_a.to_str().unwrap()],
        );
        git(
            consumer,
            &["remote", "add", "second", upstream_b.to_str().unwrap()],
        );
        git(
            consumer,
            &["remote", "add", "broken", "file:///no/such/repository.git"],
        );
        git(
            consumer,
            &["remote", "add", "skipped", upstream_a.to_str().unwrap()],
        );
        git(consumer, &["config", "remote.skipped.skipFetchAll", "true"]);
    }

    let repo = Repository::discover(Some(&grit_repo), &RepositoryOptions::empty()).expect("open");
    let config = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(repo.git_dir()),
        true,
    )
    .expect("config");
    let mut progress = NoProgress;
    let results = repo
        .remotes()
        .fetch_all(
            &config,
            &FetchOptions::default(),
            None,
            None,
            &mut progress,
            None,
        )
        .expect("fetch_all");

    assert_eq!(results.len(), 3, "skipFetchAll remote must not run");
    let (ok, err): (Vec<_>, Vec<_>) = results.into_iter().partition(|(_, r)| r.is_ok());
    assert_eq!(ok.len(), 2, "two file remotes should succeed");
    assert_eq!(err.len(), 1, "broken URL should fail");
    assert_eq!(err[0].0, "broken");

    // Git exits 1 when any remote fails, but successful remotes are still updated.
    Command::new("git")
        .args(["fetch", "--all"])
        .current_dir(&git_repo)
        .status()
        .expect("git fetch --all");

    assert_eq!(
        remote_tracking_refs(&grit_repo),
        remote_tracking_refs(&git_repo),
        "remote-tracking refs must match git fetch --all twin"
    );
}
