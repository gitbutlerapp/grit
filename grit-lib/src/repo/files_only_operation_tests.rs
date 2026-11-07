#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use super::{FilesOnlyOperation, RepositoryError};
use crate::environment::RepositoryOptions;
use crate::error::Error;
use crate::maintenance::{rewrite_local_objects_without, PromisorRewriteWhen, RepackOptions};
use crate::objects::{HashAlgo, ObjectKind, OidHashSet};
use crate::odb::store::MemoryStore;
use crate::odb::OdbBuilder;
use crate::receive_quarantine::ReceiveQuarantine;
use crate::repo::{InitOptions, Repository};

/// Every variant, listed through an exhaustive match so a new one cannot be left out.
fn all() -> Vec<FilesOnlyOperation> {
    use FilesOnlyOperation as Op;
    let all = vec![
        Op::Gc,
        Op::Prune,
        Op::Repack,
        Op::CommitGraphWrite,
        Op::MidxWrite,
        Op::PackStore,
        Op::PackInstall,
        Op::LargestPackIndex,
        Op::LooseWrite,
        Op::FilesSource,
        Op::Offload,
        Op::PromisorRewrite,
        Op::PartialFetch,
        Op::ReceiveQuarantine,
        Op::DropLargeBlobs,
    ];
    for op in &all {
        match op {
            Op::Gc
            | Op::Prune
            | Op::Repack
            | Op::CommitGraphWrite
            | Op::MidxWrite
            | Op::PackStore
            | Op::PackInstall
            | Op::LargestPackIndex
            | Op::LooseWrite
            | Op::FilesSource
            | Op::Offload
            | Op::PromisorRewrite
            | Op::PartialFetch
            | Op::ReceiveQuarantine
            | Op::DropLargeBlobs => {}
        }
    }
    all
}

fn snake_case(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The docs site derives each variant's JSON name from its Rust name the same way.
#[test]
fn as_str_is_the_snake_case_variant_name() {
    for op in all() {
        assert_eq!(op.as_str(), snake_case(&format!("{op:?}")), "{op:?}");
    }
}

#[test]
fn names_and_phrases_are_distinct_and_non_empty() {
    let ops = all();
    let names: std::collections::HashSet<_> = ops.iter().map(|op| op.as_str()).collect();
    let phrases: std::collections::HashSet<_> = ops.iter().map(ToString::to_string).collect();
    assert_eq!(names.len(), ops.len());
    assert_eq!(phrases.len(), ops.len());
    assert!(phrases.iter().all(|p| !p.is_empty()));
}

#[test]
fn error_message_names_the_operation_in_words() {
    for op in all() {
        let err = RepositoryError::UnsupportedObjectStore { operation: op };
        assert_eq!(err.kind_name(), "unsupported_object_store");
        assert!(err.to_string().starts_with(&op.to_string()), "{err}");
    }
}

fn memory_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    Repository::init(
        dir.path(),
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .unwrap();
    let git_dir = dir.path().join(".git");
    let builder = OdbBuilder::files(git_dir.join("objects"))
        .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
        .alternates(false);
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty()
            .with_odb_builder(builder)
            .with_work_tree(dir.path()),
    )
    .unwrap();
    (dir, repo)
}

#[track_caller]
fn assert_refused<T: std::fmt::Debug>(
    result: crate::error::Result<T>,
    expected: FilesOnlyOperation,
) {
    match result {
        Err(Error::Repository(RepositoryError::UnsupportedObjectStore { operation })) => {
            assert_eq!(operation, expected);
        }
        other => panic!("expected UnsupportedObjectStore({expected:?}), got {other:?}"),
    }
}

#[test]
fn receive_quarantine_refuses_custom_primary() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        ReceiveQuarantine::create(repo.odb(), repo.git_dir()).map(|_| ()),
        FilesOnlyOperation::ReceiveQuarantine,
    );
}

#[test]
fn promisor_rewrite_refuses_custom_primary() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        rewrite_local_objects_without(
            &repo,
            &OidHashSet::default(),
            &RepackOptions::default(),
            PromisorRewriteWhen::Always,
        )
        .map(|_| ()),
        FilesOnlyOperation::PromisorRewrite,
    );
}

#[test]
fn streamed_local_write_refuses_custom_primary() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        repo.odb()
            .write_local_stream_with(ObjectKind::Blob, 1, |sink| sink(b"x")),
        FilesOnlyOperation::LooseWrite,
    );
}

#[test]
fn missing_materialized_many_answers_for_a_files_primary() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::new()).unwrap();
    let odb = repo.odb();
    let present = odb.write(ObjectKind::Blob, b"present").unwrap();
    let absent = odb.hash(ObjectKind::Blob, b"absent").unwrap();
    assert_eq!(
        odb.missing_materialized_many(&[present, absent]).unwrap(),
        vec![absent]
    );
}
