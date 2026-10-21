//! Target / upstream resolution and tip comparison against repositories built by system `git`.

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::branch_tracking::{
    comparison_base, target_branch, upstream_branch, BaseSource, Standing, TipComparison,
};
use grit_lib::refs::store::{MemoryRefStore, RawRef};
use grit_lib::repo::Repository;
use grit_test_support::{git, unique_tmp, write_file};

fn commit(dir: &Path, file: &str, message: &str) {
    write_file(dir, file, message);
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

fn rev(dir: &Path, spec: &str) -> ObjectId {
    git(dir, &["rev-parse", spec])
        .trim()
        .parse()
        .expect("rev-parse prints an oid")
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

/// `origin` with two commits on `main`, cloned to `clone` (tracking `origin/main`), where the
/// clone then gains two commits of its own and `origin` one more (fetched).
fn diverged_clone(tag: &str) -> std::path::PathBuf {
    let root = unique_tmp("branch-compare", tag);
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).expect("mkdir origin");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "a", "a1");
    commit(&origin, "a", "a2");
    git(&root, &["clone", "-q", "origin", "clone"]);
    let clone = root.join("clone");
    commit(&clone, "b", "b1");
    commit(&clone, "b", "b2");
    commit(&origin, "a", "a3");
    git(&clone, &["fetch", "-q"]);
    clone
}

fn git_left_right(dir: &Path, range: &str) -> (usize, usize) {
    let out = git(dir, &["rev-list", "--left-right", "--count", range]);
    let mut parts = out.split_whitespace().map(|n| n.parse().expect("count"));
    (parts.next().expect("left"), parts.next().expect("right"))
}

#[test]
fn upstream_branch_resolves_configured_tracking_ref() {
    let clone = diverged_clone("upstream");
    let repo = open(&clone);
    let upstream = upstream_branch(&repo, "main").expect("main tracks origin/main");
    assert_eq!(upstream.name, "origin/main");
    assert_eq!(upstream.full_ref, "refs/remotes/origin/main");
    assert_eq!(upstream.oid, rev(&clone, "origin/main"));
    assert_eq!(upstream.source, BaseSource::Upstream);

    git(&clone, &["branch", "-q", "--no-track", "loose"]);
    assert_eq!(upstream_branch(&repo, "loose"), None);
}

#[test]
fn upstream_branch_is_none_when_tracking_ref_is_missing() {
    let clone = diverged_clone("upstream-gone");
    git(&clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
    let repo = open(&clone);
    assert_eq!(upstream_branch(&repo, "main"), None);
}

#[test]
fn target_branch_prefers_config_then_default_candidates() {
    let clone = diverged_clone("target");
    let repo = open(&clone);
    let target = target_branch(&repo).expect("target").expect("found");
    assert_eq!(target.name, "origin/main");
    assert_eq!(target.full_ref, "refs/remotes/origin/main");
    assert_eq!(target.oid, rev(&clone, "origin/main"));
    assert_eq!(target.source, BaseSource::Target);

    git(&clone, &["branch", "-q", "--no-track", "develop", "HEAD~1"]);
    git(&clone, &["config", "target.branch", "  develop  "]);
    let repo = open(&clone);
    let target = target_branch(&repo).expect("target").expect("found");
    assert_eq!(target.name, "develop");
    assert_eq!(target.full_ref, "refs/heads/develop");
    assert_eq!(target.oid, rev(&clone, "develop"));

    git(&clone, &["config", "target.branch", "no-such-branch"]);
    let repo = open(&clone);
    let target = target_branch(&repo).expect("target").expect("found");
    assert_eq!(target.name, "origin/main", "unresolvable config falls back");
}

#[test]
fn target_branch_is_none_without_any_candidate() {
    let root = unique_tmp("branch-compare", "no-target");
    git(&root, &["init", "-q", "-b", "trunk"]);
    commit(&root, "f", "only");
    let repo = open(&root);
    assert_eq!(target_branch(&repo).expect("target"), None);
    assert_eq!(comparison_base(&repo, "trunk").expect("base"), None);
}

#[test]
fn comparison_base_prefers_upstream_over_target() {
    let clone = diverged_clone("base");
    git(&clone, &["branch", "-q", "--no-track", "develop", "HEAD~1"]);
    git(&clone, &["config", "target.branch", "develop"]);
    let repo = open(&clone);

    let base = comparison_base(&repo, "main")
        .expect("base")
        .expect("found");
    assert_eq!(base.source, BaseSource::Upstream);
    assert_eq!(base.name, "origin/main");

    git(&clone, &["checkout", "-q", "-b", "topic"]);
    let repo = open(&clone);
    let base = comparison_base(&repo, "topic")
        .expect("base")
        .expect("found");
    assert_eq!(base.source, BaseSource::Target);
    assert_eq!(base.name, "develop");
}

#[test]
fn tip_comparison_matches_git_rev_list_left_right() {
    let clone = diverged_clone("compare");
    let repo = open(&clone);
    let head = rev(&clone, "HEAD");
    let upstream = rev(&clone, "origin/main");

    let cmp = TipComparison::compute(&repo, head, upstream, 1).expect("compare");
    assert_eq!(
        (cmp.ahead, cmp.behind),
        git_left_right(&clone, "HEAD...origin/main")
    );
    assert_eq!((cmp.ahead, cmp.behind), (2, 1));
    assert_eq!(cmp.standing(), Standing::Diverged);
    assert_eq!(cmp.newest_ahead, vec![head], "limit keeps only the newest");

    let all = TipComparison::compute(&repo, head, upstream, 10).expect("compare");
    assert_eq!(all.newest_ahead, vec![head, rev(&clone, "HEAD~1")]);

    let base = rev(&clone, "HEAD~2");
    let ahead = TipComparison::compute(&repo, head, base, 0).expect("compare");
    assert_eq!((ahead.ahead, ahead.behind), (2, 0));
    assert_eq!(ahead.standing(), Standing::Ahead);
    assert!(ahead.newest_ahead.is_empty());

    let behind = TipComparison::compute(&repo, base, upstream, 5).expect("compare");
    assert_eq!(
        (behind.ahead, behind.behind),
        git_left_right(&clone, "HEAD~2...origin/main")
    );
    assert_eq!(behind.standing(), Standing::Behind);

    let even = TipComparison::compute(&repo, head, head, 5).expect("compare");
    assert_eq!((even.ahead, even.behind), (0, 0));
    assert_eq!(even.standing(), Standing::Even);
}

#[test]
fn target_branch_reads_through_injected_ref_store() {
    let root = unique_tmp("branch-compare", "memory-store");
    git(&root, &["init", "-q", "-b", "trunk"]);
    let oid: ObjectId = "0300000000000000000000000000000000000000"
        .parse()
        .expect("oid");
    let store = Arc::new(MemoryRefStore::new());
    store
        .set_ref("refs/heads/main", RawRef::Direct(oid))
        .expect("set ref");
    let repo = open(&root).with_ref_store(store);

    let target = target_branch(&repo)
        .expect("target")
        .expect("found in store");
    assert_eq!(target.name, "main");
    assert_eq!(target.full_ref, "refs/heads/main");
    assert_eq!(target.oid, oid);
}
