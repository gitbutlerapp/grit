//! Parent rewriting through hidden merges keeps both branches connected.

mod support;

use std::collections::HashSet;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::graph::rewrite_display_parents;
use support::{build_commit_dag, git};

#[test]
fn filtered_hidden_merge_keeps_both_visible_ancestors() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let wt = dir.path();
    git(wt, &["init", "-q", "-b", "main"]);
    let oids = build_commit_dag(
        wt,
        &[("a", "r"), ("b", "r"), ("m", "a"), ("m", "b"), ("tip", "m")],
        &[("tip", "refs/heads/main")],
    );
    let repo = Repository::discover(Some(wt), &RepositoryOptions::default())?;
    let a = oids["a"];
    let b = oids["b"];
    let tip = oids["tip"];
    let visible: HashSet<_> = [a, b, tip].into_iter().collect();
    let parents = rewrite_display_parents(&repo, tip, &visible)?;
    assert!(
        parents.contains(&a) && parents.contains(&b),
        "parents={parents:?}"
    );
    Ok(())
}
