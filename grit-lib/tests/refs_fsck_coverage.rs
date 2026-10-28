//! Basic `refs_fsck` integration for files-backend repositories.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::refs::refs_fsck::{format_refs_fsck_line, refs_fsck, RefsFsckIssue};
use grit_lib::refs::store::{Expected, RawRef, RefTransaction, RefUpdate, RefUpdateFlags};
use grit_lib::refs::write_ref;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::{add_linked_worktree, AddLinkedWorktreeOptions, WorktreeBranch};
use grit_lib::RefStorageFormat;
use tempfile::tempdir;

#[test]
fn refs_fsck_reports_bad_symref_and_formats_line() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let config = ConfigSet::load_repo_local_only(&git_dir).expect("config");
    let odb = repo.odb().clone();

    std::fs::write(
        git_dir.join("refs/heads/bad-sym"),
        "ref: refs/heads/missing-target\n",
    )
    .expect("symref");

    write_ref(&git_dir, "refs/heads/good", &oid(1)).expect("good ref");
    let dangling = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/dangling".to_owned(),
            new_value: Some(RawRef::Direct(ObjectId::from_bytes(&[1; 20]).expect("oid"))),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags {
                skip_oid_verification: true,
                ..RefUpdateFlags::default()
            },
        })
        .expect("txn");
    repo.refs()
        .prepare(dangling)
        .and_then(|prepared| prepared.commit())
        .expect("deliberately dangling ref");

    let issues = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(
        issues
            .iter()
            .any(|i| i.path.contains("dangling") && i.msg_id == "missingObject"),
        "expected a missing-object issue for the dangling ref, got {issues:?}"
    );
    assert!(
        issues.iter().all(|i| !i.path.contains("good")),
        "{issues:?}"
    );

    let sample = RefsFsckIssue {
        severity: grit_lib::refs::refs_fsck::RefsFsckSeverity::Error,
        path: "refs/heads/sample".into(),
        msg_id: "badRefName".into(),
        detail: "demo".into(),
    };
    let line = format_refs_fsck_line(&sample);
    assert!(line.contains("badRefName"));
}

/// A `packed-refs` file whose header claims `sorted` while its records are out of order is
/// read by binary search (as Git does), so fsck must be the place that reports it.
#[test]
fn refs_fsck_reports_packed_refs_out_of_order_under_sorted_header() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let config = ConfigSet::load_repo_local_only(&git_dir).expect("config");
    let odb = repo.odb().clone();
    let blob = odb
        .write(grit_lib::objects::ObjectKind::Blob, b"target\n")
        .expect("blob")
        .to_hex();
    let packed = |header: &str, names: &[&str]| {
        let mut body = header.to_owned();
        for name in names {
            body.push_str(&format!("{blob} {name}\n"));
        }
        std::fs::write(git_dir.join("packed-refs"), body).expect("packed-refs");
        refs_fsck(&repo, &odb, &config, false)
            .expect("fsck")
            .into_iter()
            .filter(|i| i.msg_id == "packedRefUnsorted")
            .collect::<Vec<_>>()
    };
    let sorted = "# pack-refs with: peeled fully-peeled sorted \n";
    let out_of_order = ["refs/heads/b", "refs/heads/a", "refs/tags/t"];

    let lying = packed(sorted, &out_of_order);
    assert_eq!(lying.len(), 1, "{lying:?}");
    assert_eq!(
        lying[0].severity,
        grit_lib::refs::refs_fsck::RefsFsckSeverity::Error
    );
    assert_eq!(lying[0].path, "packed-refs line 3");
    assert!(lying[0].detail.contains("refs/heads/a"), "{lying:?}");
    assert!(format_refs_fsck_line(&lying[0]).contains("packedRefUnsorted"));

    assert!(packed(sorted, &["refs/heads/a", "refs/heads/b", "refs/tags/t"]).is_empty());
    assert!(packed("# pack-refs with: peeled fully-peeled \n", &out_of_order).is_empty());
}

#[test]
fn refs_fsck_accepts_reftable_placeholder_files_after_commit() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let config = ConfigSet::load_repo_local_only(&git_dir).expect("config");
    let odb = repo.odb().clone();

    let oid = support::stored_commit(&git_dir, 1);
    write_ref(&git_dir, "refs/heads/main", &oid).expect("main");

    let issues = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn refs_fsck_reftable_linked_worktree_placeholders_and_missing_object() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let oid = support::stored_commit(&git_dir, 1);
    write_ref(&git_dir, "refs/heads/main", &oid).expect("main");

    let wt_path = dir.path().join("linked");
    let entry = add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt_path.clone(),
            branch: Some(WorktreeBranch::Create("linked".into())),
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("worktree");
    assert_eq!(
        std::fs::read(entry.admin_dir.join("refs/heads")).expect("stub"),
        b"this repository uses the reftable format\n"
    );

    let config = ConfigSet::load_repo_local_only(&git_dir).expect("config");
    let odb = repo.odb().clone();
    let healthy = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(healthy.is_empty(), "{healthy:?}");

    let dangling = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/dangling".to_owned(),
            new_value: Some(RawRef::Direct(ObjectId::from_bytes(&[2; 20]).expect("oid"))),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags {
                skip_oid_verification: true,
                ..RefUpdateFlags::default()
            },
        })
        .expect("txn");
    repo.refs()
        .prepare(dangling)
        .and_then(|prepared| prepared.commit())
        .expect("dangling ref");

    let with_missing = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(
        with_missing
            .iter()
            .any(|i| i.msg_id == "missingObject" && i.path.contains("dangling")),
        "{with_missing:?}"
    );

    std::fs::write(git_dir.join("refs/heads"), "not the reftable sentinel\n").expect("bad stub");
    let bad_stub = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(
        bad_stub
            .iter()
            .any(|i| i.msg_id == "badRefContent" && i.path.ends_with("refs/heads")),
        "{bad_stub:?}"
    );

    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/bad..name\n").expect("bad head");
    let bad_head = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(
        bad_head
            .iter()
            .any(|i| i.path.ends_with("HEAD") && i.msg_id == "badReferentName"),
        "{bad_head:?}"
    );
}
