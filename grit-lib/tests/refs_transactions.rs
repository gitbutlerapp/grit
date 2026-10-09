//! Ref transactions, CAS batches, D/F conflicts, locking, and reftable atomic writes.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use grit_lib::gc::{update_refs, RefTransactionItem};
use grit_lib::objects::ObjectId;
use grit_lib::refs::{
    delete_ref, lock_path_for_ref, resolve_ref, update_branch_for_commit, write_ref,
    BranchCommitRefUpdate,
};
use grit_lib::reftable::{ReftableReader, ReftableStack};
use grit_lib::repo::init_repository;

use support::{
    duplicate_repo, each_backend, git, git_empty_commit_oid, git_fsck_strict, git_show_ref,
    git_update_ref_stdin, ref_store_bytes_snapshot, reflog_bytes_snapshot,
    reftable_tables_snapshot, Backend,
};

fn two_commits(worktree: &Path) -> (ObjectId, ObjectId) {
    git(
        worktree,
        &["commit", "--allow-empty", "-q", "-m", "refs-tx initial"],
    );
    let c = git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("C");
    git(
        worktree,
        &["commit", "--allow-empty", "-q", "-m", "refs-tx second"],
    );
    let d = git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("D");
    (c, d)
}

#[test]
fn t1404_failed_cas_changes_nothing() {
    each_backend(|_backend, repo| {
        let worktree = repo.worktree();
        let git_dir = repo.git_dir();
        let (c1, c2) = two_commits(worktree);

        git(
            worktree,
            &["update-ref", "refs/heads/to-update", &c1.to_hex()],
        );
        git(
            worktree,
            &["update-ref", "refs/heads/to-delete", &c1.to_hex()],
        );

        let refs_before = ref_store_bytes_snapshot(&git_dir);
        let logs_before = reflog_bytes_snapshot(&git_dir);
        let grit_before = support::grit_refs(worktree);

        let bad = vec![
            RefTransactionItem {
                name: "refs/heads/created".to_owned(),
                new_oid: Some(c2),
                expected_old: None,
            },
            RefTransactionItem {
                name: "refs/heads/to-update".to_owned(),
                new_oid: Some(c2),
                expected_old: Some(c1),
            },
            RefTransactionItem {
                name: "refs/heads/to-delete".to_owned(),
                new_oid: None,
                expected_old: Some(c2),
            },
        ];
        assert!(update_refs(&git_dir, &bad).is_err());

        assert_eq!(ref_store_bytes_snapshot(&git_dir), refs_before);
        assert_eq!(reflog_bytes_snapshot(&git_dir), logs_before);
        assert_eq!(support::grit_refs(worktree), grit_before);
        assert!(git_fsck_strict(worktree));
    });
}

#[derive(Clone, Copy)]
struct DfCase {
    name: &'static str,
    /// `true`: stdin order is create then delete; `false`: delete then create.
    add_del: bool,
    pack: bool,
    /// `true`: the created ref is `…/foo`; `false`: the created ref is `…/foo/bar`.
    add_is_short: bool,
}

const DF_CASES: &[DfCase] = &[
    DfCase {
        name: "add-long-delete-short",
        add_del: true,
        pack: false,
        add_is_short: false,
    },
    DfCase {
        name: "add-short-delete-long",
        add_del: true,
        pack: false,
        add_is_short: true,
    },
    DfCase {
        name: "delete-long-add-short",
        add_del: false,
        pack: false,
        add_is_short: true,
    },
    DfCase {
        name: "delete-short-add-long",
        add_del: false,
        pack: false,
        add_is_short: false,
    },
    DfCase {
        name: "add-long-delete-short-packed",
        add_del: true,
        pack: true,
        add_is_short: false,
    },
    DfCase {
        name: "add-short-delete-long-packed",
        add_del: true,
        pack: true,
        add_is_short: true,
    },
    DfCase {
        name: "delete-long-add-short-packed",
        add_del: false,
        pack: true,
        add_is_short: true,
    },
    DfCase {
        name: "delete-short-add-long-packed",
        add_del: false,
        pack: true,
        add_is_short: false,
    },
];

