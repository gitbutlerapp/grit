//! Reftable stack interop with system git (t0610 / t0614 / t1460 scenarios).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use grit_lib::error::Error;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::reflog::ReflogEntry;
use grit_lib::reftable::{
    read_write_options, reftable_append_reflog, reftable_create_reflog, reftable_delete_ref,
    reftable_delete_reflog, reftable_list_reflog_refs, reftable_log_record_for_commit,
    reftable_read_reflog, reftable_reflog_exists, reftable_replace_reflog, reftable_resolve_ref,
    reftable_write_ref, reftable_write_ref_with_write_options, reftable_write_symref,
    reftable_write_transaction, RefValue, ReftableStack, ReftableTransactionUpdate,
};

use support::{
    git, git_empty_commit_oid, git_for_each_ref_peeled, git_fsck_strict, git_init_reftable_repo,
    git_reflog_lines, git_refs_verify, git_show_ref, git_supports_refs_verify,
    grit_reflog_identity, require_reftable_git,
};

fn write_ref_with_autocompaction(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    log_identity: Option<&str>,
    log_message: Option<&str>,
    autocompaction_enabled: bool,
) -> grit_lib::error::Result<()> {
    let mut opts = read_write_options(git_dir);
    opts.autocompaction_enabled = autocompaction_enabled;
    reftable_write_ref_with_write_options(git_dir, refname, oid, log_identity, log_message, &opts)
}

fn grit_direct_refs(stack: &ReftableStack) -> BTreeMap<String, ObjectId> {
    let mut map = BTreeMap::new();
    for rec in stack.read_refs().expect("read_refs") {
        match rec.value {
            RefValue::Val1(oid) => {
                map.insert(rec.name, oid);
            }
            RefValue::Val2(oid, _) => {
                map.insert(rec.name, oid);
            }
            RefValue::Symref(_) | RefValue::Deletion => {}
        }
    }
    map
}

fn grit_reflog_newest_oid(stack: &ReftableStack, refname: &str) -> Vec<ObjectId> {
    stack
        .read_logs_for_ref(refname)
        .expect("read_logs_for_ref")
        .into_iter()
        .map(|log| log.new_id)
        .collect()
}

fn seed_git_reftable_repo() -> tempfile::TempDir {
    let root = git_init_reftable_repo("main");
    git(
        root.path(),
        &["commit", "--allow-empty", "-q", "-m", "seed"],
    );
    git(root.path(), &["branch", "feature"]);
    git(root.path(), &["tag", "-m", "release", "v1.0", "HEAD"]);
    git(
        root.path(),
        &["symbolic-ref", "refs/heads/sym", "refs/heads/main"],
    );
    git(
        root.path(),
        &[
            "update-ref",
            "-m",
            "git reflog",
            "refs/heads/feature",
            "HEAD",
        ],
    );
    root
}

#[test]
fn git_written_reftable_read_by_grit() {
    if !require_reftable_git() {
        return;
    }
    let root = seed_git_reftable_repo();
    let git_dir = root.path().join(".git");
    let stack = ReftableStack::open(&git_dir).expect("open stack");

    let grit_refs = grit_direct_refs(&stack);
    let git_refs = git_for_each_ref_peeled(root.path(), "refs/");
    for (name, oid) in &git_refs {
        if name.starts_with("refs/tags/") {
            continue;
        }
        if let Some(rec) = stack.lookup_ref(name).expect("lookup") {
            match rec.value {
                RefValue::Val1(got) | RefValue::Val2(got, _) => {
                    assert_eq!(&got, oid, "grit oid mismatch for {name}");
                }
                RefValue::Symref(target) => {
                    let resolved = reftable_resolve_ref(&git_dir, name).expect("resolve symref");
                    assert_eq!(
                        &resolved, oid,
                        "symref {name} -> {target} should resolve to git peeled oid"
                    );
                }
                RefValue::Deletion => panic!("unexpected deletion for {name}"),
            }
        } else {
            assert_eq!(grit_refs.get(name), Some(oid), "grit missing ref {name}");
        }
    }

    let sym = stack
        .lookup_ref("refs/heads/sym")
        .expect("lookup sym")
        .expect("sym ref");
    assert!(matches!(sym.value, RefValue::Symref(_)));

    let git_head = git(root.path(), &["symbolic-ref", "HEAD"])
        .trim()
        .to_owned();
    assert_eq!(
        git_head, "refs/heads/main",
        "expected main as default branch HEAD target"
    );

    for refname in ["refs/heads/main", "refs/heads/feature"] {
        let grit_oids = grit_reflog_newest_oid(&stack, refname);
        let git_lines = git_reflog_lines(root.path(), refname);
        assert!(
            !git_lines.is_empty(),
            "git should have reflog for {refname}"
        );
        assert_eq!(
            grit_oids.first(),
            Some(&git_lines[0].1),
            "newest reflog oid mismatch for {refname}"
        );
    }

    let all_logs = stack.read_all_logs().expect("read_all_logs");
    assert!(
        all_logs.iter().any(|l| l.refname == "refs/heads/feature"),
        "read_all_logs should include feature reflog"
    );
}

