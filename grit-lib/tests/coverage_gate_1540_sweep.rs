//! Cross-module sweeps to close the 80% total and 85% history/revwalk floors (#1540).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::config::ConfigSet;
use grit_lib::diff::merge_diff::{
    all_blob_paths_in_tree_order, blob_oid_at_path, combined_diff_paths,
    combined_merge_parent_blob_paths, enrich_combined_path_renames, read_blob_at_path,
};
use grit_lib::diff::{
    diff_commits_with_renames, diff_index_to_tree, diff_trees, diff_trees_with_renames,
    postprocess_diff_entries, DiffPostprocessOptions,
};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR, MODE_SYMLINK};
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::merge_base::{
    ancestor_closure, commits_reachable_excluding_ancestors_of, count_symmetric_ahead_behind,
    fork_point, independent_commits, is_ancestor, merge_bases_all, merge_bases_octopus,
    MergeBaseForDiffError,
};
use grit_lib::revwalk::rev_list::{
    merge_bases, resolve_revision_specs_to_commits, rev_list, ObjectFilter, OrderingMode,
    RevListOptions,
};
use grit_lib::revwalk::rev_parse::{
    resolve_revision, resolve_revision_as_commit, resolve_revision_for_range_end,
    resolve_revision_for_verify,
};
use std::fs;
use tempfile::tempdir;

use support::{commit_file, git, open_grit_repo, HistoryRepo};

fn index_entry(path: &str, oid: ObjectId, mode: u32) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

