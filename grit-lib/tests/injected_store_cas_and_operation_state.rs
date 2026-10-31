//! Compare-and-swap ref updates, reflog expiry racing appends, and in-progress operation state
//! on a repository whose object and ref stores are injected (no on-disk refs or objects).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::lockfile::LockWait;
use grit_lib::objects::odb::{store::MemoryStore, OdbBuilder};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitScope};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::prepare_with_lock_wait;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefStoreError, RefTransaction, RefUpdate,
    RefUpdateFlags, ReflogUpdate,
};
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::repo_handles::ExpireOptions;
use grit_lib::state::{
    clear_in_progress, detect_in_progress, operation_blocking_switch,
    wt_status_get_state_for_repository, HeadState, InProgressOperation,
};

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";
const LONG_WAIT: LockWait = LockWait::Backoff {
    budget: Duration::from_secs(30),
};

fn open_injected(root: &Path) -> (Arc<Repository>, Arc<MemoryRefStore>) {
    let (git_dir, work_tree) = (root.join("g.git"), root.join("wt"));
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&work_tree).expect("work tree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n",
    )
    .expect("config");
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    let store = Arc::new(MemoryRefStore::new());
    let repo = RepositoryBuilder::new(RepositoryOptions::with_environment(env))
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(store.clone())
        .initial_branch("main")
        .open(&git_dir, Some(&work_tree))
        .expect("open with injected stores");
    (Arc::new(repo), store)
}

fn commit_n(repo: &Repository, n: usize) -> Vec<ObjectId> {
    let work_tree = repo.work_tree().expect("work tree").to_path_buf();
    (0..n)
        .map(|i| {
            fs::write(work_tree.join("a"), format!("{i}\n")).expect("write");
            stage(
                repo,
                &StageOptions::default(),
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("stage");
            commit_with_scope(
                repo,
                &grit_test_support::commit_request(format!("c{i}"), IDENT_TS),
                CommitScope::Staged,
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("commit")
            .oid
        })
        .collect()
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn expire(repo: &Repository) {
    repo.reflogs()
        .expire(&ExpireOptions {
            now: now(),
            expire: Some("1.year.ago".into()),
            expire_unreachable: Some("1.year.ago".into()),
            all: false,
            dry_run: false,
        })
        .expect("expire");
}

fn reflog_len(store: &dyn RefStore, name: &str) -> usize {
    let mut n = 0;
    store
        .for_each_reflog_entry(name, false, &mut |_| {
            n += 1;
            ControlFlow::Continue(())
        })
        .expect("read reflog");
    n
}

fn set_ref(
    repo: &Repository,
    name: &str,
    new: ObjectId,
    expected: Expected,
    ts: i64,
) -> Result<(), RefStoreError> {
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: name.into(),
            new_value: Some(RawRef::Direct(new)),
            expected,
            reflog: Some(ReflogUpdate {
                identity: "T <t@example.com>".into(),
                message: "update".into(),
                time: time::OffsetDateTime::from_unix_timestamp(ts).expect("timestamp"),
                old_oid: None,
                new_oid: Some(new),
            }),
            flags: RefUpdateFlags::default(),
        })
        .expect("build update");
    prepare_with_lock_wait(repo.refs(), txn, LONG_WAIT)?.commit()
}

#[test]
fn cas_update_has_exactly_one_winner_per_round_on_injected_store() {
    const ROUNDS: usize = 40;
    const RACERS: usize = 4;

    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    let commits = commit_n(&repo, RACERS + 1);
    let (base, targets) = commits.split_first().expect("commits");
    let name = "refs/heads/cas";
    let now = now();

    for round in 0..ROUNDS {
        set_ref(&repo, name, *base, Expected::Any, now).expect("reset ref");
        let before = reflog_len(store.as_ref(), name);
        let barrier = Arc::new(Barrier::new(RACERS));
        let racers: Vec<_> = targets
            .iter()
            .map(|target| {
                let (repo, barrier, target) = (repo.clone(), barrier.clone(), *target);
                let base = *base;
                thread::spawn(move || {
                    barrier.wait();
                    set_ref(&repo, name, target, Expected::Oid(base), now)
                })
            })
            .collect();
        let outcomes: Vec<_> = racers
            .into_iter()
            .map(|racer| racer.join().expect("racer"))
            .collect();

        let winners = outcomes.iter().filter(|o| o.is_ok()).count();
        assert_eq!(winners, 1, "round {round}: {outcomes:?}");
        assert!(
            outcomes
                .iter()
                .filter_map(|o| o.as_ref().err())
                .all(|e| matches!(e, RefStoreError::ExpectedMismatch { .. })),
            "round {round}: losers must fail the compare-and-swap: {outcomes:?}"
        );
        let tip = store.resolve(name).expect("resolve");
        assert!(targets.contains(&tip), "round {round}: tip is a winner's");
        assert_eq!(
            reflog_len(store.as_ref(), name),
            before + 1,
            "round {round}: only the winner appends a reflog entry"
        );
    }
}

#[test]
fn reflog_expire_does_not_lose_concurrent_appends_on_injected_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    let tip = *commit_n(&repo, 1).last().expect("commit");
    let name = "refs/heads/race";
    set_ref(&repo, name, tip, Expected::Any, now()).expect("seed");
    let baseline = reflog_len(store.as_ref(), name);

    let background = {
        let repo = repo.clone();
        thread::spawn(move || {
            for _ in 0..100 {
                set_ref(&repo, name, tip, Expected::Any, 1_500_000_000).expect("stale append");
                expire(&repo);
            }
        })
    };
    let mut fresh = baseline;
    while !background.is_finished() {
        set_ref(&repo, name, tip, Expected::Any, now()).expect("fresh append");
        fresh += 1;
    }
    background.join().expect("background");
    expire(&repo);

    assert_eq!(
        reflog_len(store.as_ref(), name),
        fresh,
        "fresh reflog entries must survive concurrent expiry"
    );
}