#[test]
fn grit_written_reftable_passes_git_fsck_and_refs_verify() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = root.path().join(".git");
    let oid1 = git_empty_commit_oid(root.path());
    let oid2 = git_empty_commit_oid(root.path());

    let identity = grit_reflog_identity();
    reftable_write_ref(
        &git_dir,
        "refs/heads/grit-branch",
        &oid1,
        Some(&identity),
        Some("grit write one"),
    )
    .expect("write ref");
    reftable_write_symref(
        &git_dir,
        "refs/heads/grit-sym",
        "refs/heads/grit-branch",
        Some(&identity),
        Some("grit symref"),
    )
    .expect("write symref");
    reftable_write_ref(
        &git_dir,
        "refs/heads/grit-branch",
        &oid2,
        Some(&identity),
        Some("grit update"),
    )
    .expect("update ref");

    let log = reftable_log_record_for_commit(
        &git_dir,
        "refs/heads/grit-branch",
        &oid1,
        &oid2,
        &identity,
        "txn log",
    )
    .expect("log record");
    reftable_write_transaction(
        &git_dir,
        vec![ReftableTransactionUpdate {
            refname: "refs/heads/grit-txn".to_owned(),
            value: Some(RefValue::Val1(oid2)),
            log: Some(log),
            expected_old: None,
        }],
    )
    .expect("write transaction");

    reftable_delete_ref(&git_dir, "refs/heads/grit-txn").expect("delete ref");

    assert!(git_fsck_strict(root.path()), "git fsck --strict");
    git_refs_verify(root.path());

    let show = git_show_ref(root.path());
    assert_eq!(show.get("refs/heads/grit-branch"), Some(&oid2));
    assert_eq!(
        git(root.path(), &["symbolic-ref", "refs/heads/grit-sym"]).trim(),
        "refs/heads/grit-branch"
    );
    let grit_logs = reftable_read_reflog(&git_dir, "refs/heads/grit-branch").expect("grit reflog");
    assert!(
        grit_logs.iter().any(|e| e.message.contains("grit update")),
        "grit reflog should record update message"
    );
    assert!(
        git_reflog_lines(root.path(), "refs/heads/grit-branch").len() >= grit_logs.len(),
        "git should expose reflog entries for grit-written branch"
    );
}

#[test]
fn stack_tables_list_and_update_index_mechanics() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = root.path().join(".git");
    let oid = git_empty_commit_oid(root.path());
    let identity = grit_reflog_identity();

    write_ref_with_autocompaction(
        &git_dir,
        "refs/heads/a",
        &oid,
        Some(&identity),
        Some("a"),
        false,
    )
    .expect("write a");
    write_ref_with_autocompaction(
        &git_dir,
        "refs/heads/b",
        &oid,
        Some(&identity),
        Some("b"),
        false,
    )
    .expect("write b");

    let mut stack = ReftableStack::open(&git_dir).expect("open");
    assert!(stack.table_names().len() >= 2, "expected stacked tables");
    let list_path = git_dir.join("reftable/tables.list");
    let on_disk: Vec<String> = fs::read_to_string(&list_path)
        .expect("tables.list")
        .lines()
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(on_disk, stack.table_names());

    let idx_before = stack.max_update_index().expect("max idx");
    write_ref_with_autocompaction(
        &git_dir,
        "refs/heads/c",
        &oid,
        Some(&identity),
        Some("c"),
        false,
    )
    .expect("write c");
    stack = ReftableStack::open(&git_dir).expect("reopen");
    let idx_after = stack.max_update_index().expect("max idx after");
    assert!(idx_after > idx_before, "update index must increase");

    // Stale table file not listed in tables.list must be ignored.
    let stale_name = "deadbeef-deadbeef-deadbeef.ref";
    let sample_table = git_dir.join("reftable").join(&on_disk[0]);
    fs::copy(&sample_table, git_dir.join("reftable").join(stale_name)).expect("copy stale table");
    let refs_before_stale: BTreeMap<String, ObjectId> = stack
        .read_refs()
        .expect("read before stale")
        .into_iter()
        .filter_map(|r| match r.value {
            RefValue::Val1(oid) | RefValue::Val2(oid, _) => Some((r.name, oid)),
            _ => None,
        })
        .collect();
    let refs_with_stale = stack.read_refs().expect("read with stale file");
    let refs_after_stale: BTreeMap<String, ObjectId> = refs_with_stale
        .into_iter()
        .filter_map(|r| match r.value {
            RefValue::Val1(oid) | RefValue::Val2(oid, _) => Some((r.name, oid)),
            _ => None,
        })
        .collect();
    assert_eq!(
        refs_before_stale, refs_after_stale,
        "unlisted stale table must not change merged refs"
    );
    assert!(
        refs_after_stale.contains_key("refs/heads/c"),
        "stale file must not affect merged view"
    );
    assert!(
        !stack.table_names().contains(&stale_name.to_owned()),
        "stale file must not appear in stack"
    );

    // Held tables.list.lock -> typed error, no mutation.
    let list_before_lock = fs::read_to_string(&list_path).expect("tables.list before lock");
    let lock = git_dir.join("reftable/tables.list.lock");
    fs::write(&lock, b"held").expect("create lock");
    let err = write_ref_with_autocompaction(
        &git_dir,
        "refs/heads/locked",
        &oid,
        Some(&identity),
        Some("should fail"),
        false,
    )
    .expect_err("write with lock held");
    assert!(
        matches!(err, Error::InvalidRef(_)),
        "expected InvalidRef lock error, got {err:?}"
    );
    assert_eq!(
        fs::read_to_string(&list_path).expect("tables.list after failed write"),
        list_before_lock,
        "failed lock must not mutate tables.list"
    );
    let _ = fs::remove_file(&lock);
}

