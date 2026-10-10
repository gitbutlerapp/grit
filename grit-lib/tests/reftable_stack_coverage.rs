//! Reftable stack compaction, autocompaction, and multi-table append paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use grit_lib::objects::ObjectId;
use grit_lib::reftable::{
    read_write_options, reftable_delete_ref, reftable_list_refs, reftable_write_ref, RefValue,
    ReftableStack,
};
use grit_lib::repo::init_repository;
use tempfile::tempdir;

const IDENTITY: &str = "Stack Cov <stack@example.com> 1700000000 +0000";

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

fn table_count(git_dir: &std::path::Path) -> usize {
    let list = fs::read_to_string(git_dir.join("reftable/tables.list")).expect("tables.list");
    list.lines().filter(|l| !l.is_empty()).count()
}

#[test]
fn autocompaction_after_many_appends_and_manual_compact() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "reftable").expect("init");
    let git_dir = dir.path().join(".git");

    for i in 0..6u8 {
        reftable_write_ref(
            &git_dir,
            &format!("refs/heads/branch-{i}"),
            &oid(i + 10),
            Some(IDENTITY),
            Some("create"),
        )
        .expect("write ref");
    }
    assert!(
        table_count(&git_dir) >= 2,
        "expected multiple tables after writes"
    );

    let mut stack = ReftableStack::open(&git_dir).expect("open");
    let max_before = stack.max_update_index().expect("max idx");
    stack.compact().expect("compact");
    assert_eq!(table_count(&git_dir), 1);
    let listed = reftable_list_refs(&git_dir, "refs/heads/").expect("list");
    assert!(listed.iter().any(|(n, _)| n.contains("branch-5")));
    assert!(max_before >= 1);

    reftable_delete_ref(&git_dir, "refs/heads/branch-0").expect("delete");
    reftable_write_ref(
        &git_dir,
        "refs/heads/branch-new",
        &oid(99),
        Some(IDENTITY),
        Some("after delete"),
    )
    .expect("write after delete");

    let stack2 = ReftableStack::open(&git_dir).expect("open2");
    let _logs = stack2.read_all_logs().expect("logs");
    let _refs = stack2.read_refs().expect("refs");
}

#[test]
fn compact_unlocked_suffix_when_middle_table_locked() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "reftable").expect("init");
    let git_dir = dir.path().join(".git");
    let rt = git_dir.join("reftable");

    reftable_write_ref(
        &git_dir,
        "refs/heads/base-a",
        &oid(20),
        Some(IDENTITY),
        Some("a"),
    )
    .expect("a");
    reftable_write_ref(
        &git_dir,
        "refs/heads/base-b",
        &oid(21),
        Some(IDENTITY),
        Some("b"),
    )
    .expect("b");

    let list = fs::read_to_string(rt.join("tables.list")).expect("list");
    let mut names: Vec<String> = list
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    while names.len() < 3 {
        let clone_name = format!("clone-{}-{}", names.len(), names[0]);
        fs::copy(rt.join(&names[0]), rt.join(&clone_name)).expect("clone table");
        names.push(clone_name);
        fs::write(rt.join("tables.list"), format!("{}\n", names.join("\n"))).expect("extend list");
    }

    let lock_target = &names[1];
    fs::write(rt.join(format!("{lock_target}.lock")), b"1").expect("lock");

    reftable_write_ref(
        &git_dir,
        "refs/heads/after-lock",
        &oid(77),
        Some(IDENTITY),
        Some("trigger suffix compact"),
    )
    .expect("write after lock");

    let mut stack = ReftableStack::open(&git_dir).expect("open");
    stack.compact().expect("final compact");
    let listed = reftable_list_refs(&git_dir, "refs/heads/after-lock").expect("list one");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, oid(77));
}

#[test]
fn read_write_options_manual_config_fallback() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "reftable").expect("init");
    let git_dir = dir.path().join(".git");
    fs::write(
        git_dir.join("config"),
        "[reftable]\n\tblockSize = 4096\n\trestartInterval = 8\n\tindexObjects = false\n\
         [core]\n\tlogAllRefUpdates = false\n[broken\n",
    )
    .expect("broken tail");
    let opts = read_write_options(&git_dir);
    assert_eq!(opts.block_size, 4096);
    assert_eq!(opts.restart_interval, 8);
    assert!(opts.skip_index_objects);
    assert!(!opts.write_log);
}

#[test]
fn compact_prefix_after_deletion_with_multiple_tables() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "reftable").expect("init");
    let git_dir = dir.path().join(".git");
    let rt = git_dir.join("reftable");

    reftable_write_ref(
        &git_dir,
        "refs/heads/d1",
        &oid(1),
        Some(IDENTITY),
        Some("1"),
    )
    .expect("w1");
    reftable_write_ref(
        &git_dir,
        "refs/heads/d2",
        &oid(2),
        Some(IDENTITY),
        Some("2"),
    )
    .expect("w2");
    reftable_write_ref(
        &git_dir,
        "refs/heads/d3",
        &oid(3),
        Some(IDENTITY),
        Some("3"),
    )
    .expect("w3");

    while table_count(&git_dir) < 3 {
        let list = fs::read_to_string(rt.join("tables.list")).expect("list");
        let first = list
            .lines()
            .find(|l| !l.is_empty())
            .expect("name")
            .to_owned();
        let extra = format!("dup-{}", table_count(&git_dir));
        fs::copy(rt.join(&first), rt.join(&extra)).expect("copy");
        fs::write(rt.join("tables.list"), format!("{list}{extra}\n")).expect("extend");
    }

    reftable_delete_ref(&git_dir, "refs/heads/d1").expect("delete triggers prefix compact");
    assert!(table_count(&git_dir) >= 1);
}

#[test]
fn stack_write_ref_deletion_value() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "reftable").expect("init");
    let git_dir = dir.path().join(".git");
    reftable_write_ref(&git_dir, "refs/heads/del-me", &oid(1), Some(IDENTITY), None)
        .expect("create");

    let mut stack = ReftableStack::open(&git_dir).expect("open");
    let opts = grit_lib::reftable::read_write_options(&git_dir);
    stack
        .write_ref("refs/heads/del-me", RefValue::Deletion, None, &opts)
        .expect("delete via stack");
    let listed = reftable_list_refs(&git_dir, "refs/heads/del-me").expect("list");
    assert!(listed.is_empty());
}
