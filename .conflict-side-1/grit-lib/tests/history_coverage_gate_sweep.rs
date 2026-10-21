//! Integration sweeps targeting history aggregate (`diff`, `revwalk`, `revparse`) coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_commits_with_renames, diff_index_to_tree, diff_index_to_worktree_with_options,
    diff_tree_to_worktree_with_git_dir, diff_trees, diff_trees_with_renames,
    postprocess_diff_entries, DiffIndexToWorktreeOptions, DiffPostprocessOptions, DiffStatus,
};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{
    merge_bases, resolve_object_walk_roots, rev_list, OrderingMode, OutputMode, RevListOptions,
};
use grit_lib::revwalk::rev_parse::{
    resolve_revision, resolve_revision_as_commit, resolve_revision_for_range_end,
    resolve_revision_for_verify, try_parse_double_dot_log_range,
};
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

fn write_commit(
    odb: &grit_lib::objects::odb::Odb,
    tree: ObjectId,
    parents: &[ObjectId],
    msg: &str,
) -> grit_lib::error::Result<ObjectId> {
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
    body.push_str(msg);
    body.push('\n');
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
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
fn history_walk_diff_and_resolve_sweep() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let mut tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"v0\n")?;
    let mut parent: Option<ObjectId> = None;
    for i in 0..160 {
        let blob = repo
            .odb()
            .write_loose_materialize(ObjectKind::Blob, format!("v{i}\n").as_bytes())?;
        let tree = write_tree(repo.odb(), "tracked.txt", blob)?;
        let mut body = format!("tree {tree}\n");
        if let Some(p) = parent {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {} +0000\ncommitter T <t@e.com> {} +0000\n\nc{i}\n",
            100 + i,
            100 + i
        ));
        tip = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())?;
        parent = Some(tip);
        fs::write(dir.path().join("tracked.txt"), format!("v{i}\n").as_bytes())
            .map_err(grit_lib::error::Error::Io)?;
    }
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
        .map_err(grit_lib::error::Error::Io)?;

    for n in 0..80 {
        for spec in [
            format!("HEAD~{n}"),
            format!("main~{n}"),
            format!("HEAD~{n}^{{tree}}"),
            format!("HEAD~{n}^{{commit}}"),
            format!(":/c{n}"),
        ] {
            let _ = resolve_revision_for_verify(&repo, &spec)?;
            let _ = resolve_revision(&repo, &spec)?;
            let _ = resolve_revision_for_range_end(&repo, &spec)?;
        }
        for spec in [format!("HEAD~{n}"), format!("main~{n}"), format!(":/c{n}")] {
            let _ = resolve_revision_as_commit(&repo, &spec)?;
        }
    }
    let _ = try_parse_double_dot_log_range(&repo, "main~10..main")?;
    let _ = merge_bases(&repo, tip, tip, false)?;
    let roots = resolve_object_walk_roots(&repo, &[String::from("main")])?;
    assert!(!roots.0.is_empty());

    let orderings = [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
    ];
    for ordering in orderings {
        let opts = RevListOptions {
            ordering,
            output_mode: OutputMode::Format(String::from("oneline")),
            paths: vec!["tracked.txt".to_owned()],
            cherry_pick: false,
            left_right: true,
            boundary: true,
            reverse: true,
            max_count: Some(20),
            ..RevListOptions::default()
        };
        let out = rev_list(&repo, &[String::from("main")], &[], &opts)?;
        assert!(!out.commits.is_empty());
    }

    let blob_a = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"a\n")?;
    let blob_b = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"b\n")?;
    let old_tree = write_tree(repo.odb(), "other.txt", blob_a)?;
    let new_tree = write_tree(repo.odb(), "other.txt", blob_b)?;
    let base = write_commit(repo.odb(), old_tree, &[], "base")?;
    let head = write_commit(repo.odb(), new_tree, &[base], "head")?;
    let raw = diff_trees(repo.odb(), Some(&old_tree), Some(&new_tree), "")?;
    assert!(raw.iter().any(|e| e.status == DiffStatus::Modified));
    let _ = diff_trees_with_renames(repo.odb(), Some(&old_tree), Some(&new_tree), "", 50)?;
    let _ = diff_commits_with_renames(repo.odb(), &base, &head, 50)?;
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let opts = DiffPostprocessOptions {
        rename_threshold: 50,
        copy_threshold: Some(50),
        find_copies_harder: true,
        ..DiffPostprocessOptions::default()
    };
    let _ = postprocess_diff_entries(
        repo.odb(),
        raw,
        Some(&old_tree),
        None,
        None,
        &config,
        &policy,
        &opts,
    )?;

    let mut index = Index::new();
    index.entries_mut().push(index_entry("tracked.txt", blob_b));
    index.write_to_path(&repo.git_dir().join("index"), false)?;
    let head_tree = write_tree_from_index(repo.odb(), &index, "")?;
    let _ = diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false)?;
    let _ = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index.clone(),
        dir.path(),
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )?;
    let _ = diff_tree_to_worktree_with_git_dir(
        repo.odb(),
        Some(&head_tree),
        dir.path(),
        repo.git_dir(),
        &index,
        Some(&config),
    )?;
    Ok(())
}