#[test]
fn interleaved_git_and_grit_writers_keep_update_index_monotonic() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = root.path().join(".git");
    let oid = git_empty_commit_oid(root.path());
    let identity = grit_reflog_identity();

    let mut stack = ReftableStack::open(&git_dir).expect("open");
    let mut last = stack.max_update_index().expect("initial max");

    git(
        root.path(),
        &[
            "update-ref",
            "-m",
            "git side",
            "refs/heads/git-side",
            &oid.to_string(),
        ],
    );
    stack = ReftableStack::open(&git_dir).expect("reopen after git");
    let after_git = stack.max_update_index().expect("after git");
    assert!(after_git >= last);
    last = after_git;

    write_ref_with_autocompaction(
        &git_dir,
        "refs/heads/grit-side",
        &oid,
        Some(&identity),
        Some("grit side"),
        false,
    )
    .expect("grit write");
    stack = ReftableStack::open(&git_dir).expect("reopen after grit");
    let after_grit = stack.max_update_index().expect("after grit");
    assert!(after_grit > last);
    last = after_grit;

    git(
        root.path(),
        &[
            "update-ref",
            "-m",
            "git again",
            "refs/heads/git-side-2",
            &oid.to_string(),
        ],
    );
    stack = ReftableStack::open(&git_dir).expect("reopen after git2");
    let after_git2 = stack.max_update_index().expect("after git2");
    assert!(after_git2 > last);
}

#[test]
fn compaction_output_readable_by_git() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = root.path().join(".git");
    let oid = git_empty_commit_oid(root.path());
    let identity = grit_reflog_identity();

    for i in 0..5 {
        write_ref_with_autocompaction(
            &git_dir,
            &format!("refs/heads/compact-{i}"),
            &oid,
            Some(&identity),
            Some("pre-compact"),
            false,
        )
        .expect("write ref");
    }
    let mut stack = ReftableStack::open(&git_dir).expect("open");
    assert!(stack.table_names().len() > 1, "need multiple tables");

    stack.compact().expect("compact");
    assert_eq!(stack.table_names().len(), 1, "compact collapses stack");

    let grit_before: BTreeMap<String, ObjectId> = grit_direct_refs(&stack)
        .into_iter()
        .filter(|(name, _)| name.starts_with("refs/heads/compact-"))
        .collect();
    assert!(git_fsck_strict(root.path()));
    git_refs_verify(root.path());
    let git_after: BTreeMap<String, ObjectId> = git_show_ref(root.path())
        .into_iter()
        .filter(|(name, _)| name.starts_with("refs/heads/compact-"))
        .collect();
    assert_eq!(grit_before, git_after);
}