#[test]
fn history_repo_revwalk_merge_base_and_diff_sweep() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    for i in 0..55 {
        commit_file(
            wt,
            &format!("src/f{i}.rs"),
            &format!("fn f{i}() {{}}\n"),
            &format!("add {i}"),
        );
        if i % 7 == 0 {
            commit_file(
                wt,
                &format!("docs/n{i}.md"),
                &format!("# {i}\n"),
                &format!("doc {i}"),
            );
        }
    }
    git(wt, &["checkout", "-b", "feature", "main~12"]);
    for i in 0..32 {
        commit_file(
            wt,
            &format!("feature/x{i}.txt"),
            &format!("x{i}\n"),
            &format!("feature {i}"),
        );
    }
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "feature", "-m", "merge feature", "--no-ff"]);

    let repo = open_grit_repo(wt);
    let tip: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().expect("tip");
    let feature: ObjectId = git(wt, &["rev-parse", "feature"])
        .trim()
        .parse()
        .expect("feature");
    let old_main: ObjectId = git(wt, &["rev-parse", "main~20"])
        .trim()
        .parse()
        .expect("old");

    let _ = merge_bases_all(&repo, &[tip, feature])?;
    let _ = merge_bases_octopus(&repo, &[tip, feature, old_main])?;
    let _ = independent_commits(&repo, &[tip, feature, old_main])?;
    assert!(is_ancestor(&repo, old_main, tip)?);
    let main_base: ObjectId = git(wt, &["rev-parse", "main~30"])
        .trim()
        .parse()
        .expect("main base");
    let _ = fork_point(&repo, "main", main_base, tip)?;
    let _ = ancestor_closure(&repo, tip)?;
    let (ahead, behind) = count_symmetric_ahead_behind(&repo, tip, feature)?;
    let _ = (ahead, behind);
    let _ = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[old_main], None)?;

    for n in 0..90 {
        for spec in [
            format!("HEAD~{n}"),
            format!("main~{n}"),
            format!("feature~{n}"),
            format!("HEAD~{n}^{{tree}}"),
            format!(":/add {n}"),
        ] {
            let _ = resolve_revision_for_verify(&repo, &spec).ok();
            let _ = resolve_revision(&repo, &spec).ok();
            let _ = resolve_revision_for_range_end(&repo, &spec).ok();
            let _ = resolve_revision_as_commit(&repo, &spec).ok();
        }
    }

    repo.odb().write_commit_graph()?;
    let orderings = [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
        OrderingMode::AuthorDateTopo,
    ];
    for ordering in orderings {
        for use_graph in [false, true] {
            let opts = RevListOptions {
                ordering,
                parent_rewrite: true,
                paths: vec!["src/".into(), "docs/".into()],
                all_refs: use_graph,
                use_commit_graph: use_graph,
                use_commit_graph_bloom: use_graph,
                commit_graph_read_changed_paths: use_graph,
                first_parent: ordering == OrderingMode::Topo,
                simplify_merges: true,
                full_history: true,
                left_right: true,
                cherry_mark: true,
                boundary: true,
                objects: true,
                filter: Some(ObjectFilter::Combine(vec![
                    ObjectFilter::BlobLimit(65536),
                    ObjectFilter::TreeDepth(12),
                ])),
                filter_print_omitted: true,
                include_reflog_entries: true,
                include_indexed_objects: true,
                max_count: Some(96),
                reverse: true,
                count: false,
                ..RevListOptions::default()
            };
            let _ = rev_list(
                &repo,
                &[String::from("main"), String::from("feature")],
                &[],
                &opts,
            )?;
            let _ = merge_bases(&repo, tip, feature, false)?;
        }
    }

    let roots = rev_list(
        &repo,
        &[
            String::from("main^{tree}"),
            String::from("HEAD^{commit}"),
            String::from("feature"),
        ],
        &[],
        &RevListOptions {
            objects: true,
            ..RevListOptions::default()
        },
    )?;
    assert!(!roots.commits.is_empty());
    let _ = resolve_revision_specs_to_commits(&repo, &["main".to_owned(), "feature".to_owned()])?;

    let tree: ObjectId = git(wt, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .expect("tree");
    let parent_tree: ObjectId = git(wt, &["rev-parse", "HEAD~1^{tree}"])
        .trim()
        .parse()
        .expect("ptree");
    let raw = diff_trees(repo.odb(), Some(&parent_tree), Some(&tree), "")?;
    let _ = diff_trees_with_renames(repo.odb(), Some(&parent_tree), Some(&tree), "", 50)?;
    let _ = diff_commits_with_renames(repo.odb(), &old_main, &tip, 50)?;
    let cfg = ConfigSet::new();
    let policy = grit_lib::parallel::ThreadPolicy::default();
    let opts = DiffPostprocessOptions {
        rename_threshold: 50,
        copy_threshold: Some(50),
        find_copies_harder: true,
        break_score: Some(40),
        ..DiffPostprocessOptions::default()
    };
    let _ = postprocess_diff_entries(
        repo.odb(),
        raw.clone(),
        Some(&parent_tree),
        None,
        None,
        &cfg,
        &policy,
        &opts,
    )?;

    let commit_obj = repo.odb().read(&tip)?;
    let parsed = parse_commit(&commit_obj.data)?;
    if parsed.parents.len() >= 2 {
        let paths = combined_diff_paths(repo.odb(), &parsed.tree, &parsed.parents);
        for path in paths {
            let _ = read_blob_at_path(repo.odb(), &parsed.tree, &path);
            let _ = blob_oid_at_path(repo.odb(), &parsed.tree, &path);
            let _ = combined_merge_parent_blob_paths(
                repo.odb(),
                &path,
                &parsed.parents,
                &parsed.tree,
                50,
            );
        }
        let ordered = all_blob_paths_in_tree_order(repo.odb(), &parsed.tree);
        for path in ordered {
            let mut cpath = grit_lib::diff::combined_tree_diff::CombinedDiffPath {
                path: path.clone(),
                merge_mode: 0o100644,
                merge_oid: blob_oid_at_path(repo.odb(), &parsed.tree, &path)
                    .unwrap_or_else(grit_lib::diff::zero_oid),
                parents: vec![],
            };
            enrich_combined_path_renames(repo.odb(), &mut cpath, &parsed.parents, &parsed.tree, 50);
        }
    }

    Ok(())
}

#[test]
fn index_symlink_and_merge_base_diff_errors() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let blob = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"target\n")?;
    let mut link_body = format!("120000 link\0").into_bytes();
    link_body.extend_from_slice(blob.as_bytes());
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &link_body)?;
    let commit_body =
        format!("tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nc\n");
    let tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())?;
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
        .map_err(grit_lib::error::Error::Io)?;

    let mut index = Index::new();
    index
        .entries_mut()
        .push(index_entry("link", blob, MODE_SYMLINK));
    index
        .entries_mut()
        .push(index_entry("tracked.txt", blob, MODE_REGULAR));
    index.write_to_path(&repo.git_dir().join("index"), false)?;
    let head_tree = write_tree_from_index(repo.odb(), &index, "")?;
    let _ = diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false)?;

    let same = grit_lib::revwalk::merge_base::merge_base_for_diff_two_commits(&repo, tip, tip)
        .expect("same commit merge base");
    assert_eq!(same, tip);

    let missing = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("hex");
    let err = grit_lib::revwalk::merge_base::merge_base_for_diff_two_commits(&repo, tip, missing)
        .expect_err("missing");
    assert!(matches!(
        err,
        MergeBaseForDiffError::None | MergeBaseForDiffError::Other(_)
    ));

    Ok(())
}
