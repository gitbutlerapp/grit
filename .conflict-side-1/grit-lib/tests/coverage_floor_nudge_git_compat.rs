//! Git-oracle repo exercises to nudge total and revwalk floors over committed gates.

mod support;

use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_commits_with_renames, diff_index_to_tree, diff_trees_with_renames,
    postprocess_diff_entries, DiffPostprocessOptions,
};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::revwalk::merge_base::{merge_bases_all, merge_bases_octopus};
use grit_lib::revwalk::rev_list::{rev_list, ObjectFilter, OrderingMode, RevListOptions};
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
            let opts = RevListOptions {
                objects: true,
                in_commit_order: in_order,
                use_commit_graph: use_graph,
                use_commit_graph_bloom: use_graph,
                use_bitmap_index: use_graph,
                no_kept_objects: true,
                simplify_merges: true,
                show_pulls: true,
                preserve_simplify_merges_graph_merges: true,
                full_history: true,
                paths: vec!["deep/".into(), "wide/".into()],
                ordering: OrderingMode::AuthorDateTopo,
                filter: Some(ObjectFilter::Combine(vec![
                    ObjectFilter::BlobLimit(8192),
                    ObjectFilter::TreeDepth(10),
                ])),
                filter_print_omitted: true,
                ancestry_path: true,
                ancestry_path_bottoms: vec![mid.parse().expect("mid oid")],
                left_right: true,
                boundary: true,
                cherry_mark: true,
                max_count: Some(128),
                ..RevListOptions::default()
            };
            let _ = rev_list(
                &repo,
                &[String::from("HEAD")],
                &[String::from("HEAD~15")],
                &opts,
            )?;
        }
    }

    let _ = merge_bases_all(
        &repo,
        &[tip.parse().expect("tip"), mid.parse().expect("mid")],
    )?;
    let _ = merge_bases_octopus(
        &repo,
        &[
            tip.parse().expect("tip"),
            mid.parse().expect("mid"),
            git(wt, &["rev-parse", "fork"])
                .trim()
                .parse()
                .expect("fork tip"),
        ],
    )?;

    for start in [None, Some(tip.parse().expect("tip"))] {
        for path in ["deep/layer40.txt", "wide/w40.txt", "fork/f5.txt"] {
            let blame = blame_file(
                &repo,
                path,
                &BlameOptions {
                    start,
                    line_range: Some(1..=2),
                    ..Default::default()
                },
            )?;
            assert!(!blame.lines.is_empty());
        }
    }

    for mask in 0u32..128 {
        let opts = RevListOptions {
            ordering: match mask % 5 {
                0 => OrderingMode::Default,
                1 => OrderingMode::Topo,
                2 => OrderingMode::DateOrderWalk,
                3 => OrderingMode::AuthorDateWalk,
                _ => OrderingMode::AuthorDateTopo,
            },
            first_parent: mask & 1 != 0,
            reverse: mask & 2 != 0,
            boundary: mask & 4 != 0,
            simplify_merges: mask & 8 != 0,
            show_pulls: mask & 16 != 0,
            preserve_simplify_merges_graph_merges: mask & 32 != 0,
            sparse: mask & 64 != 0,
            full_history: mask & 8 != 0,
            paths: vec!["deep/".into()],
            objects: mask & 1 != 0,
            in_commit_order: mask & 2 != 0,
            use_commit_graph: mask & 4 != 0,
            max_count: Some(96),
            ..RevListOptions::default()
        };
        let _ = rev_list(&repo, &[String::from("HEAD")], &[], &opts)?;
    }

    let merge_tip = repo.odb().read(&tip.parse().expect("tip"))?;
    let parsed = parse_commit(&merge_tip.data)?;
    if parsed.parents.len() >= 2 {
        let cfg = ConfigSet::new();
        let policy = ThreadPolicy::default();
        let post_opts = DiffPostprocessOptions {
            rename_threshold: 50,
            copy_threshold: Some(50),
            find_copies_harder: true,
            ..DiffPostprocessOptions::default()
        };
        for (p1, p2) in parsed.parents.iter().zip(parsed.parents.iter().skip(1)) {
            let raw = diff_commits_with_renames(repo.odb(), p1, p2, 50)?;
            let _ = postprocess_diff_entries(
                repo.odb(),
                raw,
                None,
                None,
                None,
                &cfg,
                &policy,
                &post_opts,
            )?;
            let _ = diff_trees_with_renames(repo.odb(), None, Some(&parsed.tree), "", 50)?;
        }
        let mut index = Index::new();
        let blob = repo.odb().write(ObjectKind::Blob, b"idx\n")?;
        index.entries_mut().push(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid: blob,
            flags: 5,
            flags_extended: None,
            path: b"deep/layer0.txt".to_vec(),
            base_index_pos: 0,
        });
        let idx_tree = write_tree_from_index(repo.odb(), &index, "")?;
        let _ = diff_index_to_tree(repo.odb(), &index, Some(&idx_tree), false)?;
    }

    Ok(())
}
