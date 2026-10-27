//! Hard reset honours cancellation while checking out files.

use crate::support;

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::progress::{Cancel, FlagCancel, ProgressPhase, ProgressSink};
use grit_lib::refs;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::{objects, RefStorageFormat};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct CancelHardResetOnFiles {
    flag: FlagCancel,
    checkout_incs: AtomicU64,
    limit: u64,
    in_checkout: AtomicBool,
}

impl CancelHardResetOnFiles {
    fn new(limit: u64) -> Self {
        Self {
            flag: FlagCancel::new(),
            checkout_incs: AtomicU64::new(0),
            limit,
            in_checkout: AtomicBool::new(false),
        }
    }
}

impl ProgressSink for CancelHardResetOnFiles {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        let _ = total;
        self.in_checkout
            .store(phase == ProgressPhase::CheckingOutFiles, Ordering::Relaxed);
    }

    fn inc(&mut self, units: u64) {
        if self.flag.is_cancelled() || !self.in_checkout.load(Ordering::Relaxed) {
            return;
        }
        let prev = self.checkout_incs.fetch_add(units, Ordering::Relaxed);
        if prev + units >= self.limit {
            self.flag.cancel();
        }
    }

    fn should_cancel(&self) -> bool {
        self.flag.is_cancelled()
    }
}

impl Cancel for CancelHardResetOnFiles {
    fn is_cancelled(&self) -> bool {
        self.flag.is_cancelled()
    }
}

fn reset_identity() -> grit_lib::ident::Signature {
    support::parse_reflog_signature("T <t@t> 0 +0000")
}

fn identity_line() -> String {
    "T <t@t> 0 +0000".into()
}

fn commit_file(
    repo: &Repository,
    path: &str,
    contents: &str,
    parent: Option<ObjectId>,
) -> ObjectId {
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join(path), contents).expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let oid = repo
        .odb()
        .write(objects::ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    let entry = grit_lib::index::entry_from_stat(
        &wt.join(path),
        path.as_bytes(),
        oid,
        grit_lib::index::MODE_REGULAR,
    )
    .expect("entry");
    index.add_or_replace(entry);
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = objects::CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: identity_line(),
        committer: identity_line(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let co = repo
        .odb()
        .write(
            objects::ObjectKind::Commit,
            &objects::serialize_commit(&commit),
        )
        .expect("commit");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &co).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
    co
}

fn seed_many_file_repo(repo: &Repository, files: usize) -> ObjectId {
    let mut parent: Option<ObjectId> = None;
    for i in 0..files {
        parent = Some(commit_file(
            repo,
            &format!("f{i}.txt"),
            &format!("{i}\n"),
            parent,
        ));
    }
    parent.expect("commits")
}

#[test]
fn hard_reset_returns_cancelled_and_stops_checkout_early() {
    const FILES: usize = 120;
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let head = seed_many_file_repo(&repo, FILES);

    let first = grit_lib::revwalk::rev_parse::resolve_revision_as_commit_without_index_dwim(
        &repo, "HEAD~119",
    )
    .expect("first commit");

    reset_to_commit(
        &repo,
        &ResetOptions::new(first, ResetMode::Hard, reset_identity(), "reset"),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset to first");

    let mut progress = CancelHardResetOnFiles::new(5);
    let cancel = progress.flag.clone();
    let err = reset_to_commit(
        &repo,
        &ResetOptions::new(head, ResetMode::Hard, reset_identity(), "reset"),
        &mut progress,
        &cancel,
    )
    .expect_err("cancel during hard reset checkout");
    assert!(
        matches!(err, grit_lib::error::Error::Cancelled),
        "expected cancelled reset, got {err:?}"
    );
    let checkout_incs = progress.checkout_incs.load(Ordering::Relaxed);
    assert!(
        checkout_incs <= 15,
        "expected bounded checkout progress after cancel, saw {checkout_incs} file increments"
    );
    let wt = repo.work_tree().expect("work tree");
    let present = (0..FILES)
        .filter(|i| wt.join(format!("f{i}.txt")).exists())
        .count();
    assert!(
        present < FILES,
        "hard reset should stop before writing every file, found {present}/{FILES}"
    );
}

#[test]
fn hard_reset_backward_returns_cancelled_and_stops_deletes_early() {
    const FILES: usize = 120;
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let head = seed_many_file_repo(&repo, FILES);
    let first = grit_lib::revwalk::rev_parse::resolve_revision_as_commit_without_index_dwim(
        &repo, "HEAD~119",
    )
    .expect("first commit");

    let mut progress = CancelHardResetOnFiles::new(5);
    let cancel = progress.flag.clone();
    let err = reset_to_commit(
        &repo,
        &ResetOptions::new(first, ResetMode::Hard, reset_identity(), "reset"),
        &mut progress,
        &cancel,
    )
    .expect_err("cancel during backward hard reset deletes");
    assert!(
        matches!(err, grit_lib::error::Error::Cancelled),
        "expected cancelled reset, got {err:?}"
    );
    let checkout_incs = progress.checkout_incs.load(Ordering::Relaxed);
    assert!(
        (5..=15).contains(&checkout_incs),
        "expected bounded delete progress, saw {checkout_incs} increments"
    );
    // A cancelled checkout restores the files it already deleted (delete rollback), so the
    // work tree and HEAD are left exactly as they were before the reset.
    let wt = repo.work_tree().expect("work tree");
    let present = (0..FILES)
        .filter(|i| wt.join(format!("f{i}.txt")).exists())
        .count();
    assert_eq!(
        present, FILES,
        "cancelled backward reset must roll back its deletions"
    );
    assert_eq!(
        refs::resolve_ref(repo.git_dir(), "HEAD").expect("HEAD"),
        head,
        "cancelled reset must not move HEAD"
    );
}
