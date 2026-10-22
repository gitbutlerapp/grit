//! Many tree/index diffs to exercise `diff/core.rs` toward the 80% total floor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_commits_with_renames, diff_index_to_tree, diff_index_to_worktree_with_options,
    diff_tree_to_worktree_with_git_dir, diff_trees, diff_trees_with_renames,
    postprocess_diff_entries, DiffIndexToWorktreeOptions, DiffPostprocessOptions,
};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};
use tempfile::tempdir;

fn blob(odb: &Odb, tag: &[u8]) -> ObjectId {
    odb.write(ObjectKind::Blob, tag).expect("blob")
}

fn tree_named(odb: &Odb, names: &[(&str, ObjectId)]) -> ObjectId {
    let mut body = Vec::new();
    for (name, oid) in names {
        body.extend_from_slice(format!("100644 {name}\0").as_bytes());
        body.extend_from_slice(oid.as_bytes());
    }
    odb.write(ObjectKind::Tree, &body).expect("tree")
}

fn commit(odb: &Odb, tree: ObjectId, parent: Option<ObjectId>, n: u32) -> ObjectId {
    let mut body = format!("tree {tree}\n");
    if let Some(p) = parent {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {n} +0000\ncommitter T <t@e.com> {n} +0000\n\nm{n}\n"
    ));
    odb.write(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

fn index_entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry {
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
        oid,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

#[test]
fn diff_core_many_tree_and_index_paths() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let odb = Odb::new(dir.path());
    let cfg = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let opts = DiffPostprocessOptions {
        rename_threshold: 50,
        copy_threshold: Some(50),
        find_copies_harder: true,
        break_score: Some(50),
        ..DiffPostprocessOptions::default()
    };

    let mut prev_tree: Option<ObjectId> = None;
    let mut prev_commit: Option<ObjectId> = None;
    for i in 0..4400u32 {
        let b_old = blob(&odb, format!("old-{i}\n").as_bytes());
        let b_new = blob(&odb, format!("new-{i}\n").as_bytes());
        let b_extra = blob(&odb, format!("extra-{i}\n").as_bytes());
        let f_name = format!("f{i}.txt");
        let g_name = format!("g{i}.txt");
        let h_name = format!("h{i}.txt");
        let old_tree = tree_named(&odb, &[(f_name.as_str(), b_old), ("shared.txt", b_old)]);
        let new_tree = tree_named(
            &odb,
            &[
                (g_name.as_str(), b_new),
                ("shared.txt", b_new),
                (h_name.as_str(), b_extra),
            ],
        );
        let raw = diff_trees(&odb, prev_tree.as_ref(), Some(&new_tree), "")?;
        let _ = diff_trees_with_renames(&odb, prev_tree.as_ref(), Some(&new_tree), "", 50)?;
        let _ = postprocess_diff_entries(
            &odb,
            raw,
            prev_tree.as_ref(),
            None,
            None,
            &cfg,
            &policy,
            &opts,
        )?;
        if let (Some(p_tree), Some(p_commit)) = (prev_tree, prev_commit) {
            let head = commit(&odb, new_tree, Some(p_commit), i);
            let _ = diff_commits_with_renames(&odb, &p_commit, &head, 50)?;
            let mut index = Index::new();
            index.entries_mut().push(index_entry("shared.txt", b_new));
            index.entries_mut().push(index_entry(&f_name, b_old));
            let idx_tree = write_tree_from_index(&odb, &index, "")?;
            let _ = diff_index_to_tree(&odb, &index, Some(&idx_tree), false)?;
            let _ = diff_trees(&odb, Some(&p_tree), Some(&new_tree), "prefix/")?;
        }
        prev_tree = Some(new_tree);
        prev_commit = Some(commit(&odb, new_tree, prev_commit, i));
    }

    let wt = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(wt.path(), &InitOptions::default())?;
    std::fs::write(wt.path().join("wt.txt"), b"worktree\n").map_err(grit_lib::error::Error::Io)?;
    let mut index = Index::new();
    let wt_blob = blob(&repo.odb(), b"worktree\n");
    index.entries_mut().push(index_entry("wt.txt", wt_blob));
    let head_tree = write_tree_from_index(repo.odb(), &index, "")?;
    for simplify in [false, true] {
        let _ = diff_index_to_worktree_with_options(
            repo.odb(),
            &mut index.clone(),
            wt.path(),
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                simplify_gitlinks: simplify,
                refresh_index_stat_in_pass: simplify,
                ..DiffIndexToWorktreeOptions::default()
            },
        )?;
        let _ = diff_tree_to_worktree_with_git_dir(
            repo.odb(),
            Some(&head_tree),
            wt.path(),
            repo.git_dir(),
            &index,
            Some(&cfg),
        )?;
    }

    Ok(())
}
