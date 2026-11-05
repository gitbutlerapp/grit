//! Merge branch moves must commit branch tip and reflogs atomically (issue #2503).

use std::fmt;
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::branch::{create_branch, CreateBranchOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::merge::{integrate_branch_with, IntegrateBranchOutcome, MergeOptions};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::{
    BackendError, MemoryRefStore, PreparedRefTransaction, RawRef, RefEntry, RefStore,
    RefStoreError, RefTransaction,
};
use grit_lib::repo::{Repository, RepositoryBuilder};

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";

#[derive(Default)]
struct TxnLog(Mutex<Vec<Vec<(String, bool, Option<String>)>>>);

struct RefStoreProbe {
    inner: Arc<MemoryRefStore>,
    txns: Arc<TxnLog>,
    fail_log_only_commit: Arc<AtomicBool>,
}

impl fmt::Debug for RefStoreProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefStoreProbe").finish_non_exhaustive()
    }
}

impl RefStoreProbe {
    fn wrap(inner: Arc<MemoryRefStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            txns: Arc::new(TxnLog::default()),
            fail_log_only_commit: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl RefStore for RefStoreProbe {
    fn format(&self) -> grit_lib::refs::store::RefStorageFormat {
        self.inner.format()
    }

    fn read_raw(&self, name: &str) -> grit_lib::error::Result<Option<RawRef>> {
        self.inner.read_raw(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_ref(prefix, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> std::result::Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        let snapshot = txn
            .updates()
            .iter()
            .map(|u| {
                (
                    u.name.clone(),
                    u.flags.log_only,
                    u.reflog.as_ref().map(|r| r.message.clone()),
                )
            })
            .collect::<Vec<_>>();
        self.txns
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(snapshot);
        let has_log_only = txn.updates().iter().any(|u| u.flags.log_only);
        let prepared = self.inner.prepare(txn)?;
        Ok(Box::new(ProbePrepared {
            inner: prepared,
            fail_log_only: has_log_only,
            fail_flag: self.fail_log_only_commit.clone(),
        }))
    }

    fn reflog_exists(&self, name: &str) -> grit_lib::error::Result<bool> {
        self.inner.reflog_exists(name)
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &str) -> grit_lib::error::Result<()> {
        self.inner.create_reflog(name)
    }

    fn delete_reflog(&self, name: &str) -> grit_lib::error::Result<()> {
        self.inner.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> grit_lib::error::Result<()> {
        self.inner.replace_reflog(name, entries)
    }

    fn rewrite_reflog(
        &self,
        name: &str,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> grit_lib::error::Result<Vec<ReflogEntry>>,
    ) -> grit_lib::error::Result<()> {
        self.inner.rewrite_reflog(name, transform)
    }

    fn for_each_reflog_ref(
        &self,
        f: &mut dyn FnMut(&str) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_reflog_ref(f)
    }
}

struct ProbePrepared<'a> {
    inner: Box<dyn PreparedRefTransaction + Send + 'a>,
    fail_log_only: bool,
    fail_flag: Arc<AtomicBool>,
}

impl PreparedRefTransaction for ProbePrepared<'_> {
    fn commit(self: Box<Self>) -> std::result::Result<(), RefStoreError> {
        let ProbePrepared {
            inner,
            fail_log_only,
            fail_flag,
        } = *self;
        if fail_log_only && fail_flag.load(Ordering::SeqCst) {
            let _ = inner.abort();
            return Err(RefStoreError::Backend(BackendError::new(Error::Io(
                io::Error::other("boom"),
            ))));
        }
        inner.commit()
    }

    fn rejected_updates(&self) -> &[(String, RefStoreError)] {
        self.inner.rejected_updates()
    }

    fn abort(self: Box<Self>) -> std::result::Result<(), RefStoreError> {
        let ProbePrepared { inner, .. } = *self;
        inner.abort()
    }
}

fn memory_repo_with_ref_probe(
    dir: &tempfile::TempDir,
) -> (Repository, Arc<MemoryStore>, Arc<RefStoreProbe>) {
    let git_dir = dir.path().join("mem.git");
    let wt = dir.path().join("wt");
    fs::create_dir_all(&git_dir).expect("mkdir git dir");
    fs::create_dir_all(&wt).expect("mkdir worktree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n",
    )
    .expect("config");
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let ref_inner = Arc::new(MemoryRefStore::new());
    let ref_store = RefStoreProbe::wrap(ref_inner);
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store.clone())
                .alternates(false),
        )
        .ref_store(ref_store.clone())
        .initial_branch("main")
        .open(&git_dir, Some(&wt))
        .expect("open");
    (repo, store, ref_store)
}

fn commit_all(repo: &Repository, message: &str) -> ObjectId {
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    let mut req = CommitRequest::new(message);
    req.author = Some(IDENT_TS.parse().expect("signature"));
    req.committer = Some(IDENT_TS.parse().expect("signature"));
    commit_with_scope(
        repo,
        &req,
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit")
    .oid
}

fn switch_to(repo: &Repository, branch: &str) {
    let oid = repo
        .rev_walk()
        .resolve(&format!("refs/heads/{branch}"))
        .expect("branch tip");
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: branch.into(),
                oid,
            },
            dry_run: false,
            hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("switch");
}

fn diverged_merge_fixture(repo: &Repository, wt: &std::path::Path) -> (ObjectId, ObjectId) {
    fs::write(wt.join("a.txt"), "c1\n").expect("write");
    commit_all(repo, "c1");
    create_branch(repo, &CreateBranchOptions::at_head("feat")).expect("branch");

    switch_to(repo, "feat");
    fs::write(wt.join("b.txt"), "feat1\n").expect("write");
    let feat_tip = commit_all(repo, "feat1");

    switch_to(repo, "main");
    fs::write(wt.join("c.txt"), "main1\n").expect("write");
    let main_tip = commit_all(repo, "main1");
    (main_tip, feat_tip)
}

fn last_reflog_message(store: &dyn RefStore, name: &str) -> Option<String> {
    let mut last = None;
    store
        .for_each_reflog_entry(name, false, &mut |e| {
            last = Some(e.message.clone());
            ControlFlow::Continue(())
        })
        .expect("reflog");
    last
}

#[test]
fn merge_ref_update_rolls_back_when_reflog_commit_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _store, ref_store) = memory_repo_with_ref_probe(&dir);
    let wt = dir.path().join("wt");
    let (main_tip, feat_tip) = diverged_merge_fixture(&repo, &wt);

    ref_store.fail_log_only_commit.store(true, Ordering::SeqCst);
    let err = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main_tip,
        feat_tip,
        "feat",
        &MergeOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect_err("merge fails when reflog transaction cannot commit");
    assert!(matches!(err, Error::RefStore(_)), "{err:?}");

    assert_eq!(
        ref_store
            .inner
            .read_raw("refs/heads/main")
            .expect("main ref"),
        Some(RawRef::Direct(main_tip))
    );
    assert_ne!(
        last_reflog_message(ref_store.as_ref(), "refs/heads/main"),
        Some("merge feat".into())
    );
    assert_ne!(
        last_reflog_message(ref_store.as_ref(), "HEAD"),
        Some("merge feat".into())
    );
}

