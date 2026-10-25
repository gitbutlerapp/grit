//! Apply a pack stream to a custom object store.
//!
//! Builds a pack from a files-backed repository (as a fetch, push or bundle would
//! deliver it), then writes its objects into an in-memory store two ways: straight
//! through the [`WritableObjectStore`](grit_lib::objects::odb::store::WritableObjectStore)
//! trait, and through a repository whose primary is that store.

use std::io::Cursor;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::index_pack::{ingest_received_pack, IngestPackOptions};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::pack_objects::{build_pack, PackBuildOptions};
use grit_lib::objects::unpack_objects::{ingest_pack_into, UnpackOptions};
use grit_lib::objects::{serialize_tree, HashAlgo, ObjectKind, TreeEntry};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

fn main() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let source = Repository::init(&dir.path().join("source"), &InitOptions::default())?;
    let mut parent = None;
    for version in 1..=2 {
        let blob = source
            .odb()
            .write(ObjectKind::Blob, format!("version {version}\n").as_bytes())?;
        let tree = source.odb().write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o100644,
                name: b"file.txt".to_vec(),
                oid: blob,
            }]),
        )?;
        let parent_line = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
        let body = format!(
            "tree {tree}\n{parent_line}author A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nv{version}\n"
        );
        parent = Some(source.odb().write(ObjectKind::Commit, body.as_bytes())?);
    }
    let tip = parent.ok_or(grit_lib::error::Error::ObjectNotFound("tip".into()))?;
    let pack = build_pack(source.odb(), &[tip], &[], &PackBuildOptions::default())?;
    println!("pack_bytes={}", pack.len());

    // Any WritableObjectStore: deltas are resolved and each object is written.
    let store = MemoryStore::new(HashAlgo::Sha1);
    let ingested = ingest_pack_into(
        &mut Cursor::new(&pack),
        &store,
        &store,
        &UnpackOptions::default(),
    )?;
    println!("store_objects={}", ingested.object_ids.len());
    println!("store_has_tip={}", store.contains(&tip)?);

    // A repository whose primary is a custom store: the same entry point fetch and
    // clone use writes into that store instead of installing a pack under objects/.
    let git_dir = dir.path().join("custom.git");
    std::fs::create_dir_all(&git_dir)?;
    let primary = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(primary.clone())
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("main")
        .open(&git_dir, None)?;
    let received = ingest_received_pack(pack, repo.odb(), &IngestPackOptions::default())?;
    println!("repo_objects={}", received.object_ids.len());
    println!("repo_commits={}", received.object_counts.commits);
    println!("pack_installed={}", received.pack_path.is_some());
    println!("repo_reads_tip={}", repo.odb().read(&tip).is_ok());
    Ok(())
}