#[test]
fn operation_sentinels_are_read_from_the_git_dir_beside_an_injected_ref_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _store) = open_injected(tmp.path());
    let tips = commit_n(&repo, 2);
    assert!(detect_in_progress(repo.git_dir()).is_empty());
    assert_eq!(
        operation_blocking_switch(repo.git_dir()).expect("probe"),
        None
    );

    fs::write(repo.git_dir().join("MERGE_HEAD"), format!("{}\n", tips[0])).expect("MERGE_HEAD");
    fs::write(repo.git_dir().join("MERGE_MSG"), "Merge x\n").expect("MERGE_MSG");
    assert_eq!(
        detect_in_progress(repo.git_dir()),
        [InProgressOperation::Merge]
    );
    assert_eq!(
        operation_blocking_switch(repo.git_dir()).expect("probe"),
        Some(InProgressOperation::Merge)
    );

    let cleared = clear_in_progress(&repo).expect("clear");
    assert_eq!(cleared, [InProgressOperation::Merge]);
    assert!(detect_in_progress(repo.git_dir()).is_empty());
    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
}

#[test]
fn detached_head_label_comes_from_the_injected_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    let tips = commit_n(&repo, 2);
    let (older, newer) = (tips[0], tips[1]);

    set_ref(
        &repo,
        "refs/heads/topic",
        older,
        Expected::Any,
        1_700_000_000,
    )
    .expect("topic");
    let detach = RefTransaction::new()
        .update(RefUpdate {
            name: "HEAD".into(),
            new_value: Some(RawRef::Direct(older)),
            expected: Expected::Any,
            reflog: Some(ReflogUpdate {
                identity: "T <t@example.com>".into(),
                message: "checkout: moving from main to topic".into(),
                time: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
                old_oid: Some(newer),
                new_oid: Some(older),
            }),
            flags: RefUpdateFlags::default(),
        })
        .expect("detach");
    prepare_with_lock_wait(store.as_ref(), detach, LONG_WAIT)
        .expect("prepare")
        .commit()
        .expect("commit");

    let head = HeadState::Detached { oid: older };
    let state = wt_status_get_state_for_repository(&repo, &head, true).expect("state");
    assert_eq!(state.detached_from.as_deref(), Some("topic"));
    assert!(state.detached_at, "HEAD is still at the checked-out commit");

    let moved = HeadState::Detached { oid: newer };
    let state = wt_status_get_state_for_repository(&repo, &moved, true).expect("state");
    assert_eq!(state.detached_from.as_deref(), Some("topic"));
    assert!(!state.detached_at, "HEAD moved off the checked-out commit");

    let git_dir_only =
        grit_lib::state::wt_status_get_state(repo.git_dir(), &head, true).expect("state");
    assert_eq!(
        git_dir_only.detached_from, None,
        "the git-dir variant cannot see an injected store's reflog"
    );

    let without = wt_status_get_state_for_repository(&repo, &head, false).expect("state");
    assert_eq!(without.detached_from, None);
}
