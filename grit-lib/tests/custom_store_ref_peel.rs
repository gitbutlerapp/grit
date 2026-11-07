//! `packed-refs` peel lines for a repository whose objects live in a custom primary store while
//! its refs use the files backend.
//!
//! Annotated tags must get a `^` peel line however `packed-refs` is written: by pack-refs, or
//! by the clone that imports tags. The lines are checked against the system `git`, which reads
//! an exported files copy of the repository.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::maintenance::PackRefsOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_test_support::git;

use crate::support::{
    clone_upstream, init_files_repo, open_repo, packed_refs_records, tagged_history,
    tagged_upstream,
};

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// A files-backed copy of the work tree at `dir` with every object of `store` written to its
/// `objects/`, so the system `git` can read the repository.
fn export_to_files(dir: &Path, store: &MemoryStore) -> tempfile::TempDir {
    let export = tempfile::tempdir().expect("tempdir");
    copy_tree(dir, export.path());
    let repo = open_repo(export.path(), None);
    let mut oids = Vec::new();
    store
        .for_each_object(&mut |oid| {
            oids.push(*oid);
            std::ops::ControlFlow::Continue(())
        })
        .expect("list objects");
    for oid in oids {
        let object = store.read(&oid).expect("read").expect("stored");
        let written = repo
            .odb()
            .write(object.kind, &object.data)
            .expect("write object");
        assert_eq!(written, oid);
    }
    export
}

/// `git show-ref -d` in `dir`, for the refs named in `records`: each ref's value, and its peel
/// from the `^{}` line.
fn git_show_ref_peels(
    dir: &Path,
    records: &BTreeMap<String, (ObjectId, Option<ObjectId>)>,
) -> BTreeMap<String, (ObjectId, Option<ObjectId>)> {
    let mut out: BTreeMap<String, (ObjectId, Option<ObjectId>)> = BTreeMap::new();
    for line in git(dir, &["show-ref", "-d"]).lines() {
        let (oid, name) = line.split_once(' ').expect("show-ref line");
        let oid: ObjectId = oid.parse().expect("oid");
        match name.strip_suffix("^{}") {
            Some(base) => out.get_mut(base).expect("peeled ref listed first").1 = Some(oid),
            None => {
                out.insert(name.to_owned(), (oid, None));
            }
        }
    }
    out.retain(|name, _| records.contains_key(name));
    out
}

fn peels(
    records: &BTreeMap<String, (ObjectId, Option<ObjectId>)>,
) -> BTreeMap<String, Option<ObjectId>> {
    records
        .iter()
        .map(|(name, (_, peel))| (name.clone(), *peel))
        .collect()
}

#[test]
fn pack_refs_on_memory_primary_writes_peel_lines_git_agrees() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_files_repo(dir.path());
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = open_repo(dir.path(), Some(store.clone()));
    let history = tagged_history(&repo);
    let hex = history.v1.to_hex();
    assert!(store.contains(&history.v1).expect("contains"));
    assert!(
        !dir.path()
            .join(".git/objects")
            .join(&hex[..2])
            .join(&hex[2..])
            .exists(),
        "tags must live only in the memory store"
    );

    let report = repo
        .references()
        .pack(&PackRefsOptions::with_all(true))
        .expect("pack refs");
    assert_eq!(report.refs_packed, 5);

    let records = packed_refs_records(&dir.path().join(".git"));
    assert_eq!(peels(&records), history.expected_peels());
    assert_eq!(records["refs/tags/nested"].0, history.nested);
    assert_eq!(records["refs/tags/treetag"].0, history.treetag);

    let export = export_to_files(dir.path(), &store);
    git(export.path(), &["fsck", "--full", "--no-dangling"]);
    assert_eq!(git_show_ref_peels(export.path(), &records), records);
}

#[test]
fn clone_into_memory_primary_writes_the_peel_lines_of_a_files_clone() {
    let (upstream, history) = tagged_upstream();
    let (files_dir, _files_repo, _) = clone_upstream(upstream.path(), false);
    let (memory_dir, _memory_repo, store) = clone_upstream(upstream.path(), true);
    let store = store.expect("memory store");
    assert!(store.contains(&history.nested).expect("contains"));

    let files_records = packed_refs_records(&files_dir.path().join(".git"));
    let memory_records = packed_refs_records(&memory_dir.path().join(".git"));
    let tag_peels: BTreeMap<String, Option<ObjectId>> = peels(&memory_records)
        .into_iter()
        .filter(|(name, _)| name.starts_with("refs/tags/"))
        .collect();
    let expected: BTreeMap<String, Option<ObjectId>> = history
        .expected_peels()
        .into_iter()
        .filter(|(name, _)| name.starts_with("refs/tags/"))
        .collect();
    assert_eq!(tag_peels, expected);
    assert_eq!(memory_records, files_records);

    let export = export_to_files(memory_dir.path(), &store);
    git(export.path(), &["fsck", "--full", "--no-dangling"]);
    assert_eq!(
        git_show_ref_peels(export.path(), &memory_records),
        memory_records
    );
}
