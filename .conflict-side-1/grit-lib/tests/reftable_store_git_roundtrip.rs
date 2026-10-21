//! Round-trip reftable [`RefStore`] transactions against system `git`.

mod support;

use std::collections::BTreeMap;

use grit_lib::objects::ObjectId;
use grit_lib::refs::reftable::ReftableStack;
use grit_lib::refs::store::ReftableRefStore;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags, ReflogUpdate,
};

use support::{
    git, git_for_each_ref, git_fsck_strict, git_init_reftable_repo, git_ok,
    git_supports_refs_verify, grit_reflog_identity, require_reftable_git,
};

fn oid(byte: u8) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes[19] = byte;
    ObjectId::from_bytes(&bytes).expect("valid oid")
}

fn sample_log(n: u8) -> ReflogUpdate {
    ReflogUpdate {
        identity: grit_reflog_identity(),
        message: format!("commit: batch {n}"),
        time: time::OffsetDateTime::from_unix_timestamp(1_700_000_000 + i64::from(n))
            .expect("valid timestamp"),
        old_oid: None,
        new_oid: None,
    }
}

#[test]
fn reftable_store_git_roundtrip() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let worktree = root.path();
    let git_dir = worktree.join(".git");

    let store = ReftableRefStore::open(git_dir.clone()).expect("open store");

    let mut txn = RefTransaction::new();
    for i in 0..1000_u16 {
        let name = format!("refs/heads/rt-{i:04}");
        txn = txn
            .update(RefUpdate {
                name,
                new_value: Some(RawRef::Direct(oid(((i % 199) + 1) as u8))),
                expected: Expected::Missing,
                reflog: Some(sample_log(((i % 199) + 1) as u8)),
                flags: RefUpdateFlags::default(),
            })
            .expect("queue update");
    }
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    let grit_refs: BTreeMap<String, ObjectId> = {
        let mut map = BTreeMap::new();
        store
            .for_each_ref("refs/heads/", &mut |entry| {
                if let RawRef::Direct(oid) = entry.value {
                    map.insert(entry.name.clone(), oid);
                }
                std::ops::ControlFlow::Continue(())
            })
            .expect("for_each_ref");
        map
    };
    let git_refs: BTreeMap<String, ObjectId> = git_for_each_ref(worktree, "refs/heads/")
        .into_iter()
        .collect();
    assert_eq!(grit_refs, git_refs, "for-each-ref must match git");

    for i in 0..10_u16 {
        let name = format!("refs/heads/rt-{i:04}");
        if !git_refs.contains_key(&name) {
            continue;
        }
        if !git_ok(worktree, &["reflog", "--format=%H %H %gs", &name]) {
            continue;
        }
        let grit_log = {
            let mut lines = Vec::new();
            store
                .for_each_reflog_entry(&name, true, &mut |e| {
                    lines.push(format!(
                        "{} {} {}",
                        e.old_oid.to_hex(),
                        e.new_oid.to_hex(),
                        e.message
                    ));
                    std::ops::ControlFlow::Continue(())
                })
                .expect("reflog");
            lines
        };
        let git_log = git(worktree, &["reflog", "--format=%H %H %gs", &name]);
        let git_lines: Vec<String> = git_log.lines().map(str::to_owned).collect();
        assert_eq!(
            grit_log.len(),
            git_lines.len(),
            "reflog line count for {name}"
        );
    }

    git_fsck_strict(worktree);
    if git_supports_refs_verify() {
        git(worktree, &["refs", "verify"]);
    }

    // Git-written stack reads back equal through grit.
    git(
        worktree,
        &["commit", "--allow-empty", "-q", "-m", "from-git-old"],
    );
    let oid_old: ObjectId = git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("from-git old");
    git(
        worktree,
        &["commit", "--allow-empty", "-q", "-m", "from-git-new"],
    );
    let oid_new: ObjectId = git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("from-git new");
    git(
        worktree,
        &["update-ref", "refs/heads/from-git", &oid_old.to_hex()],
    );
    git(
        worktree,
        &[
            "update-ref",
            "-m",
            "git write",
            "refs/heads/from-git",
            &oid_new.to_hex(),
            &oid_old.to_hex(),
        ],
    );

    let store2 = ReftableRefStore::open(git_dir.clone()).expect("reopen");
    assert_eq!(
        store2.resolve("refs/heads/from-git").expect("resolve"),
        oid_new
    );
    let mut git_written_log = 0;
    store2
        .for_each_reflog_entry("refs/heads/from-git", false, &mut |_| {
            git_written_log += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("read git reflog");
    assert!(git_written_log >= 1, "git reflog visible in store");

    let stack = ReftableStack::open(&git_dir).expect("stack");
    assert!(
        stack
            .lookup_ref("refs/heads/from-git")
            .expect("lookup")
            .is_some(),
        "stack sees git-written ref"
    );
}

#[test]
fn reftable_reflog_identity_with_digits_in_name_visible_to_git() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let worktree = root.path();
    let git_dir = worktree.join(".git");
    let store = ReftableRefStore::open(git_dir.clone()).expect("open store");
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "first"]);
    let oid_a: ObjectId = git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("first commit");
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "second"]);
    let oid_b: ObjectId = git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("second commit");
    let time = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time");
    let identity = "Alice2 <alice@example.com>";

    let seed = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/digit-name".to_owned(),
            new_value: Some(RawRef::Direct(oid_a)),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        })
        .expect("seed txn");
    store
        .prepare(seed)
        .expect("seed prepare")
        .commit()
        .expect("seed commit");

    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/digit-name".to_owned(),
            new_value: Some(RawRef::Direct(oid_b)),
            expected: Expected::Any,
            reflog: Some(ReflogUpdate {
                identity: identity.to_owned(),
                message: String::new(),
                time,
                old_oid: None,
                new_oid: None,
            }),
            flags: RefUpdateFlags::default(),
        })
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    assert!(
        git_ok(worktree, &["reflog", "show", "-1", "refs/heads/digit-name"],),
        "git must read grit-written reftable reflog"
    );
    let mut identity_line = String::new();
    store
        .for_each_reflog_entry("refs/heads/digit-name", false, &mut |e| {
            identity_line = e.identity.clone();
            std::ops::ControlFlow::Break(())
        })
        .expect("read grit reflog");
    assert!(
        identity_line.contains("Alice2"),
        "grit reflog identity must preserve digits in name: {identity_line}"
    );
}
