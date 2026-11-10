//! Integration tests for typed [`RevParseError`] and [`RevListError`] variants.

use grit_lib::progress::{CancelToken, NullProgress};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::error::RevisionError;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{ObjectFilter, RevListOptions};
use grit_lib::revwalk::rev_list_error::RevListError;
use grit_lib::revwalk::rev_parse_error::RevParseError;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn empty_tree(odb: &Odb) -> grit_lib::error::Result<ObjectId> {
    odb.write_loose_materialize(ObjectKind::Tree, b"")
}

fn write_commit(odb: &Odb, msg: &str) -> grit_lib::error::Result<ObjectId> {
    let tree = empty_tree(odb)?;
    let body = format!(
        "tree {tree}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n{msg}\n"
    );
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn open_bare_with_main(dir: &Path) -> grit_lib::error::Result<(Repository, ObjectId)> {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    let repo = Repository::open(dir, &RepositoryOptions::empty())?;
    let tip = write_commit(repo.odb(), "init")?;
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))?;
    Ok((repo, tip))
}

fn write_config(git_dir: &Path, body: &str) {
    fs::write(git_dir.join("config"), body).expect("write config");
}

#[test]
fn upstream_missing_reports_no_upstream_variant() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = open_bare_with_main(dir.path()).expect("repo");
    write_config(
        repo.git_dir(),
        "[core]\n\trepositoryformatversion = 0\n[branch \"main\"]\n",
    );
    let err = repo
        .revwalk()
        .upstream_symbolic_name("main@{u}")
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::Parse(RevParseError::NoUpstream { .. }))
    ));
}

#[test]
fn push_default_nothing_reports_variant() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = open_bare_with_main(dir.path()).expect("repo");
    write_config(
        repo.git_dir(),
        "[core]\n\trepositoryformatversion = 0\n\
         [remote \"origin\"]\n\turl = /tmp/x\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\
         [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n\
         [push]\n\tdefault = nothing\n",
    );
    fs::create_dir_all(repo.git_dir().join("refs/remotes/origin")).expect("remotes dir");
    fs::write(
        repo.git_dir().join("refs/remotes/origin/main"),
        b"0000000000000000000000000000000000000001\n",
    )
    .expect("tracking ref");
    let err = repo
        .revwalk()
        .upstream_symbolic_name("main@{push}")
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::Parse(RevParseError::PushDefaultNothing))
    ));
}

#[test]
fn ambiguous_spec_reports_ambiguous_argument() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = open_bare_with_main(dir.path()).expect("repo");
    let err = repo
        .revwalk()
        .resolve(
            "not-a-ref-or-path",
            &grit_lib::revwalk::ResolveOptions::default(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::Parse(
            RevParseError::AmbiguousArgument { .. }
        ))
    ));
}

#[test]
fn sparse_blob_unreadable_for_missing_spec() {
    let dir = tempdir().expect("tempdir");
    let (repo, tip) = open_bare_with_main(dir.path()).expect("repo");
    let mut opts = RevListOptions::default();
    opts.filter = Some(ObjectFilter::SparseOid("missing:path".into()));
    let err = repo
        .revwalk()
        .rev_list(
            &[tip.to_hex()],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::List(
            RevListError::SparseBlobUnreadable { .. }
        ))
    ));
}

#[test]
fn sparse_filter_unparsable_when_commit_not_blob() {
    let dir = tempdir().expect("tempdir");
    let (repo, tip) = open_bare_with_main(dir.path()).expect("repo");
    let mut opts = RevListOptions::default();
    opts.filter = Some(ObjectFilter::SparseOid(tip.to_hex()));
    let err = repo
        .revwalk()
        .rev_list(
            &[tip.to_hex()],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::List(
            RevListError::SparseFilterUnparsable { .. }
        ))
    ));
}

#[test]
fn sparse_filter_unparsable_on_invalid_utf8_blob() {
    let dir = tempdir().expect("tempdir");
    let (repo, tip) = open_bare_with_main(dir.path()).expect("repo");
    let blob = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &[0xff, 0xfe])
        .expect("blob");
    let mut opts = RevListOptions::default();
    opts.filter = Some(ObjectFilter::SparseOid(blob.to_hex()));
    let err = repo
        .revwalk()
        .rev_list(
            &[tip.to_hex()],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::List(
            RevListError::SparseFilterUnparsable { .. }
        ))
    ));
}

#[test]
fn push_refspec_missing_when_branch_not_mapped() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = open_bare_with_main(dir.path()).expect("repo");
    write_config(
        repo.git_dir(),
        "[core]\n\trepositoryformatversion = 0\n\
         [remote \"origin\"]\n\turl = /tmp/x\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\
         \tpush = refs/heads/other:refs/heads/other\n\
         [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    );
    let err = repo
        .revwalk()
        .upstream_symbolic_name("main@{push}")
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Revision(RevisionError::Parse(
                RevParseError::PushRefspecMissing { .. }
            ))
        ),
        "unexpected error: {err:?}"
    );
}
