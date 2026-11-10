//! Lane topology tests for [`grit_lib::revwalk::GraphLayout`].

use grit_lib::progress::{CancelToken, NullProgress};

use crate::support;

use std::collections::HashSet;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;
use grit_lib::revwalk::{GraphCommitInput, GraphLayout};
use support::{build_commit_dag, git};

#[test]
fn graph_layout_criss_cross_topology() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let wt = dir.path();
    git(wt, &["init", "-q", "-b", "main"]);
    // Two branches cross: A-B, X-Y, B merges Y, Y merges B (simplified criss-cross).
    let oids = build_commit_dag(
        wt,
        &[
            ("a", "r"),
            ("x", "r"),
            ("b", "a"),
            ("y", "x"),
            ("m1", "b"),
            ("m1", "y"),
            ("m2", "y"),
            ("m2", "b"),
        ],
        &[("m2", "refs/heads/main")],
    );
    let repo = Repository::discover(Some(wt), &RepositoryOptions::default())?;
    let walk = repo.revwalk().commits(
        &["main"],
        &{
            let mut walk = (&RevWalkListOptions::default()).clone();
            walk.graph = true;
            walk
        },
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    let visible: HashSet<ObjectId> = walk.iter().copied().collect();
    let inputs: Vec<GraphCommitInput> = walk
        .iter()
        .map(|&id| {
            Ok(GraphCommitInput {
                id,
                parents: repo
                    .revwalk()
                    .rewrite_display_parents(&visible)
                    .map(|parents| parents.get(&id).cloned().unwrap_or_default())?,
            })
        })
        .collect::<grit_lib::error::Result<_>>()?;
    let mut layout = GraphLayout::default();
    let layout_blocks = layout.render(&inputs);
    assert_eq!(layout_blocks.len(), walk.len());
    let merge_blocks: Vec<_> = walk
        .iter()
        .zip(&layout_blocks)
        .filter(|(id, _)| {
            oids.get("m1").copied() == Some(**id) || oids.get("m2").copied() == Some(**id)
        })
        .collect();
    assert!(
        !merge_blocks.is_empty(),
        "expected at least one merge commit in walk"
    );
    for (_, block) in merge_blocks {
        assert!(block.iter().any(|line| line.prefix.contains('*')));
    }
    Ok(())
}

#[test]
fn graph_layout_filtered_parents_rewrite() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let wt = dir.path();
    git(wt, &["init", "-q", "-b", "main"]);
    let oids = build_commit_dag(
        wt,
        &[("a", "r"), ("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let repo = Repository::discover(Some(wt), &RepositoryOptions::default())?;
    use grit_lib::revwalk::CommitFilterBuilder;
    let filter = CommitFilterBuilder::new()
        .grep("history harness".to_owned())
        .build()
        .expect("grep pattern");
    let walk = repo.revwalk().commits(
        &["main"],
        &{
            let mut walk = (&RevWalkListOptions {
                commit_filter: Some(std::sync::Arc::new(filter)),
                limit: Some(10),
                ..Default::default()
            })
                .clone();
            walk.graph = true;
            walk
        },
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    assert!(
        walk.len() >= 2,
        "grep filter should still emit multiple matching commits"
    );
    let visible: HashSet<ObjectId> = walk.iter().copied().collect();
    let tip = walk[0];
    let parents = repo
        .revwalk()
        .rewrite_display_parents(&visible)
        .map(|parents| parents.get(&tip).cloned().unwrap_or_default())?;
    assert!(
        !parents.is_empty(),
        "filtered graph must connect through skipped commits"
    );
    let _ = oids;
    Ok(())
}
