//! Guard that push fast-path commit selection stays bounded on the deep-history fixture.

#![allow(clippy::unwrap_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::{
    walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use std::path::Path;

const FIXTURE_CLIENT: &str = "/tmp/grit-bench-network-cache/deep-history-prod.git-client";
const FETCH_BASE: &str = "85a77d0004ca5e990b8f2a7c79a28319f66c21f7";

#[test]
fn push_fast_hide_window_is_incremental_not_full_history() {
    let git_dir = Path::new(FIXTURE_CLIENT).join(".git");
    if !git_dir.is_dir() {
        return;
    }
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open client");
    let tip = resolve_ref(&git_dir, "refs/heads/main").expect("main");
    let base = ObjectId::from_hex(FETCH_BASE).expect("base hex");
    let (_, commits) = walk_commits_reachable_excluding_ancestors_of(
        &repo,
        &[tip],
        &[base],
        ReachableWalkLimit::Unlimited,
        Default::default(),
    )
    .expect("walk");
    assert!(
        commits.len() <= 128,
        "expected incremental push window, got {} commits",
        commits.len()
    );
}