#[test]
fn auto_compaction_keeps_table_count_bounded() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = root.path().join(".git");
    let oid = git_empty_commit_oid(root.path());
    let identity = grit_reflog_identity();

    for i in 0..12 {
        write_ref_with_autocompaction(
            &git_dir,
            &format!("refs/heads/auto-{i}"),
            &oid,
            Some(&identity),
            Some("auto"),
            true,
        )
        .expect("write");
    }
    let stack = ReftableStack::open(&git_dir).expect("open");
    let table_count = stack.table_names().len();
    assert!(
        table_count <= 4,
        "auto-compaction should keep stack small, got {table_count} tables"
    );
    // Geometric stack: each compaction roughly halves depth; n writes stay O(log n) tables.
    let max_tables = 1 + (12 as f64).log2().ceil() as usize + 1;
    assert!(
        table_count <= max_tables,
        "expected O(log n) tables, got {table_count} (max {max_tables})"
    );
}

#[test]
fn reftable_reflog_api_matches_git() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = root.path().join(".git");
    let oid = git_empty_commit_oid(root.path());
    let identity = grit_reflog_identity();

    reftable_write_ref(
        &git_dir,
        "refs/heads/log-test",
        &oid,
        Some(&identity),
        Some("seed ref"),
    )
    .expect("seed ref");
    reftable_create_reflog(&git_dir, "refs/heads/log-test").expect("create reflog");
    assert!(reftable_reflog_exists(&git_dir, "refs/heads/log-test"));

    let null = ObjectId::null(HashAlgo::Sha1);
    reftable_append_reflog(
        &git_dir,
        "refs/heads/log-test",
        &null,
        &oid,
        &identity,
        "append one",
        true,
    )
    .expect("append");
    reftable_append_reflog(
        &git_dir,
        "refs/heads/log-test",
        &oid,
        &oid,
        &identity,
        "append two",
        true,
    )
    .expect("append2");

    let replaced = vec![ReflogEntry {
        old_oid: null,
        new_oid: oid,
        identity: identity.clone(),
        message: "replaced\n".to_owned(),
    }];
    reftable_replace_reflog(&git_dir, "refs/heads/log-test", &replaced).expect("replace");

    let grit_entries = reftable_read_reflog(&git_dir, "refs/heads/log-test").expect("read");
    let git_lines = git_reflog_lines(root.path(), "refs/heads/log-test");
    assert_eq!(grit_entries.len(), git_lines.len());
    assert_eq!(grit_entries[0].new_oid, git_lines[0].1);

    let listed = reftable_list_reflog_refs(&git_dir).expect("list");
    assert!(listed.iter().any(|r| r == "refs/heads/log-test"));
    assert!(
        git_ok_list_reflog(root.path(), "refs/heads/log-test"),
        "git should list reflog for log-test"
    );

    reftable_delete_reflog(&git_dir, "refs/heads/log-test").expect("delete reflog");
    assert!(!reftable_reflog_exists(&git_dir, "refs/heads/log-test"));
    assert!(
        reftable_read_reflog(&git_dir, "refs/heads/log-test")
            .expect("read after delete")
            .is_empty(),
        "grit should have no log lines after delete"
    );
    assert!(git_fsck_strict(root.path()));
}

fn git_ok_list_reflog(worktree: &Path, refname: &str) -> bool {
    support::git_ok(worktree, &["reflog", "show", refname, "-1", "--format=%H"])
}

#[test]
fn concurrent_grit_writers_serialize_and_preserve_updates() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let git_dir = Arc::new(root.path().join(".git"));
    let oid = git_empty_commit_oid(root.path());
    let identity = Arc::new(grit_reflog_identity());
    let barrier = Arc::new(Barrier::new(2));

    let git_dir_a = Arc::clone(&git_dir);
    let git_dir_b = Arc::clone(&git_dir);
    let id_a = Arc::clone(&identity);
    let id_b = Arc::clone(&identity);
    let bar_a = Arc::clone(&barrier);
    let bar_b = Arc::clone(&barrier);

    let t1 = thread::spawn(move || {
        bar_a.wait();
        reftable_write_ref(
            git_dir_a.as_path(),
            "refs/heads/concurrent-a",
            &oid,
            Some(id_a.as_str()),
            Some("thread a"),
        )
        .expect("thread a write");
    });
    let t2 = thread::spawn(move || {
        bar_b.wait();
        reftable_write_ref(
            git_dir_b.as_path(),
            "refs/heads/concurrent-b",
            &oid,
            Some(id_b.as_str()),
            Some("thread b"),
        )
        .expect("thread b write");
    });
    t1.join().expect("join t1");
    t2.join().expect("join t2");

    let show = git_show_ref(root.path());
    assert_eq!(show.get("refs/heads/concurrent-a"), Some(&oid));
    assert_eq!(show.get("refs/heads/concurrent-b"), Some(&oid));
    assert!(git_fsck_strict(root.path()));
    if git_supports_refs_verify() {
        git_refs_verify(root.path());
    }
}