fn df_refs(prefix: &str, add_is_short: bool) -> (String, String) {
    let short = format!("{prefix}/r/foo");
    let long = format!("{prefix}/r/foo/bar");
    if add_is_short {
        (short, long)
    } else {
        (long, short)
    }
}

#[test]
fn t1404_df_conflicts_match_git_update_ref_stdin() {
    for &case in DF_CASES {
        each_backend(|_backend, repo| {
            let grit_repo = duplicate_repo(&repo);
            let git_repo = duplicate_repo(&repo);

            let prefix = format!("refs/df-{}", case.name);
            let (addref, delref) = df_refs(&prefix, case.add_is_short);
            let mut c = ObjectId::from_hex("0000000000000000000000000000000000000000").unwrap();
            let mut d = c;

            for setup in [&grit_repo, &git_repo] {
                let (c_oid, d_oid) = two_commits(setup.worktree());
                c = c_oid;
                d = d_oid;
                git(setup.worktree(), &["update-ref", &delref, &c.to_hex()]);
                if case.pack {
                    git(setup.worktree(), &["pack-refs", "--all"]);
                }
            }

            let script = if case.add_del {
                format!("create {addref} {}\ndelete {delref}\n", d.to_hex())
            } else {
                format!("delete {delref}\ncreate {addref} {}\n", d.to_hex())
            };

            let git_ok = git_update_ref_stdin(git_repo.worktree(), &script);

            let items = if case.add_del {
                vec![
                    RefTransactionItem {
                        name: addref.clone(),
                        new_oid: Some(d),
                        expected_old: None,
                    },
                    RefTransactionItem {
                        name: delref.clone(),
                        new_oid: None,
                        expected_old: Some(c),
                    },
                ]
            } else {
                vec![
                    RefTransactionItem {
                        name: delref.clone(),
                        new_oid: None,
                        expected_old: Some(c),
                    },
                    RefTransactionItem {
                        name: addref.clone(),
                        new_oid: Some(d),
                        expected_old: None,
                    },
                ]
            };
            let grit_ok = update_refs(&grit_repo.git_dir(), &items).is_ok();

            assert_eq!(
                git_ok,
                grit_ok,
                "df case {} backend {:?}: git accepted={git_ok} grit accepted={grit_ok}",
                case.name,
                grit_repo.backend()
            );

            let expected = BTreeMap::from([(delref.clone(), c)]);
            let grit_r: BTreeMap<String, ObjectId> = support::grit_refs(grit_repo.worktree())
                .into_iter()
                .filter(|(n, _)| n.starts_with(&format!("{prefix}/r/")))
                .collect();
            let git_r: BTreeMap<String, ObjectId> = git_show_ref(git_repo.worktree())
                .into_iter()
                .filter(|(n, _)| n.starts_with(&format!("{prefix}/r/")))
                .collect();
            assert_eq!(grit_r, expected, "grit refs after df case {}", case.name);
            assert_eq!(git_r, expected, "git refs after df case {}", case.name);
        });
    }
}

