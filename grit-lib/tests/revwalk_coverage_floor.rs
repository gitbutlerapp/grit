//! Deep rev-list sweeps targeting the `group:revwalk` 85% floor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{
    expand_object_filter_for_protocol, merge_bases, rev_list, ObjectFilter, OrderingMode,
    OutputMode, RevListOptions,
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
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )?;
    let (merge, tip, side) = seed_merged_history(&repo, 1100)?;
    repo.odb().write_commit_graph()?;

    let _ = expand_object_filter_for_protocol("tree:0").map_err(grit_lib::error::Error::Message)?;
    let _ =
        expand_object_filter_for_protocol("blob:none").map_err(grit_lib::error::Error::Message)?;

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
    let formats = [
        OutputMode::OidOnly,
        OutputMode::Parents,
        OutputMode::Format(String::from("oneline")),
        OutputMode::Format(String::from("format:%H %P %T %t %p %s %D %d %gD %N %G?")),
        OutputMode::Format(String::from("tformat:%<(12,trunc)%h %>(10,trunc)%s")),
    ];

    for ordering in orderings {
        for output_mode in &formats {
            for use_graph in [false, true] {
                let opts = RevListOptions {
                    ordering,
                    output_mode: output_mode.clone(),
                    all_refs: use_graph,
                    first_parent: ordering == OrderingMode::Topo,
                    ancestry_path: true,
                    ancestry_path_bottoms: vec![tip],
                    simplify_by_decoration: true,
                    simplify_by_decoration_oids: deco.clone(),
                    full_history: true,
                    simplify_merges: true,
                    preserve_simplify_merges_graph_merges: true,
                    show_pulls: true,
                    sparse: true,
                    parent_rewrite: true,
                    paths: vec!["tracked.txt".into()],
                    objects: true,
                    no_object_names: false,
                    boundary: true,
                    left_right: true,
                    left_right_explicit: true,
                    left_only: false,
                    right_only: false,
                    cherry_mark: true,
                    cherry_pick: false,
                    min_parents: Some(1),
                    max_parents: Some(3),
                    symmetric_left: Some(tip),
                    symmetric_right: Some(side),
                    exclude_first_parent_only: true,
                    filter: Some(ObjectFilter::Combine(vec![
                        ObjectFilter::BlobLimit(8192),
                        ObjectFilter::TreeDepth(8),
                    ])),
                    filter_raw_specs: vec!["blob:limit=8k".into(), "tree:8".into()],
                    filter_provided_objects: true,
                    filter_print_omitted: true,
                    include_reflog_entries: true,
                    include_indexed_objects: true,
                    use_commit_graph: use_graph,
                    use_commit_graph_bloom: use_graph,
                    commit_graph_read_changed_paths: use_graph,
                    use_bitmap_index: use_graph,
                    bitmap_oid_only_objects: use_graph,
                    in_commit_order: true,
                    maximal_only: false,
                    reverse: true,
                    skip: 1,
                    max_count: Some(128),
                    count: false,
                    path_graph_reorder: true,
                    ignore_missing: true,
                    ..RevListOptions::default()
                };
                let _out = rev_list(
                    &repo,
                    &[String::from("main"), String::from("side")],
                    &[],
                    &opts,
                )?;
            }
        }
    }

    let walk = RevWalk::new(&repo);
    let _ = merge_bases(&repo, tip, side, true)?;
    // Every commit has the same tree, so only the root (below the merge base) touches the path;
    // `git rev-list --left-right main...side -- tracked.txt` is empty here.
    let path_limited = walk.walk(
        "main...side",
        &RevListOptions {
            ordering: OrderingMode::AuthorDateTopo,
            left_right: true,
            paths: vec!["tracked.txt".into()],
            ..RevListOptions::default()
        },
    )?;
    assert!(path_limited.commits.is_empty());
    let via_handle = walk.walk(
        "main...side",
        &RevListOptions {
            ordering: OrderingMode::AuthorDateTopo,
            left_right: true,
            ..RevListOptions::default()
        },
    )?;
    // `side` is merged into `main`: the linear commits above the fork plus the merge, all left.
    assert_eq!(via_handle.commits.len(), 1100 - 1100 / 3 - 1 + 1);
    assert!(via_handle
        .commits
        .iter()
        .all(|oid| via_handle.left_right_map.get(oid) == Some(&true)));

    Ok(())
}
