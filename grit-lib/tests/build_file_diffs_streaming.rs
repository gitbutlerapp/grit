//! [`build_file_diffs_streaming`] matches [`build_file_diffs`] and propagates callback errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::diff::{
    build_file_diffs, build_file_diffs_streaming, diff_trees, DiffDetail, DiffOptions,
};
use grit_lib::error::Error;
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn tree_with_files(repo: &Repository, version: &str) -> grit_lib::objects::ObjectId {
    let odb = repo.odb();
    let entries: Vec<TreeEntry> = (0..8)
        .map(|i| {
            let oid = odb
                .write(
                    ObjectKind::Blob,
                    format!("{version} file {i}\nline two\n").as_bytes(),
                )
                .expect("blob");
            TreeEntry {
                mode: 0o100644,
                name: format!("f/{i:02}.txt").into_bytes(),
                oid,
            }
        })
        .collect();
    odb.write(ObjectKind::Tree, &serialize_tree(&entries))
        .expect("tree")
}

#[test]
fn streaming_patch_rows_match_build_file_diffs() {
    let (_dir, repo) = init_repo();
    let old_tree = tree_with_files(&repo, "old");
    let new_tree = tree_with_files(&repo, "new");
    let entries = diff_trees(repo.odb(), Some(&old_tree), Some(&new_tree), "").expect("diff");
    let config = repo.config().expect("config");
    let thread_policy = ThreadPolicy::default();
    let opts = DiffOptions::default();

    let bulk = build_file_diffs(
        repo.odb(),
        entries.clone(),
        DiffDetail::Patch,
        opts.clone(),
        config.as_ref(),
        &thread_policy,
    )
    .expect("bulk");

    let mut streamed = Vec::new();
    build_file_diffs_streaming(
        repo.odb(),
        entries,
        opts,
        config.as_ref(),
        &thread_policy,
        |row| {
            streamed.push(row);
            Ok(())
        },
    )
    .expect("stream");

    assert_eq!(streamed.len(), bulk.files.len());
    for (a, b) in streamed.iter().zip(bulk.files.iter()) {
        assert_eq!(a.path, b.path);
        assert_eq!(a.status, b.status);
        assert_eq!(a.structured, b.structured);
    }
}

#[test]
fn streaming_propagates_callback_errors() {
    let (_dir, repo) = init_repo();
    let old_tree = tree_with_files(&repo, "a");
    let new_tree = tree_with_files(&repo, "b");
    let entries = diff_trees(repo.odb(), Some(&old_tree), Some(&new_tree), "").expect("diff");
    let config = repo.config().expect("config");
    let thread_policy = ThreadPolicy::default();
    let err = build_file_diffs_streaming(
        repo.odb(),
        entries,
        DiffOptions::default(),
        config.as_ref(),
        &thread_policy,
        |_| Err(Error::Cancelled),
    )
    .expect_err("callback error");
    assert!(matches!(err, Error::Cancelled));
}
