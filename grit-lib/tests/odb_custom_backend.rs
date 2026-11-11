//! Public custom [`Odb`](grit_lib::objects::odb::Odb) backends via
//! [`RepositoryOptions::with_object_store`].

use grit_lib::progress::{CancelToken, NullProgress};

use grit_lib::diff::TreeDiffOptions;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{serialize_tag, TagData};
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::refs::gc::prune_loose_unreachable;
use grit_lib::repo::error::FilesOnlyOperation;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};
use grit_lib::revwalk::rev_list::RevListOptions;

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
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(dir.path())),
    )
    .expect("open");
    (dir, repo, store)
}

/// `tmp/real` holds an initialized repository and `tmp/link` is a symlink to it, like macOS
/// `/var` -> `/private/var`.
#[cfg(unix)]
fn symlinked_repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    fs::create_dir(&real).expect("mkdir real");
    Repository::init(&real, &InitOptions::default()).expect("init");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    (tmp, real, link)
}

#[cfg(unix)]
fn open_with_memory_primary(
    options: RepositoryOptions,
    git_dir: &Path,
    work_tree: &Path,
) -> grit_lib::error::Result<Repository> {
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    Repository::open(
        git_dir,
        &(options.with_object_store(store).with_work_tree(work_tree)),
    )
}

#[cfg(unix)]
fn assert_blob_round_trip(repo: &Repository) {
    let oid = repo.odb().write(ObjectKind::Blob, b"hello").expect("write");
    let obj = repo.odb().read(&oid).expect("read");
    assert_eq!(obj.data, b"hello");
}

#[cfg(unix)]
#[test]
fn custom_primary_accepts_objects_dir_through_symlink() {
    let (_tmp, real, link) = symlinked_repo();
    let git_dir = link.join(".git");
    let repo = open_with_memory_primary(RepositoryOptions::empty(), &git_dir, &link)
        .expect("open through symlink");
    let canonical_objects = real.join(".git/objects").canonicalize().expect("canon");
    assert_eq!(repo.odb().objects_dir(), canonical_objects);
    assert_eq!(repo.odb().objects_dir(), repo.git_dir().join("objects"));
    assert_blob_round_trip(&repo);
}

#[cfg(unix)]
#[test]
fn custom_primary_accepts_symlinked_objects_dir() {
    let (_tmp, real, _link) = symlinked_repo();
    let git_dir = real.join(".git");
    let backing = git_dir.join("objects-backing");
    fs::rename(git_dir.join("objects"), &backing).expect("move objects");
    std::os::unix::fs::symlink(&backing, git_dir.join("objects")).expect("symlink objects");
    let canonical_backing = backing.canonicalize().expect("canon");

    let repo = open_with_memory_primary(RepositoryOptions::empty(), &git_dir, &real)
        .expect("open with symlinked objects dir");
    assert_eq!(repo.odb().objects_dir(), canonical_backing);
    assert_blob_round_trip(&repo);
}

#[cfg(unix)]
#[test]
fn custom_primary_accepts_mixed_symlink_and_real_paths() {
    let (_tmp, real, link) = symlinked_repo();
    let via_link = open_with_memory_primary(RepositoryOptions::empty(), &link.join(".git"), &real)
        .expect("git dir via symlink, work tree via real path");
    assert_blob_round_trip(&via_link);

    let via_real = open_with_memory_primary(RepositoryOptions::empty(), &real.join(".git"), &link)
        .expect("git dir via real path, work tree via symlink");
    assert_blob_round_trip(&via_real);
    assert_eq!(via_link.odb().objects_dir(), via_real.odb().objects_dir());
}

#[cfg(unix)]
#[test]
fn custom_primary_resolves_relative_paths_against_discovery_cwd() {
    let (_tmp, real, link) = symlinked_repo();
    let mut environment = grit_lib::config::environment::Environment::empty();
    environment.cwd = link.clone();
    let repo = open_with_memory_primary(
        RepositoryOptions::with_environment(environment),
        Path::new(".git"),
        Path::new("."),
    )
    .expect("relative paths resolve against the environment cwd");
    let canonical_real = real.canonicalize().expect("canon");
    assert_eq!(repo.git_dir(), canonical_real.join(".git"));
    assert_eq!(
        repo.odb().objects_dir(),
        canonical_real.join(".git/objects")
    );
    assert_blob_round_trip(&repo);
}

/// The repository resolves its own `objects/` directory; a custom primary neither needs it
/// to exist nor creates it.
#[test]
fn custom_primary_opens_without_objects_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let git_dir = dir.path().join(".git");
    fs::remove_dir_all(git_dir.join("objects")).expect("remove objects");
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(dir.path())),
    )
    .expect("open without objects/");
    let oid = repo.odb().write(ObjectKind::Blob, b"hello").expect("write");
    assert!(store.contains(&oid).expect("contains"));
    assert!(!git_dir.join("objects").exists());
    assert_eq!(repo.odb().files_objects_dir(), None);

    let err = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .err()
    .expect("files primary still needs objects/");
    assert!(
        matches!(err, Error::Repository(RepositoryError::NotARepository(_))),
        "unexpected error: {err:?}"
    );
}

