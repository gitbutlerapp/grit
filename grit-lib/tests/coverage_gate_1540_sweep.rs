//! Cross-module sweeps to close the 80% total and 85% history/revwalk floors (#1540).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::FileMode;
use grit_lib::progress::{CancelToken, NullProgress};

use crate::support;

use grit_lib::config::ConfigSet;
use grit_lib::diff::merge_diff::{
    all_blob_paths_in_tree_order, blob_oid_at_path, combined_diff_paths,
    combined_merge_parent_blob_paths, enrich_combined_path_renames, read_blob_at_path,
};
use grit_lib::diff::{BreakRewrites, CopyDetection, RenameDetection, TreeDiffOptions};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR, MODE_SYMLINK};
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::merge_base::MergeBaseForDiffError;
use grit_lib::revwalk::rev_list::{ObjectFilter, OrderingMode, RevListOptions};

use std::fs;
use tempfile::tempdir;

use support::{commit_file, git, open_grit_repo, HistoryRepo};

fn index_entry(path: &str, oid: ObjectId, mode: u32) -> IndexEntry {
    IndexEntry::new(path.as_bytes().to_vec(), oid, FileMode::new(mode))
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

    let _ = repo
        .revwalk()
        .merge_bases(&[tip, feature], grit_lib::revwalk::MergeBaseMode::All)?;
    let _ = repo.revwalk().merge_bases(
        &[tip, feature, old_main],
        grit_lib::revwalk::MergeBaseMode::Octopus,
    )?;
    let _ = repo.revwalk().independent(&[tip, feature, old_main])?;
    assert!(repo.revwalk().is_ancestor(old_main, tip)?);
    let main_base: ObjectId = git(wt, &["rev-parse", "main~30"])
        .trim()
        .parse()
        .expect("main base");
    let _ = repo.revwalk().fork_point("main", main_base, tip)?;
    let _ = repo.revwalk().ancestor_closure(tip)?;
    let counts = repo.revwalk().ahead_behind(
        tip,
        feature,
        &grit_lib::revwalk::AheadBehindOptions::default(),
    )?;
    let (ahead, behind) = (counts.ahead, counts.behind);
    let _ = (ahead, behind);
    let _ = repo
        .revwalk()
        .reachable_excluding(
            &[tip],
            &[old_main],
            &grit_lib::revwalk::ReachableWalkOptions::default().with_limit((None).map_or(
                grit_lib::revwalk::merge_base::ReachableWalkLimit::Unlimited,
                grit_lib::revwalk::merge_base::ReachableWalkLimit::StopAfter,
            )),
        )
        .map(|(_, commits)| commits)?;

    for n in 0..90 {
        for spec in [
            format!("HEAD~{n}"),
            format!("main~{n}"),
            format!("feature~{n}"),
            format!("HEAD~{n}^{{tree}}"),
            format!(":/add {n}"),
        ] {
            let _ = repo
                .revwalk()
                .resolve(
                    &spec,
                    &grit_lib::revwalk::ResolveOptions::default()
                        .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                        .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
                )
                .ok();
            let _ = repo
                .revwalk()
                .resolve(&spec, &grit_lib::revwalk::ResolveOptions::default())
                .ok();
            let _ = repo
                .revwalk()
                .resolve(
                    &spec,
                    &grit_lib::revwalk::ResolveOptions::default()
                        .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
                )
                .ok();
            let _ = repo
                .revwalk()
                .resolve(
                    &spec,
                    &grit_lib::revwalk::ResolveOptions::default()
                        .with_peel(grit_lib::revwalk::Peel::Commit),
                )
                .ok();
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
            let _ = repo.revwalk().rev_list(
                &[String::from("main"), String::from("feature")],
                &[],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )?;
            let _ = repo
                .revwalk()
                .merge_bases(&[tip, feature], grit_lib::revwalk::MergeBaseMode::All)?;
        }
    }

    let roots = repo.revwalk().rev_list(
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
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    assert!(!roots.commits.is_empty());
    let _ = repo
        .revwalk()
        .resolve_commits(&["main".to_owned(), "feature".to_owned()])?;

    let tree: ObjectId = git(wt, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .expect("tree");
    let parent_tree: ObjectId = git(wt, &["rev-parse", "HEAD~1^{tree}"])
        .trim()
        .parse()
        .expect("ptree");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent_tree), Some(&tree), &TreeDiffOptions::default())
        .map(|r| r.entries)?;
    let _ = repo
        .diff()
        .tree_to_tree(
            Some(&parent_tree),
            Some(&tree),
            &TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50)),
        )
        .map(|r| r.entries)?;
    let _ = repo
        .diff()
        .commits(
            &old_main,
            &tip,
            &TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50)),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .map(|r| r.entries)?;
    let cfg = ConfigSet::new();
    let policy = grit_lib::parallel::ThreadPolicy::default();
    let opts = TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(50))
        .with_copies(CopyDetection::FindHarder(50))
        .with_break_rewrites(BreakRewrites::Score(40));
    let _ = repo
        .diff()
        .detect_renames(raw.clone(), Some(&parent_tree), &opts)
        .map(|r| r.entries)?;

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
            let mut cpath = grit_lib::diff::combined_tree_diff::CombinedDiffPath::new(
                path.clone(),
                0o100644,
                blob_oid_at_path(repo.odb(), &parsed.tree, &path)
                    .unwrap_or_else(grit_lib::diff::zero_oid),
                vec![],
            );
            enrich_combined_path_renames(repo.odb(), &mut cpath, &parsed.parents, &parsed.tree, 50);
        }
    }

    Ok(())
}

#[test]
fn index_symlink_and_merge_base_diff_errors() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
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
    let _ = repo
        .diff()
        .index_to_tree(&index, Some(&head_tree), &TreeDiffOptions::default())
        .map(|r| r.entries)?;

    let same = repo
        .revwalk()
        .merge_base_for_diff(tip, tip)
        .expect("same commit merge base");
    assert_eq!(same, tip);

    let missing = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("hex");
    let err = repo
        .revwalk()
        .merge_base_for_diff(tip, missing)
        .expect_err("missing");
    assert!(matches!(
        err,
        MergeBaseForDiffError::None | MergeBaseForDiffError::Other(_)
    ));

    Ok(())
}
