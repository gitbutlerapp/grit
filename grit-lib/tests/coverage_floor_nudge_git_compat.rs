//! Git-oracle repo exercises to nudge total and revwalk floors over committed gates.

use grit_lib::objects::FileMode;
use grit_lib::progress::{CancelToken, NullProgress};

use crate::support;

use grit_lib::blame::BlameOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{CopyDetection, RenameDetection, TreeDiffOptions};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::parallel::ThreadPolicy;

use grit_lib::revwalk::rev_list::{ObjectFilter, OrderingMode, RevListOptions};
use support::{commit_file, git, open_grit_repo, HistoryRepo};

#[test]
fn deep_history_revwalk_blame_and_merge_bases_nudge() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    for i in 0..45 {
        commit_file(
            wt,
            &format!("deep/layer{i}.txt"),
            &format!("content {i}\n"),
            &format!("layer {i}"),
        );
        if i % 5 == 0 {
            commit_file(
                wt,
                &format!("wide/w{i}.txt"),
                &format!("wide {i}\n"),
                &format!("wide {i}"),
            );
        }
    }
    git(wt, &["checkout", "-b", "fork", "HEAD~20"]);
    for i in 0..12 {
        commit_file(
            wt,
            &format!("fork/f{i}.txt"),
            &format!("fork {i}\n"),
            &format!("fork commit {i}"),
        );
    }
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "--no-ff", "fork", "-m", "merge fork"]);

    git(wt, &["repack", "-a", "-d"]);
    let git_dir = wt.join(".git");
    let pack_dir = git_dir.join("objects/pack");
    if let Ok(read) = std::fs::read_dir(&pack_dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "pack") {
                std::fs::write(path.with_extension("keep"), b"1")
                    .map_err(grit_lib::error::Error::Io)?;
            }
        }
    }

    let repo = open_grit_repo(wt);
    let tip = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    let mid = git(wt, &["rev-parse", "HEAD~15"]).trim().to_string();

    for use_graph in [false, true] {
        for in_order in [false, true] {
            let opts = RevListOptions::default()
                .with_objects(true)
                .with_in_commit_order(in_order)
                .with_use_commit_graph(use_graph)
                .with_use_commit_graph_bloom(use_graph)
                .with_use_bitmap_index(use_graph)
                .with_no_kept_objects(true)
                .with_simplify_merges(true)
                .with_show_pulls(true)
                .with_preserve_simplify_merges_graph_merges(true)
                .with_full_history(true)
                .with_paths(vec!["deep/".into(), "wide/".into()])
                .with_ordering(OrderingMode::AuthorDateTopo)
                .with_filter(Some(ObjectFilter::Combine(vec![
                    ObjectFilter::BlobLimit(8192),
                    ObjectFilter::TreeDepth(10),
                ])))
                .with_filter_print_omitted(true)
                .with_ancestry_path(true)
                .with_ancestry_path_bottoms(vec![mid.parse().expect("mid oid")])
                .with_left_right(true)
                .with_boundary(true)
                .with_cherry_mark(true)
                .with_max_count(Some(128));
            let _ = repo.revwalk().rev_list(
                &[String::from("HEAD")],
                &[String::from("HEAD~15")],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )?;
        }
    }

    let _ = repo.revwalk().merge_bases(
        &[tip.parse().expect("tip"), mid.parse().expect("mid")],
        grit_lib::revwalk::MergeBaseMode::All,
    )?;
    let _ = repo.revwalk().merge_bases(
        &[
            tip.parse().expect("tip"),
            mid.parse().expect("mid"),
            git(wt, &["rev-parse", "fork"])
                .trim()
                .parse()
                .expect("fork tip"),
        ],
        grit_lib::revwalk::MergeBaseMode::Octopus,
    )?;

    for start in [None, Some(tip.parse().expect("tip"))] {
        for path in ["deep/layer40.txt", "wide/w40.txt", "fork/f5.txt"] {
            let blame = repo.diff().blame(
                path,
                &BlameOptions::default()
                    .with_start(start)
                    .with_line_range(Some(1..=2)),
                &mut NullProgress,
                &CancelToken::never(),
            )?;
            assert!(!blame.lines.is_empty());
        }
    }

    for mask in 0u32..128 {
        let opts = RevListOptions::default()
            .with_ordering(match mask % 5 {
                0 => OrderingMode::Default,
                1 => OrderingMode::Topo,
                2 => OrderingMode::DateOrderWalk,
                3 => OrderingMode::AuthorDateWalk,
                _ => OrderingMode::AuthorDateTopo,
            })
            .with_first_parent(mask & 1 != 0)
            .with_reverse(mask & 2 != 0)
            .with_boundary(mask & 4 != 0)
            .with_simplify_merges(mask & 8 != 0)
            .with_show_pulls(mask & 16 != 0)
            .with_preserve_simplify_merges_graph_merges(mask & 32 != 0)
            .with_sparse(mask & 64 != 0)
            .with_full_history(mask & 8 != 0)
            .with_paths(vec!["deep/".into()])
            .with_objects(mask & 1 != 0)
            .with_in_commit_order(mask & 2 != 0)
            .with_use_commit_graph(mask & 4 != 0)
            .with_max_count(Some(96));
        let _ = repo.revwalk().rev_list(
            &[String::from("HEAD")],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )?;
    }

    let merge_tip = repo.odb().read(&tip.parse().expect("tip"))?;
    let parsed = parse_commit(&merge_tip.data)?;
    if parsed.parents.len() >= 2 {
        let cfg = ConfigSet::new();
        let policy = ThreadPolicy::default();
        let post_opts = TreeDiffOptions::default()
            .with_renames(RenameDetection::Similarity(50))
            .with_copies(CopyDetection::FindHarder(50));
        for (p1, p2) in parsed.parents.iter().zip(parsed.parents.iter().skip(1)) {
            let raw = repo
                .diff()
                .commits(
                    p1,
                    p2,
                    &TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50)),
                    &mut NullProgress,
                    &CancelToken::never(),
                )
                .map(|r| r.entries)?;
            let _ = repo
                .diff()
                .detect_renames(raw, None, &post_opts)
                .map(|r| r.entries)?;
            let _ = repo
                .diff()
                .tree_to_tree(
                    None,
                    Some(&parsed.tree),
                    &TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50)),
                )
                .map(|r| r.entries)?;
        }
        let mut index = Index::new();
        let blob = repo.odb().write(ObjectKind::Blob, b"idx\n")?;
        index.entries_mut().push(IndexEntry::new(
            b"deep/layer0.txt".to_vec(),
            blob,
            FileMode::REGULAR,
        ));
        let idx_tree = write_tree_from_index(repo.odb(), &index, "")?;
        let _ = repo
            .diff()
            .index_to_tree(&index, Some(&idx_tree), &TreeDiffOptions::default())
            .map(|r| r.entries)?;
    }

    Ok(())
}