#[test]
fn ref_lock_present_fails_and_preserves_ref() {
    each_backend(|backend, repo| {
        if backend == Backend::Reftable {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = git_empty_commit_oid(repo.worktree());
        write_ref(&git_dir, "refs/heads/locked", &oid).expect("seed ref");

        let ref_path = git_dir.join("refs/heads/locked");
        let lock = lock_path_for_ref(&ref_path);
        fs::write(&lock, b"held").expect("pre-create lock");

        git(
            repo.worktree(),
            &["commit", "--allow-empty", "-q", "-m", "other"],
        );
        let new_oid: ObjectId = git(repo.worktree(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("oid");

        assert!(write_ref(&git_dir, "refs/heads/locked", &new_oid).is_err());
        assert_eq!(resolve_ref(&git_dir, "refs/heads/locked").unwrap(), oid);
        assert!(delete_ref(&git_dir, "refs/heads/locked").is_err());
        assert_eq!(resolve_ref(&git_dir, "refs/heads/locked").unwrap(), oid);

        fs::remove_file(&lock).expect("drop external lock");
        write_ref(&git_dir, "refs/heads/locked", &new_oid).expect("write after lock cleared");
        assert!(!lock.exists(), "lock file must not remain after success");
        assert_eq!(resolve_ref(&git_dir, "refs/heads/locked").unwrap(), new_oid);
    });
}

fn concurrent_cas_winners(
    git_dir: &Path,
    items: RefTransactionItem,
    expected_winners: usize,
    final_check: impl FnOnce(&Path),
) {
    let n = 32;
    let barrier = Arc::new(Barrier::new(n));
    let wins = Arc::new(AtomicUsize::new(0));
    let git_dir = git_dir.to_path_buf();

    let mut handles = Vec::new();
    for _ in 0..n {
        let git_dir = git_dir.clone();
        let barrier = barrier.clone();
        let wins = wins.clone();
        let items = items.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            if update_refs(&git_dir, std::slice::from_ref(&items)).is_ok() {
                wins.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(
        wins.load(Ordering::SeqCst),
        expected_winners,
        "CAS winner count"
    );
    final_check(&git_dir);
}

#[test]
fn concurrent_cas_single_winner() {
    each_backend(|_backend, repo| {
        let git_dir = repo.git_dir();
        let (c1, c2) = two_commits(repo.worktree());
        write_ref(&git_dir, "refs/heads/cas-race", &c1).expect("seed");

        let item = RefTransactionItem {
            name: "refs/heads/cas-race".to_owned(),
            new_oid: Some(c2),
            expected_old: Some(c1),
        };
        concurrent_cas_winners(&git_dir, item, 1, |gd| {
            assert_eq!(resolve_ref(gd, "refs/heads/cas-race").unwrap(), c2);
        });
        assert!(git_fsck_strict(repo.worktree()));
    });
}

#[test]
fn concurrent_cas_delete_single_winner() {
    each_backend(|backend, repo| {
        if backend == Backend::Reftable {
            return;
        }
        let git_dir = repo.git_dir();
        let (c1, _) = two_commits(repo.worktree());
        write_ref(&git_dir, "refs/heads/cas-del", &c1).expect("seed");

        let item = RefTransactionItem {
            name: "refs/heads/cas-del".to_owned(),
            new_oid: None,
            expected_old: Some(c1),
        };
        concurrent_cas_winners(&git_dir, item, 1, |gd| {
            assert!(resolve_ref(gd, "refs/heads/cas-del").is_err());
        });
        assert!(git_fsck_strict(repo.worktree()));
    });
}

#[test]
fn concurrent_reftable_cas_single_winner() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(tmp.path(), false, "main", None, "reftable").expect("init");
    let git_dir = repo.git_dir.clone();
    let c1: ObjectId = "67bf698f3ab735e92fb011a99cff3497c44d30c1"
        .parse()
        .expect("c1");
    let c2: ObjectId = "1111111111111111111111111111111111111111"
        .parse()
        .expect("c2");
    write_ref(&git_dir, "refs/heads/cas-rt", &c1).expect("seed");

    let item = RefTransactionItem {
        name: "refs/heads/cas-rt".to_owned(),
        new_oid: Some(c2),
        expected_old: Some(c1),
    };
    concurrent_cas_winners(&git_dir, item, 1, |gd| {
        assert_eq!(resolve_ref(gd, "refs/heads/cas-rt").unwrap(), c2);
    });
}

#[test]
fn duplicate_ref_updates_match_git_update_ref_stdin() {
    each_backend(|_backend, repo| {
        let grit_repo = duplicate_repo(&repo);
        let git_repo = duplicate_repo(&repo);
        let (c1, c2) = two_commits(grit_repo.worktree());
        let _ = two_commits(git_repo.worktree());

        let script = format!(
            "create refs/heads/dup {}\ncreate refs/heads/dup {}\n",
            c1.to_hex(),
            c2.to_hex()
        );
        assert!(!git_update_ref_stdin(git_repo.worktree(), &script));

        let items = vec![
            RefTransactionItem {
                name: "refs/heads/dup".to_owned(),
                new_oid: Some(c1),
                expected_old: None,
            },
            RefTransactionItem {
                name: "refs/heads/dup".to_owned(),
                new_oid: Some(c2),
                expected_old: None,
            },
        ];
        assert!(update_refs(&grit_repo.git_dir(), &items).is_err());
        assert!(resolve_ref(&grit_repo.git_dir(), "refs/heads/dup").is_err());
    });
}

#[test]
fn update_branch_for_commit_writes_head_and_branch_reflogs() {
    each_backend(|_backend, repo| {
        let git_dir = repo.git_dir();
        let (c1, c2) = two_commits(repo.worktree());
        write_ref(&git_dir, "refs/heads/main", &c1).expect("branch");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("symref HEAD");

        update_branch_for_commit(
            &git_dir,
            &BranchCommitRefUpdate {
                branch_ref: "refs/heads/main",
                expected_old: Some(c1),
                new_oid: c2,
                identity: "Refs Tx Author <refs-harness@example.com> 1700000000 +0000",
                reflog_message: "commit: refs transaction test",
            },
        )
        .expect("branch update");

        let head_log = git(
            repo.worktree(),
            &["reflog", "show", "--format=%gs", "-1", "HEAD"],
        );
        let branch_log = git(
            repo.worktree(),
            &["reflog", "show", "--format=%gs", "-1", "refs/heads/main"],
        );
        assert!(
            head_log.contains("commit: refs transaction test"),
            "HEAD reflog: {head_log:?}"
        );
        assert!(
            branch_log.contains("commit: refs transaction test"),
            "branch reflog: {branch_log:?}"
        );
        assert_eq!(resolve_ref(&git_dir, "refs/heads/main").unwrap(), c2);
    });
}

#[test]
fn reftable_transaction_one_table_single_update_index() {
    each_backend(|backend, repo| {
        if backend != Backend::Reftable {
            return;
        }
        let git_dir = repo.git_dir();
        let (_, c2) = two_commits(repo.worktree());
        let (list_before, count_before) = reftable_tables_snapshot(&git_dir);

        update_refs(
            &git_dir,
            &[
                RefTransactionItem {
                    name: "refs/heads/tx-a".to_owned(),
                    new_oid: Some(c2),
                    expected_old: None,
                },
                RefTransactionItem {
                    name: "refs/heads/tx-b".to_owned(),
                    new_oid: Some(c2),
                    expected_old: None,
                },
            ],
        )
        .expect("batch write");

        let (list_after, count_after) = reftable_tables_snapshot(&git_dir);
        assert_eq!(count_after, count_before + 1, "one new table appended");
        assert_ne!(list_after, list_before);

        let stack = ReftableStack::open(&git_dir).expect("open stack");
        let max_idx = stack.max_update_index().expect("max idx");
        assert!(max_idx >= 1);

        let list = fs::read_to_string(git_dir.join("reftable/tables.list")).expect("tables.list");
        let table_name = list
            .lines()
            .filter(|l| !l.is_empty())
            .next_back()
            .expect("new table name");
        let data = fs::read(git_dir.join("reftable").join(table_name)).expect("read table");
        let reader = ReftableReader::new(data).expect("reader");
        let refs = reader.read_refs().expect("refs");
        let indices: Vec<u64> = refs
            .iter()
            .filter(|r| r.name.starts_with("refs/heads/tx-"))
            .map(|r| r.update_index)
            .collect();
        assert_eq!(indices.len(), 2);
        assert_eq!(indices[0], indices[1]);
        assert_eq!(indices[0], max_idx);
    });
}