#[test]
fn merge_branch_move_and_reflogs_share_one_transaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _store, ref_store) = memory_repo_with_ref_probe(&dir);
    let wt = dir.path().join("wt");
    let (main_tip, feat_tip) = diverged_merge_fixture(&repo, &wt);

    ref_store
        .txns
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main_tip,
        feat_tip,
        "feat",
        &MergeOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    let IntegrateBranchOutcome::Merged { oid: merge_oid, .. } = outcome else {
        panic!("expected merge commit, got {outcome:?}");
    };

    assert_eq!(
        ref_store
            .inner
            .read_raw("refs/heads/main")
            .expect("main ref"),
        Some(RawRef::Direct(merge_oid))
    );
    assert_eq!(
        last_reflog_message(ref_store.as_ref(), "refs/heads/main"),
        Some("merge feat".into())
    );
    assert_eq!(
        last_reflog_message(ref_store.as_ref(), "HEAD"),
        Some("merge feat".into())
    );

    let txns = ref_store
        .txns
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let branch_txns = txns
        .iter()
        .filter(|txn| txn.iter().any(|(name, _, _)| name == "refs/heads/main"))
        .collect::<Vec<_>>();
    assert_eq!(branch_txns.len(), 1, "branch move txns: {txns:?}");
    let txn = branch_txns[0];
    assert_eq!(txn.len(), 2);
    assert!(txn.iter().any(|(name, log_only, msg)| {
        name == "refs/heads/main" && !log_only && msg.as_deref() == Some("merge feat")
    }));
    assert!(txn
        .iter()
        .any(|(name, log_only, _)| name == "HEAD" && *log_only));
}
