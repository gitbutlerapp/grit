//! Public custom [`Odb`](grit_lib::odb::Odb) backends via [`OdbBuilder`](grit_lib::odb::OdbBuilder).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::diff::diff_trees;
use grit_lib::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::odb::store::MemoryStore;
use grit_lib::odb::OdbBuilder;
use grit_lib::refs::write_ref;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::rev_list::{rev_list, RevListOptions};

fn count_loose_hex_dirs(objects: &Path) -> usize {
    let Ok(entries) = fs::read_dir(objects) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.len() == 2 && n.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .count()
}

fn memory_repo() -> (tempfile::TempDir, Repository, Arc<MemoryStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    let objects = git_dir.join("objects");
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let builder = OdbBuilder::files(&objects)
        .primary(store.clone())
        .alternates(false);
    let repo = Repository::open_with_odb(
        &RepositoryOptions::empty(),
        &git_dir,
        Some(dir.path()),
        builder,
    )
    .expect("open");
    (dir, repo, store)
}

#[test]
fn memory_primary_revwalk_diff_without_loose_objects() {
    let (_dir, repo, _store) = memory_repo();

    let blob = repo
        .odb
        .write(ObjectKind::Blob, b"payload\n")
        .expect("blob");
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"f".to_vec(),
        oid: blob,
    }]);
    let tree = repo.odb.write(ObjectKind::Tree, &tree_body).expect("tree");
    let commit_body = format!(
        "tree {tree}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nmsg\n"
    );
    let commit = repo
        .odb
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    write_ref(&repo.git_dir, "refs/heads/main", &commit).expect("ref");

    let tip = commit.to_hex();
    let listed =
        rev_list(&repo, &[tip.clone()], &[], &RevListOptions::default()).expect("rev-list");
    assert_eq!(listed.commits.len(), 1);
    assert_eq!(listed.commits[0], commit);

    let blob2 = repo.odb.write(ObjectKind::Blob, b"other\n").expect("blob2");
    let tree2_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"g".to_vec(),
        oid: blob2,
    }]);
    let tree2 = repo
        .odb
        .write(ObjectKind::Tree, &tree2_body)
        .expect("tree2");
    let diff = diff_trees(&repo.odb, Some(&tree2), Some(&tree), "").expect("diff");
    assert!(!diff.is_empty());

    assert_eq!(
        count_loose_hex_dirs(repo.odb.objects_dir()),
        0,
        "custom primary must not create loose object shards"
    );
}

#[test]
fn memory_primary_rejects_filesystem_gc_and_commit_graph() {
    let (_dir, repo, _store) = memory_repo();
    assert!(matches!(
        repo.odb.gc(),
        Err(Error::UnsupportedObjectStore { operation: "gc" })
    ));
    assert!(matches!(
        repo.odb.write_commit_graph(),
        Err(Error::UnsupportedObjectStore {
            operation: "commit-graph write"
        })
    ));
}
