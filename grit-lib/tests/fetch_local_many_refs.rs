//! Local fetch on the packed 3,000-branch / 2,000-tag shape from #2037.

#[path = "support/fetch_many_refs.rs"]
mod fixture;
#[path = "support/refs_harness.rs"]
mod refs_harness;

use std::collections::BTreeMap;
use std::path::Path;

use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn options(tags: TagMode) -> FetchOptions {
    {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".into()];
        o.tags = tags;
        o.remote_name = Some("origin".into());
        o.fetch_source = Some("fixture".into());
        o
    }
}

fn refs(repo: &Path, prefix: &str) -> BTreeMap<String, String> {
    git(
        repo,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    )
    .lines()
    .map(|line| {
        let (name, oid) = line.split_once(' ').unwrap();
        (name.to_owned(), oid.to_owned())
    })
    .collect()
}

fn assert_parity(remote: &Path, client: &Path) {
    let expected: BTreeMap<_, _> = refs(remote, "refs/heads/")
        .into_iter()
        .map(|(name, oid)| (name.replacen("refs/heads/", "refs/remotes/origin/", 1), oid))
        .collect();
    let mut actual = refs(client, "refs/remotes/origin/");
    actual.remove("refs/remotes/origin/HEAD");
    assert_eq!(actual, expected);
    assert_eq!(refs(remote, "refs/tags/"), refs(client, "refs/tags/"));
    git(client, &["fsck", "--strict"]);
}

fn assert_noop(remote: &Path, client: &Path, opts: &FetchOptions, heads: usize) {
    let outcome = fetch_local(&client.join(".git"), remote, opts).unwrap();
    assert_eq!(outcome.transfer.objects, 0);
    assert_eq!(outcome.transfer.bytes, 0);
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));
    assert_eq!(
        outcome
            .updates
            .iter()
            .filter(|u| !u.remote_ref.starts_with("refs/tags/"))
            .count(),
        heads
    );
    assert!(outcome
        .updates
        .iter()
        .all(|u| u.mode == UpdateMode::UpToDate));
    if opts.tags == TagMode::All {
        assert_eq!(outcome.updates.len(), heads + 2000);
    }
    assert_parity(remote, client);
}

#[test]
fn repeated_many_ref_fetches_preserve_refs_and_transfer_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, client) = fixture::fixture(tmp.path(), 3000, 2000, false);
    for tags in [TagMode::Following, TagMode::None, TagMode::All] {
        let opts = options(tags);
        for _ in 0..2 {
            assert_noop(&remote, &client, &opts, 3001);
        }
    }
    let git_dir = client.join(".git");
    git(
        &client,
        &["update-ref", "--no-deref", "-d", "refs/remotes/origin/HEAD"],
    );
    std::fs::write(git_dir.join("FETCH_HEAD"), "stale\n").unwrap();
    let explicit = {
        let mut o = options(TagMode::Following);
        o.refspecs = vec!["+refs/heads/main:refs/remotes/origin/main".into()];
        o
    };
    assert_noop(&remote, &client, &explicit, 1);
    let fetch_head = std::fs::read_to_string(git_dir.join("FETCH_HEAD")).unwrap();
    assert!(fetch_head.contains(git(&remote, &["rev-parse", "HEAD"]).trim()));
    assert!(!fetch_head.contains("stale"));
    assert_eq!(
        git(&client, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
        "refs/remotes/origin/main"
    );

    // A loose remote tip overrides its packed entry; a loose local symbolic ref
    // must be resolved before classifying the update.
    git(&client, &["commit", "--allow-empty", "-qm", "advance"]);
    let tip = git(&client, &["rev-parse", "HEAD"]);
    git(
        &client,
        &["push", "-q", "origin", "HEAD:refs/heads/team/0000"],
    );
    git(
        &client,
        &[
            "symbolic-ref",
            "refs/remotes/origin/team/0001",
            "refs/remotes/origin/team/0000",
        ],
    );
    let opts = options(TagMode::All);
    let outcome = fetch_local(&client.join(".git"), &remote, &opts).unwrap();
    let symbolic = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/team/0001")
        .unwrap();
    assert_eq!(symbolic.old_oid.unwrap().to_hex(), tip.trim());
    assert_eq!(symbolic.mode, UpdateMode::Forced);
    git(
        &client,
        &[
            "update-ref",
            "--no-deref",
            "-d",
            "refs/remotes/origin/team/0001",
        ],
    );
    git(&client, &["fetch", "-q", "origin"]);

    // Copy a genuinely missing commit and its tree/blob, then repeat without a transfer.
    let seed = tmp.path().join("seed");
    std::fs::write(seed.join("f"), "changed\n").unwrap();
    git(&seed, &["commit", "-qam", "change"]);
    git(&seed, &["push", "-q", "../remote.git", "main"]);
    let outcome = fetch_local(&client.join(".git"), &remote, &opts).unwrap();
    assert!(outcome.transfer.objects > 0);
    assert!(outcome
        .updates
        .iter()
        .any(|u| u.remote_ref == "refs/heads/main" && u.mode == UpdateMode::FastForward));
    assert_noop(&remote, &client, &opts, 3001);

    git(&remote, &["update-ref", "-d", "refs/heads/team/2999"]);
    let opts = {
        let mut o = opts;
        o.prune = true;
        o.tags = TagMode::Following;
        o
    };
    let outcome = fetch_local(&client.join(".git"), &remote, &opts).unwrap();
    assert_eq!(
        outcome
            .updates
            .iter()
            .filter(|u| u.mode == UpdateMode::DeletedMissing)
            .count(),
        1
    );
    assert_noop(&remote, &client, &opts, 3000);
    assert_noop(&remote, &client, &opts, 3000);
}

#[test]
fn reftable_local_fetch_remains_fresh_after_prune() {
    if !refs_harness::require_reftable_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (remote, client) = fixture::fixture(tmp.path(), 30, 20, true);
    let opts = {
        let mut o = options(TagMode::All);
        o.prune = true;
        o
    };
    for _ in 0..2 {
        let outcome = fetch_local(&client.join(".git"), &remote, &opts).unwrap();
        assert_eq!(outcome.transfer.objects, 0);
        assert!(outcome
            .updates
            .iter()
            .all(|u| u.mode == UpdateMode::UpToDate));
    }
    git(&remote, &["update-ref", "-d", "refs/heads/team/0029"]);
    let outcome = fetch_local(&client.join(".git"), &remote, &opts).unwrap();
    assert!(outcome
        .updates
        .iter()
        .any(|u| u.mode == UpdateMode::DeletedMissing));
    let outcome = fetch_local(&client.join(".git"), &remote, &opts).unwrap();
    assert!(outcome
        .updates
        .iter()
        .all(|u| u.mode == UpdateMode::UpToDate));
    assert_parity(&remote, &client);
}

#[test]
fn linked_worktree_fetch_resolves_shared_tracking_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, client) = fixture::fixture(tmp.path(), 10, 5, false);
    git(
        &client,
        &["worktree", "add", "--detach", "../linked", "HEAD"],
    );
    let linked = tmp.path().join("linked");
    let git_dir = git(&linked, &["rev-parse", "--absolute-git-dir"]);
    for tags in [TagMode::Following, TagMode::None, TagMode::All] {
        let outcome = fetch_local(Path::new(git_dir.trim()), &remote, &options(tags)).unwrap();
        assert_eq!(outcome.transfer.objects, 0);
        assert!(outcome
            .updates
            .iter()
            .all(|u| u.mode == UpdateMode::UpToDate));
    }
    assert_parity(&remote, &client);
    git(&linked, &["fsck", "--strict"]);
}
