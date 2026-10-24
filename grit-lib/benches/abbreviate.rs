//! Criterion benchmarks for unique object-id abbreviation over different object stores.
//!
//! Each case abbreviates the same 200 ids against 200k objects: the built-in
//! [`MemoryStore`], a minimal custom store that relies on the default
//! [`ObjectStore::lookup_prefix`], and a files ODB holding one pack (built with
//! `git fast-import`, which is allowed in benchmark harness code).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::hint::black_box;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::error::Result;
use grit_lib::objects::odb::builder::OdbBuilder;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::WriteOptions;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::prelude::RepositoryOptions;
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::revwalk::rev_parse::abbreviate_object_id;
use tempfile::TempDir;

const OBJECTS: usize = 200_000;
const ABBREVIATED: usize = 200;

type Objects = HashMap<ObjectId, (ObjectKind, Arc<[u8]>)>;

/// A store implementing only the required trait methods.
#[derive(Debug, Default)]
struct MinimalStore {
    objects: RwLock<Objects>,
}

impl ObjectStore for MinimalStore {
    fn hash_algo(&self) -> HashAlgo {
        HashAlgo::Sha1
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        let guard = self.objects.read().unwrap();
        Ok(guard.get(oid).map(|(kind, data)| Object {
            kind: *kind,
            data: data.to_vec(),
        }))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let guard = self.objects.read().unwrap();
        Ok(guard.get(oid).map(|(kind, data)| ObjectInfo {
            kind: *kind,
            size: data.len() as u64,
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let guard = self.objects.read().unwrap();
        for oid in guard.keys() {
            if f(oid).is_break() {
                break;
            }
        }
        Ok(())
    }
}

impl WritableObjectStore for MinimalStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        let oid = grit_lib::objects::hash::hash_object(HashAlgo::Sha1, kind, data)?;
        self.objects
            .write()
            .unwrap()
            .entry(oid)
            .or_insert_with(|| (kind, Arc::from(data)));
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.objects.read().unwrap().contains_key(oid))
    }
}

fn blob_body(i: usize) -> String {
    format!("blob {i}\n")
}

fn repo_over_store(store: Arc<dyn WritableObjectStore>) -> (TempDir, Repository, Vec<ObjectId>) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::new().with_initial_branch("main")).expect("init");
    let oids: Vec<ObjectId> = (0..OBJECTS)
        .map(|i| {
            store
                .write(
                    ObjectKind::Blob,
                    blob_body(i).as_bytes(),
                    WriteOptions::default(),
                )
                .expect("write blob")
        })
        .collect();
    let git_dir = dir.path().join(".git");
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store)
                .alternates(false),
        )
        .open(&git_dir, Some(dir.path()))
        .expect("open repo over custom store");
    (dir, repo, oids)
}

fn fast_import_blobs(work_tree: &Path) {
    let mut child = Command::new("git")
        .current_dir(work_tree)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git fast-import");
    {
        let mut stdin = std::io::BufWriter::new(child.stdin.take().expect("stdin"));
        for i in 0..OBJECTS {
            let body = blob_body(i);
            write!(stdin, "blob\ndata {}\n{body}\n", body.len()).expect("fast-import stream");
        }
        stdin.flush().expect("flush fast-import stream");
    }
    assert!(child.wait().expect("wait fast-import").success());
}

fn files_repo() -> (TempDir, Repository, Vec<ObjectId>) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::new().with_initial_branch("main")).expect("init");
    fast_import_blobs(dir.path());
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open repo");
    let oids = (0..OBJECTS)
        .map(|i| {
            grit_lib::objects::hash::hash_object(
                HashAlgo::Sha1,
                ObjectKind::Blob,
                blob_body(i).as_bytes(),
            )
            .expect("hash blob")
        })
        .collect();
    (dir, repo, oids)
}

fn bench_case(c: &mut Criterion, name: &str, repo: &Repository, oids: &[ObjectId]) {
    c.bench_function(&format!("abbreviate/{name}_200k_x200"), |b| {
        b.iter(|| {
            for oid in &oids[..ABBREVIATED] {
                black_box(abbreviate_object_id(repo, *oid, 7).expect("abbreviate"));
            }
        });
    });
}

fn bench_abbreviate(c: &mut Criterion) {
    let (_mem_dir, mem_repo, mem_oids) =
        repo_over_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)));
    bench_case(c, "memory_store", &mem_repo, &mem_oids);

    let (_min_dir, min_repo, min_oids) = repo_over_store(Arc::new(MinimalStore::default()));
    bench_case(c, "default_lookup_prefix", &min_repo, &min_oids);

    let (_files_dir, files_repo, files_oids) = files_repo();
    bench_case(c, "files_pack", &files_repo, &files_oids);
}

criterion_group! {
    name = abbreviate;
    config = Criterion::default().sample_size(10);
    targets = bench_abbreviate
}
criterion_main!(abbreviate);
