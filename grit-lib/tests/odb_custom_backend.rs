//! Public custom [`Odb`](grit_lib::odb::Odb) backends via [`OdbBuilder`](grit_lib::odb::OdbBuilder).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::diff::diff_trees;
use grit_lib::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::gc::prune_loose_unreachable;
use grit_lib::objects::{serialize_tag, TagData};
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::odb::store::{MemoryStore, ObjectStore};
use grit_lib::odb::OdbBuilder;
use grit_lib::refs::write_ref;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_lib::rev_parse::{abbreviate_object_id, resolve_revision};
use std::process::Command;

fn canonical_store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{kind} {}\0", data.len());
    let mut store_bytes = header.into_bytes();
    store_bytes.extend_from_slice(data);
    store_bytes
}

fn list_files_under(base: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in read.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                paths.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    paths.sort();
    paths
}

fn git_fsck_full(repo_root: &Path) {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["fsck", "--full"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --full: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn memory_repo() -> (tempfile::TempDir, Repository, Arc<MemoryStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
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
    let objects_snapshot_before = list_files_under(repo.odb.objects_dir());

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
        list_files_under(repo.odb.objects_dir()),
        objects_snapshot_before,
        "custom primary must not mutate the on-disk objects/ tree"
    );
}

#[test]
fn memory_primary_resolve_abbrev_via_lookup_prefix() {
    let (_dir, repo, _store) = memory_repo();
    let blob = repo
        .odb
        .write(ObjectKind::Blob, b"abbrev-test\n")
        .expect("blob");
    let prefix = &blob.to_hex()[..8];
    let resolved = resolve_revision(&repo, prefix).expect("abbrev");
    assert_eq!(resolved, blob);
}

#[test]
fn memory_primary_write_raw_ignores_seeded_loose_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let objects = git_dir.join("objects");
    let payload = b"seed-on-disk-only\n";
    let store_bytes = canonical_store_bytes(ObjectKind::Blob, payload);
    let oid =
        grit_lib::hash::hash_object(grit_lib::objects::HashAlgo::Sha1, ObjectKind::Blob, payload);
    let loose = objects.join(oid.loose_prefix()).join(oid.loose_suffix());
    fs::create_dir_all(loose.parent().unwrap()).expect("shard");
    fs::write(&loose, b"placeholder").expect("seed loose");

    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let repo = Repository::open_with_odb(
        &RepositoryOptions::empty(),
        &git_dir,
        Some(dir.path()),
        OdbBuilder::files(&objects)
            .primary(store.clone())
            .alternates(false),
    )
    .expect("open");

    assert!(!store.contains(&oid).expect("contains"));
    repo.odb.write_raw(&store_bytes).expect("write_raw");
    assert!(store.contains(&oid).expect("contains after"));
}

#[test]
fn files_primary_gc_prunes_unreachable_loose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let kept_blob = repo.odb.write(ObjectKind::Blob, b"keep\n").expect("keep");
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"f".to_vec(),
        oid: kept_blob,
    }]);
    let tree = repo.odb.write(ObjectKind::Tree, &tree_body).expect("tree");
    let commit_body = format!(
        "tree {tree}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nm\n"
    );
    let commit = repo
        .odb
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    write_ref(&repo.git_dir, "refs/heads/main", &commit).expect("ref");

    let orphan_payload = b"orphan\n";
    let orphan_oid = grit_lib::hash::hash_object(
        grit_lib::objects::HashAlgo::Sha1,
        ObjectKind::Blob,
        orphan_payload,
    );
    let orphan_path = repo.odb.object_path(&orphan_oid);
    fs::create_dir_all(orphan_path.parent().unwrap()).expect("shard");
    fs::write(&orphan_path, b"x").expect("orphan loose");

    repo.odb.gc().expect("gc");
    assert!(
        !orphan_path.exists(),
        "gc must prune unreachable loose objects"
    );
    assert!(repo.odb.read(&kept_blob).is_ok());
    let stats = prune_loose_unreachable(&repo.odb, &[commit], None).expect("verify prune");
    assert_eq!(stats.pruned, 0, "gc should have already pruned orphans");
}

#[test]
fn memory_primary_abbrev_unique_prefix_considers_store_collisions() {
    use grit_lib::objects::ObjectId;
    use std::collections::HashMap;

    let (_dir, repo, _store) = memory_repo();
    let mut by_prefix: HashMap<String, ObjectId> = HashMap::new();
    let mut pair = None;
    for i in 0..100_000u32 {
        let oid = repo
            .odb
            .write(ObjectKind::Blob, format!("collide-{i}\n").as_bytes())
            .expect("blob");
        let prefix4 = oid.to_hex()[..4].to_owned();
        if let Some(other) = by_prefix.insert(prefix4.clone(), oid) {
            if other != oid {
                pair = Some((other, oid));
                break;
            }
        }
    }
    let (first, second) = pair.expect("find blob pair with shared 4-char prefix");
    assert_ne!(first, second);
    assert_eq!(first.to_hex()[..4], second.to_hex()[..4]);
    let abbrev = abbreviate_object_id(&repo, first, 4).expect("abbrev");
    assert_ne!(
        abbrev.len(),
        4,
        "shared prefix {second} must force longer abbreviation"
    );
    assert!(first.to_hex().starts_with(&abbrev));
}

#[test]
fn gc_keeps_blob_tree_and_annotated_tag_ref_targets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");

    let blob = repo
        .odb
        .write(ObjectKind::Blob, b"tagged-blob\n")
        .expect("blob");
    write_ref(&repo.git_dir, "refs/tags/blob-tip", &blob).expect("blob ref");

    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"t".to_vec(),
        oid: blob,
    }]);
    let tree = repo.odb.write(ObjectKind::Tree, &tree_body).expect("tree");
    write_ref(&repo.git_dir, "refs/tags/tree-tip", &tree).expect("tree ref");

    let tag_bytes = serialize_tag(&TagData {
        object: blob,
        object_type: "blob".into(),
        tag: "v1".into(),
        tagger: Some("T <t@example.com> 1 +0000".into()),
        message: "annotated\n".into(),
    });
    let tag_oid = repo
        .odb
        .write(ObjectKind::Tag, &tag_bytes)
        .expect("tag object");
    write_ref(&repo.git_dir, "refs/tags/annotated", &tag_oid).expect("tag ref");

    repo.odb.gc().expect("gc");

    assert!(repo.odb.read(&blob).is_ok());
    assert!(repo.odb.read(&tree).is_ok());
    assert!(repo.odb.read(&tag_oid).is_ok());
    git_fsck_full(dir.path());
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
