//! Broad `rev_list` / `diff` API sweeps on synthetic repos (coverage-oriented).

use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_commits, diff_trees, diff_trees_with_renames, postprocess_diff_entries,
    DiffPostprocessOptions, DiffStatus,
};
use grit_lib::error::Error;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::ref_storage::RefStorageFormat;
use grit_lib::repo::{init_bare_clone_minimal, Repository};
use grit_lib::rev_list::{
    merge_bases, resolve_object_walk_roots, rev_list, OrderingMode, OutputMode, RevListOptions,
};
use std::fs;
use tempfile::tempdir;

fn write_tree(odb: &Odb, name: &str, blob: ObjectId) -> grit_lib::error::Result<ObjectId> {
    let mut body = format!("100644 {name}\0").into_bytes();
    body.extend_from_slice(blob.as_bytes());
    odb.write_loose_materialize(ObjectKind::Tree, &body)
}

fn write_commit(
    odb: &Odb,
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

fn open_repo() -> grit_lib::error::Result<(tempfile::TempDir, Repository)> {
    let dir = tempdir().map_err(Error::Io)?;
    let path = dir.path().to_path_buf();
    init_bare_clone_minimal(&path, "main", RefStorageFormat::Files)?;
    Ok((dir, Repository::open(&path, None)?))
}

#[test]
fn symmetric_range_ancestry_and_object_roots() -> grit_lib::error::Result<()> {
    let (_dir, repo) = open_repo()?;
    let b1 = repo.odb.write_loose_materialize(ObjectKind::Blob, b"a\n")?;
    let b2 = repo.odb.write_loose_materialize(ObjectKind::Blob, b"b\n")?;
    let t1 = write_tree(&repo.odb, "f.txt", b1)?;
    let t2 = write_tree(&repo.odb, "f.txt", b2)?;
    let base = write_commit(&repo.odb, t1, &[], "base")?;
    let tip = write_commit(&repo.odb, t2, &[base], "tip")?;
    fs::write(repo.git_dir.join("refs/heads/main"), format!("{tip}\n")).map_err(Error::Io)?;
    fs::write(
        repo.git_dir.join("refs/heads/ancestor"),
        format!("{base}\n"),
    )
    .map_err(Error::Io)?;

    let opts = RevListOptions {
        ordering: OrderingMode::AuthorDateWalk,
        output_mode: OutputMode::Format(String::from("oneline")),
        objects: true,
        in_commit_order: true,
        ..RevListOptions::default()
    };
    let out = rev_list(&repo, &[String::from("main")], &[], &opts)?;
    assert_eq!(out.commits.len(), 2);

    let roots = resolve_object_walk_roots(&repo, &[String::from("main")])?;
    assert!(!roots.0.is_empty());
    Ok(())
}

#[test]
fn diff_postprocess_across_commit_pair() -> grit_lib::error::Result<()> {
    let (_dir, repo) = open_repo()?;
    let unchanged = repo
        .odb
        .write_loose_materialize(ObjectKind::Blob, b"same\n")?;
    let old_only = repo
        .odb
        .write_loose_materialize(ObjectKind::Blob, b"removed\n")?;
    let new_only = repo
        .odb
        .write_loose_materialize(ObjectKind::Blob, b"added line\n")?;
    let old_tree = {
        let mut body = Vec::new();
        for (n, oid) in [("keep.txt", unchanged), ("gone.txt", old_only)] {
            body.extend_from_slice(format!("100644 {n}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        repo.odb.write_loose_materialize(ObjectKind::Tree, &body)?
    };
    let new_tree = {
        let mut body = Vec::new();
        for (n, oid) in [("keep.txt", unchanged), ("fresh.txt", new_only)] {
            body.extend_from_slice(format!("100644 {n}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        repo.odb.write_loose_materialize(ObjectKind::Tree, &body)?
    };
    let base = write_commit(&repo.odb, old_tree, &[], "base")?;
    let tip = write_commit(&repo.odb, new_tree, &[base], "tip")?;

    let raw = diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "")?;
    assert!(raw.iter().any(|e| e.status == DiffStatus::Deleted));
    let renamed = diff_trees_with_renames(&repo.odb, Some(&old_tree), Some(&new_tree), "", 30)?;
    assert!(!renamed.is_empty());

    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let opts = DiffPostprocessOptions {
        rename_threshold: 30,
        copy_threshold: Some(30),
        find_copies_harder: true,
        ..DiffPostprocessOptions::default()
    };
    let processed = postprocess_diff_entries(
        &repo.odb,
        raw,
        Some(&old_tree),
        None,
        &config,
        &policy,
        &opts,
    )?;
    assert!(!processed.is_empty());

    let via_commits = diff_commits(&repo.odb, &base, &tip)?;
    assert!(!via_commits.is_empty());
    let _ = merge_bases(&repo, tip, base, false)?;
    Ok(())
}
