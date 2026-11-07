//! [`SqliteOdbStore`] primary with [`MemoryRefStore`]: repository conformance suite.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use grit_examples::sqlite_odb::SqliteOdbStore;
use grit_lib::objects::HashAlgo;
use grit_lib::refs::commit_ref_updates_on_store;
use grit_lib::refs::store::{Expected, MemoryRefStore, RefUpdate, RefUpdateFlags};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::repository_conformance::{run_parity, run_repository_suite, BackendFixture};

static SQLITE_DB_COUNTER: AtomicU32 = AtomicU32::new(0);

struct SqliteMemoryBackend;

impl BackendFixture for SqliteMemoryBackend {
    fn name(&self) -> &str {
        "sqlite-memory-refs"
    }

    fn init(&self, dir: &Path, opts: &InitOptions) -> Repository {
        let n = SQLITE_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = std::env::temp_dir().join(format!(
            "grit-sqlite-conformance-{}-{n}.sqlite",
            std::process::id()
        ));
        let store = Arc::new(SqliteOdbStore::create(&db_path, HashAlgo::Sha1).expect("sqlite"));
        let merged = opts.clone().with_repository_options(
            opts.repository
                .clone()
                .with_object_store(store)
                .with_ref_store(Arc::new(MemoryRefStore::new()))
                .with_work_tree(dir),
        );
        let (repo, _) = Repository::init(dir, &merged).expect("init");
        repo
    }

    fn export_to_files(&self, repo: &Repository, dest: &Path) {
        export_injected_repo_to_files(repo, dest);
    }
}

struct FilesBackend;

impl BackendFixture for FilesBackend {
    fn name(&self) -> &str {
        "files"
    }

    fn init(&self, dir: &Path, opts: &InitOptions) -> Repository {
        let (repo, _) = Repository::init(dir, opts).expect("init");
        repo
    }

    fn export_to_files(&self, repo: &Repository, dest: &Path) {
        grit_test_support::repository_conformance::export_objects_to_files_repo(repo, dest);
    }
}

fn copy_worktree(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).expect("mkdir dest");
    for entry in fs::read_dir(src).expect("read worktree") {
        let entry = entry.expect("entry");
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(file_name);
        if from.is_dir() {
            copy_worktree(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

/// Materialize a repository whose objects and refs live in custom stores into a normal layout.
fn export_injected_repo_to_files(repo: &Repository, dest: &Path) {
    if dest.exists() {
        fs::remove_dir_all(dest).expect("clear export dest");
    }
    if let Some(wt) = repo.work_tree() {
        copy_worktree(wt, dest);
    } else {
        fs::create_dir_all(dest).expect("mkdir dest");
    }
    let (disk, _) = Repository::init(dest, &InitOptions::new().with_initial_branch("main"))
        .expect("init export dest");
    repo.odb()
        .for_each_object(&mut |oid| {
            let obj = repo.odb().read(oid).expect("read");
            let written = disk.odb().write(obj.kind, &obj.data).expect("write export");
            assert_eq!(written, *oid);
            ControlFlow::Continue(())
        })
        .expect("export objects");
    let mut updates = Vec::new();
    repo.ref_store()
        .for_each_ref("", &mut |entry| {
            updates.push(RefUpdate {
                name: entry.name.clone(),
                new_value: Some(entry.value.clone()),
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            });
            ControlFlow::Continue(())
        })
        .expect("list injected refs");
    commit_ref_updates_on_store(disk.ref_store(), &updates, &[]).expect("export refs");
}

#[test]
fn sqlite_primary_passes_repository_conformance_suite() {
    run_repository_suite(&SqliteMemoryBackend);
}

#[test]
fn sqlite_and_files_backends_match_typed_outcomes() {
    run_parity(&FilesBackend, &SqliteMemoryBackend);
}
