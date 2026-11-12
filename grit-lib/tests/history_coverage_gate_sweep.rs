//! Integration sweeps targeting history aggregate (`diff`, `revwalk`, `revparse`) coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::FileMode;
use grit_lib::progress::{CancelToken, NullProgress};

use grit_lib::diff::{CopyDetection, DiffStatus, RenameDetection, TreeDiffOptions};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{OrderingMode, RevListOptions};
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
    IndexEntry::new(path.as_bytes().to_vec(), oid, FileMode::REGULAR)
}

#[test]
fn history_walk_diff_and_resolve_sweep() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
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
            let _ = repo.revwalk().resolve(
                &spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                    .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
            )?;
            let _ = repo
                .revwalk()
                .resolve(&spec, &grit_lib::revwalk::ResolveOptions::default())?;
            let _ = repo.revwalk().resolve(
                &spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
            )?;
        }
        for spec in [format!("HEAD~{n}"), format!("main~{n}"), format!(":/c{n}")] {
            let _ = repo.revwalk().resolve(
                &spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_peel(grit_lib::revwalk::Peel::Commit),
            )?;
        }
    }
    let _ = repo.revwalk().resolve(
        "main~10..main",
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Commit),
    )?;
    let _ = repo
        .revwalk()
        .merge_bases(&[tip, tip], grit_lib::revwalk::MergeBaseMode::All)?;

    let orderings = [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
    ];
    for ordering in orderings {
        let opts = RevListOptions::default()
            .with_ordering(ordering)
            .with_paths(vec!["tracked.txt".to_owned()])
            .with_left_right(true)
            .with_boundary(true)
            .with_reverse(true)
            .with_max_count(Some(20));
        let out = repo.revwalk().rev_list(
            &[String::from("main")],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )?;
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
    let diff = repo.diff();
    let raw = diff
        .tree_to_tree(
            Some(&old_tree),
            Some(&new_tree),
            &TreeDiffOptions::default(),
        )?
        .entries;
    assert!(raw.iter().any(|e| e.status == DiffStatus::Modified));
    let renames = TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50));
    let _ = diff.tree_to_tree(Some(&old_tree), Some(&new_tree), &renames)?;
    let _ = diff.commits(&base, &head, &renames)?;
    let copies = renames
        .with_copies(CopyDetection::FindHarder(50))
        .with_threads(Some(ThreadPolicy::default()));
    let _ = diff.tree_to_tree(Some(&old_tree), Some(&new_tree), &copies)?;

    let mut index = Index::new();
    index.entries_mut().push(index_entry("tracked.txt", blob_b));
    index.write_to_path(&repo.git_dir().join("index"), false)?;
    let head_tree = write_tree_from_index(repo.odb(), &index, "")?;
    let _ = diff.index_to_tree(&index, Some(&head_tree), &TreeDiffOptions::default())?;
    let _ = diff.index_to_worktree(&mut index.clone(), &TreeDiffOptions::default())?;
    let _ = diff.tree_to_worktree(Some(&head_tree), &index, &TreeDiffOptions::default())?;
    Ok(())
}
