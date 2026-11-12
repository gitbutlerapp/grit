//! Deep rev-list sweeps targeting the `group:revwalk` 85% floor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::progress::{CancelToken, NullProgress};

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{
    expand_object_filter_for_protocol, ObjectFilter, OrderingMode, RevListOptions,
};
use grit_lib::revwalk::RevWalk;
use std::collections::HashSet;
use std::fs;
use tempfile::tempdir;

fn write_tree(
    odb: &grit_lib::objects::odb::Odb,
    name: &str,
    blob: ObjectId,
) -> grit_lib::error::Result<ObjectId> {
    let mut body = format!("100644 {name}\0").into_bytes();
    body.extend_from_slice(blob.as_bytes());
    odb.write_loose_materialize(ObjectKind::Tree, &body)
}

fn seed_merged_history(
    repo: &Repository,
    depth: usize,
) -> grit_lib::error::Result<(ObjectId, ObjectId, ObjectId)> {
    let blob = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"payload\n")?;
    let tree = write_tree(repo.odb(), "tracked.txt", blob)?;
    let ident = "T <t@e.com> 100 +0000";
    let mut parent: Option<ObjectId> = None;
    let mut linear = Vec::new();
    for i in 0..depth {
        let mut body = format!("tree {tree}\n");
        if let Some(p) = parent {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!("author {ident}\ncommitter {ident}\n\nc{i}\n"));
        let oid = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())?;
        parent = Some(oid);
        linear.push(oid);
    }
    let tip = *linear.last().expect("tip");
    let base = linear[linear.len() / 3];
    let side_body =
        format!("tree {tree}\nparent {base}\nauthor {ident}\ncommitter {ident}\n\nside\n");
    let side_tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, side_body.as_bytes())?;
    fs::write(
        repo.git_dir().join("refs/heads/side"),
        format!("{side_tip}\n"),
    )
    .map_err(grit_lib::error::Error::Io)?;
    let merge_body = format!(
        "tree {tree}\nparent {tip}\nparent {side_tip}\nauthor {ident}\ncommitter {ident}\n\nmerge\n"
    );
    let merge = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, merge_body.as_bytes())?;
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{merge}\n"))
        .map_err(grit_lib::error::Error::Io)?;
    for (i, oid) in linear.iter().enumerate().step_by(4) {
        fs::write(
            repo.git_dir().join(format!("refs/heads/topic-{i}")),
            format!("{oid}\n"),
        )
        .map_err(grit_lib::error::Error::Io)?;
    }
    Ok((merge, tip, side_tip))
}

#[test]
fn revwalk_floor_cartesian_sweep() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )?;
    let (merge, tip, side) = seed_merged_history(&repo, 1100)?;
    repo.odb().write_commit_graph()?;

    let _ =
        expand_object_filter_for_protocol("tree:0").map_err(grit_lib::error::Error::Verbatim)?;
    let _ =
        expand_object_filter_for_protocol("blob:none").map_err(grit_lib::error::Error::Verbatim)?;

    let mut deco = HashSet::new();
    deco.insert(merge);
    deco.insert(tip);
    deco.insert(side);

    let orderings = [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
        OrderingMode::AuthorDateTopo,
    ];
    for ordering in orderings {
        for use_graph in [false, true] {
            let opts = RevListOptions::default()
                .with_ordering(ordering)
                .with_all_refs(use_graph)
                .with_first_parent(ordering == OrderingMode::Topo)
                .with_ancestry_path(true)
                .with_ancestry_path_bottoms(vec![tip])
                .with_simplify_by_decoration(true)
                .with_simplify_by_decoration_oids(deco.clone())
                .with_full_history(true)
                .with_simplify_merges(true)
                .with_preserve_simplify_merges_graph_merges(true)
                .with_show_pulls(true)
                .with_sparse(true)
                .with_parent_rewrite(true)
                .with_paths(vec!["tracked.txt".into()])
                .with_objects(true)
                .with_boundary(true)
                .with_left_right(true)
                .with_left_right_explicit(true)
                .with_cherry_mark(true)
                .with_min_parents(Some(1))
                .with_max_parents(Some(3))
                .with_symmetric_left(Some(tip))
                .with_symmetric_right(Some(side))
                .with_exclude_first_parent_only(true)
                .with_filter(Some(ObjectFilter::Combine(vec![
                    ObjectFilter::BlobLimit(8192),
                    ObjectFilter::TreeDepth(8),
                ])))
                .with_filter_raw_specs(vec!["blob:limit=8k".into(), "tree:8".into()])
                .with_filter_provided_objects(true)
                .with_filter_print_omitted(true)
                .with_include_reflog_entries(true)
                .with_include_indexed_objects(true)
                .with_use_commit_graph(use_graph)
                .with_use_commit_graph_bloom(use_graph)
                .with_commit_graph_read_changed_paths(use_graph)
                .with_use_bitmap_index(use_graph)
                .with_bitmap_oid_only_objects(use_graph)
                .with_in_commit_order(true)
                .with_reverse(true)
                .with_skip(1)
                .with_max_count(Some(128))
                .with_path_graph_reorder(true)
                .with_ignore_missing(true);
            let _out = repo.revwalk().rev_list(
                &[String::from("main"), String::from("side")],
                &[],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )?;
        }
    }

    let walk = RevWalk::new(&repo);
    let _ = repo
        .revwalk()
        .merge_bases(&[tip, side], grit_lib::revwalk::MergeBaseMode::All)?;
    // Every commit has the same tree, so only the root (below the merge base) touches the path;
    // `git rev-list --left-right main...side -- tracked.txt` is empty here.
    let path_limited = walk.walk(
        "main...side",
        &RevListOptions::default()
            .with_ordering(OrderingMode::AuthorDateTopo)
            .with_left_right(true)
            .with_paths(vec!["tracked.txt".into()]),
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    assert!(path_limited.commits.is_empty());
    let via_handle = walk.walk(
        "main...side",
        &RevListOptions::default()
            .with_ordering(OrderingMode::AuthorDateTopo)
            .with_left_right(true),
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    // `side` is merged into `main`: the linear commits above the fork plus the merge, all left.
    assert_eq!(via_handle.commits.len(), 1100 - 1100 / 3 - 1 + 1);
    assert!(via_handle
        .commits
        .iter()
        .all(|oid| via_handle.left_right_map.get(oid) == Some(&true)));

    Ok(())
}
