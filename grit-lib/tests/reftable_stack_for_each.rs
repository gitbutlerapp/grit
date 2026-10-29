//! Multi-table reftable stacks: merged iteration and indexed lookup (issue 2308).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use grit_lib::objects::ObjectId;
use grit_lib::refs::reftable::{
    read_write_options, reftable_delete_ref_with_write_options,
    reftable_write_ref_with_write_options, ReftableStack,
};
use grit_lib::refs::store::{RawRef, RefStore, ReftableRefStore};
use grit_lib::repo::{InitOptions, Repository};

use support::{git_for_each_ref, git_fsck_strict, require_reftable_git, stored_commit};

fn autocompact_off(git_dir: &std::path::Path) -> grit_lib::refs::reftable::WriteOptions {
    let mut opts = read_write_options(git_dir);
    opts.autocompaction_enabled = false;
    opts
}

fn write_ref(
    git_dir: &std::path::Path,
    name: &str,
    n: u8,
    opts: &grit_lib::refs::reftable::WriteOptions,
) {
    let oid = stored_commit(git_dir, n);
    reftable_write_ref_with_write_options(git_dir, name, &oid, None, None, opts).expect("write");
}

#[test]
fn stacked_tables_for_each_matches_git_with_override_and_deletion() {
    if !require_reftable_git() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Reftable),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let opts = autocompact_off(&git_dir);

    for batch in 0..4_u8 {
        for i in 0..250_u16 {
            let idx = batch as u16 * 250 + i;
            write_ref(
                &git_dir,
                &format!("refs/heads/b/n{idx:05}"),
                (idx % 200) as u8 + 1,
                &opts,
            );
        }
    }
    let override_oid = stored_commit(&git_dir, 99);
    reftable_write_ref_with_write_options(
        &git_dir,
        "refs/heads/b/n00100",
        &override_oid,
        None,
        None,
        &opts,
    )
    .expect("override");
    reftable_delete_ref_with_write_options(&git_dir, "refs/heads/b/n00200", &opts).expect("delete");

    let stack = ReftableStack::open(&git_dir).expect("open");
    assert!(
        stack.table_names().len() >= 4,
        "expected multiple tables, got {}",
        stack.table_names().len()
    );

    assert!(
        stack
            .lookup_ref("refs/heads/b/n00100")
            .expect("lookup")
            .is_some_and(|r| matches!(r.value, grit_lib::refs::reftable::RefValue::Val1(oid) if oid == override_oid)),
        "newer table must win on override"
    );
    assert!(
        stack
            .lookup_ref("refs/heads/b/n00200")
            .expect("lookup")
            .is_none(),
        "deletion in newer table must hide ref"
    );
    assert!(
        stack
            .lookup_ref("refs/heads/b/n00500")
            .expect("lookup")
            .is_some(),
        "ref present only in older indexed tables must still resolve"
    );
    assert!(
        stack
            .lookup_ref("refs/heads/b/n99999")
            .expect("lookup")
            .is_none(),
        "absent ref must not fall back to full scan"
    );

    let store = ReftableRefStore::open(git_dir.clone()).expect("store");
    let mut grit_map = BTreeMap::new();
    store
        .for_each_ref("refs/heads/b/", &mut |entry| {
            if let RawRef::Direct(oid) = entry.value {
                grit_map.insert(entry.name.clone(), oid);
            }
            ControlFlow::Continue(())
        })
        .expect("for_each");

    let git_map: BTreeMap<String, ObjectId> = git_for_each_ref(dir.path(), "refs/heads/b/")
        .into_iter()
        .collect();
    assert_eq!(
        grit_map, git_map,
        "for-each-ref must match git on stacked tables"
    );

    let mut prefixed = Vec::new();
    store
        .for_each_ref("refs/heads/b/n001", &mut |entry| {
            prefixed.push(entry.name.clone());
            ControlFlow::Continue(())
        })
        .expect("prefix walk");
    assert!(
        prefixed.iter().all(|n| n.starts_with("refs/heads/b/n001")),
        "prefix must push down: {prefixed:?}"
    );
    assert!(!prefixed.is_empty());

    git_fsck_strict(dir.path());
}

#[test]
fn stacked_tables_prefix_for_each_stops_early() {
    if !require_reftable_git() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Reftable),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let opts = autocompact_off(&git_dir);
    for batch in 0..4_u8 {
        for i in 0..500_u16 {
            let idx = batch as u16 * 500 + i;
            write_ref(
                &git_dir,
                &format!("refs/heads/s/n{idx:05}"),
                (idx % 200) as u8 + 1,
                &opts,
            );
        }
    }

    let store = ReftableRefStore::open(git_dir).expect("store");
    let start = Instant::now();
    let mut count = 0usize;
    store
        .for_each_ref("refs/heads/s/", &mut |_| {
            count += 1;
            if count >= 5 {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })
        .expect("for_each");
    assert_eq!(count, 5);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "early break must not walk entire stack: {:?}",
        start.elapsed()
    );
}