#[test]
fn memory_primary_revwalk_diff_without_loose_objects() {
    let (_dir, repo, _store) = memory_repo();
    let objects_snapshot_before = list_files_under(repo.odb().objects_dir());

    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"payload\n")
        .expect("blob");
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"f".to_vec(),
        oid: blob,
    }]);
    let tree = repo
        .odb()
        .write(ObjectKind::Tree, &tree_body)
        .expect("tree");
    let commit_body = format!(
        "tree {tree}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nmsg\n"
    );
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    grit_lib::refs::update_direct_ref_on_store(
        repo.ref_store(),
        repo.git_dir(),
        "refs/heads/main",
        &commit,
    )
    .expect("ref");

    let tip = commit.to_hex();
    let listed = repo
        .revwalk()
        .rev_list(
            &[tip.clone()],
            &[],
            &RevListOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rev-list");
    assert_eq!(listed.commits.len(), 1);
    assert_eq!(listed.commits[0], commit);

    let blob2 = repo
        .odb()
        .write(ObjectKind::Blob, b"other\n")
        .expect("blob2");
    let tree2_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"g".to_vec(),
        oid: blob2,
    }]);
    let tree2 = repo
        .odb()
        .write(ObjectKind::Tree, &tree2_body)
        .expect("tree2");
    let diff = repo
        .diff()
        .tree_to_tree(Some(&tree2), Some(&tree), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    assert!(!diff.is_empty());

    assert_eq!(
        list_files_under(repo.odb().objects_dir()),
        objects_snapshot_before,
        "custom primary must not mutate the on-disk objects/ tree"
    );
}

#[test]
fn memory_primary_resolve_abbrev_via_lookup_prefix() {
    let (_dir, repo, _store) = memory_repo();
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"abbrev-test\n")
        .expect("blob");
    let prefix = &blob.to_hex()[..8];
    let resolved = repo
        .revwalk()
        .resolve(prefix, &grit_lib::revwalk::ResolveOptions::default())
        .expect("abbrev");
    assert_eq!(resolved, blob);
}

#[test]
fn memory_primary_write_raw_ignores_seeded_loose_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let objects = git_dir.join("objects");
    let payload = b"seed-on-disk-only\n";
    let store_bytes = canonical_store_bytes(ObjectKind::Blob, payload);
    let oid = grit_lib::objects::hash::hash_object(
        grit_lib::objects::HashAlgo::Sha1,
        ObjectKind::Blob,
        payload,
    )
    .expect("hash");
    let loose = objects.join(oid.loose_prefix()).join(oid.loose_suffix());
    fs::create_dir_all(loose.parent().unwrap()).expect("shard");
    fs::write(&loose, b"placeholder").expect("seed loose");

    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(dir.path())),
    )
    .expect("open");

    assert!(!store.contains(&oid).expect("contains"));
    repo.odb().write_raw(&store_bytes).expect("write_raw");
    assert!(store.contains(&oid).expect("contains after"));
}

#[test]
fn files_primary_gc_prunes_unreachable_loose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let kept_blob = repo.odb().write(ObjectKind::Blob, b"keep\n").expect("keep");
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"f".to_vec(),
        oid: kept_blob,
    }]);
    let tree = repo
        .odb()
        .write(ObjectKind::Tree, &tree_body)
        .expect("tree");
    let commit_body = format!(
        "tree {tree}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nm\n"
    );
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    grit_lib::refs::update_direct_ref_on_store(
        repo.ref_store(),
        repo.git_dir(),
        "refs/heads/main",
        &commit,
    )
    .expect("ref");

    let orphan_payload = b"orphan\n";
    let orphan_oid = grit_lib::objects::hash::hash_object(
        grit_lib::objects::HashAlgo::Sha1,
        ObjectKind::Blob,
        orphan_payload,
    )
    .expect("hash");
    let orphan_path = repo.odb().object_path(&orphan_oid);
    fs::create_dir_all(orphan_path.parent().unwrap()).expect("shard");
    fs::write(&orphan_path, b"x").expect("orphan loose");

    repo.odb().gc().expect("gc");
    assert!(
        !orphan_path.exists(),
        "gc must prune unreachable loose objects"
    );
    assert!(repo.odb().read(&kept_blob).is_ok());
    let stats = prune_loose_unreachable(repo.odb(), &[commit], None).expect("verify prune");
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
            .odb()
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
    let abbrev = repo
        .revwalk()
        .abbreviate(first, grit_lib::revwalk::AbbrevLength::AtLeast(4))
        .expect("abbrev");
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
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");

    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"tagged-blob\n")
        .expect("blob");
    grit_test_support::refs::set_ref(&repo, "refs/tags/blob-tip", &blob).expect("blob ref");

    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"t".to_vec(),
        oid: blob,
    }]);
    let tree = repo
        .odb()
        .write(ObjectKind::Tree, &tree_body)
        .expect("tree");
    grit_test_support::refs::set_ref(&repo, "refs/tags/tree-tip", &tree).expect("tree ref");

    let tag_bytes = serialize_tag(&TagData::new(
        blob,
        "blob",
        "v1",
        Some("T <t@example.com> 1 +0000".into()),
        "annotated\n",
    ));
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &tag_bytes)
        .expect("tag object");
    grit_test_support::refs::set_ref(&repo, "refs/tags/annotated", &tag_oid).expect("tag ref");

    repo.odb().gc().expect("gc");

    assert!(repo.odb().read(&blob).is_ok());
    assert!(repo.odb().read(&tree).is_ok());
    assert!(repo.odb().read(&tag_oid).is_ok());
    git_fsck_full(dir.path());
}

#[test]
fn memory_primary_gc_prunes_but_commit_graph_write_refuses() {
    let (_dir, repo, store) = memory_repo();
    let orphan = repo
        .odb()
        .write(ObjectKind::Blob, b"orphan\n")
        .expect("orphan");
    repo.odb()
        .gc()
        .expect("gc prunes through MemoryStore's capability");
    assert!(!store.contains(&orphan).expect("contains"));
    assert!(matches!(
        repo.odb().write_commit_graph(),
        Err(Error::Repository(RepositoryError::UnsupportedObjectStore {
            operation: FilesOnlyOperation::CommitGraphWrite,
        }))
    ));
}
