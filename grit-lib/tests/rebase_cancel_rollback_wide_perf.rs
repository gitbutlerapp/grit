//! Issue #2475: cancel during rebase apply rollback should stay near checkout time, not
//! re-walk the old tree once per path.
//!
//! `GRIT_REBASE_CANCEL_PERF=1 cargo test -p grit-lib --test rebase_cancel_rollback_wide_perf --release`

use std::time::{Duration, Instant};

use grit_lib::porcelain::commit::{commit_with_scope, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress, ProgressPhase, ProgressSink};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::history::{
    start_rebase, ApplyOptions, EmptyPolicy, RebaseOntoOptions, RebaseRequest, RebaseSessionError,
    RefScope, RewriteOptions, StartRebaseOptions,
};

const FILES: usize = 300;
const MAX_LATE_CANCEL_RATIO: f64 = 3.0;

struct CancelAfterCheckoutUnits {
    seen: u64,
    active: bool,
    flag: CancelToken,
}

impl CancelAfterCheckoutUnits {
    fn new() -> Self {
        Self {
            seen: 0,
            active: false,
            flag: CancelToken::new(),
        }
    }
}

impl ProgressSink for CancelAfterCheckoutUnits {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.active = phase == ProgressPhase::CheckingOutFiles;
        self.seen = 0;
    }

    fn inc(&mut self, units: u64) {
        if !self.active {
            return;
        }
        self.seen += units;
        if self.seen >= 5 {
            self.flag.cancel();
        }
    }
}

fn write_files(root: &std::path::Path, prefix: &str, tag: &str) {
    std::fs::create_dir_all(root.join(prefix)).expect("mkdir");
    for i in 0..FILES {
        std::fs::write(
            root.join(format!("{prefix}/f{i:04}.txt")),
            format!("{tag} {i}\n"),
        )
        .expect("write");
    }
}

fn repo_for_rebase() -> (
    tempfile::TempDir,
    Repository,
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    std::fs::write(tmp.path().join("base.txt"), "base\n").expect("write");
    let base = commit_all(&repo, "base");
    write_files(tmp.path(), "side", "side");
    let side = commit_all(&repo, "side");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/side", &side).expect("side");
    grit_lib::porcelain::reset::reset_to_commit(
        &repo,
        &grit_lib::porcelain::reset::ResetOptions::new(
            base,
            grit_lib::porcelain::reset::ResetMode::Hard,
            IDENT.parse().expect("sig"),
            "reset",
        ),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("reset");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/topic", &base).expect("topic");
    grit_lib::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/topic").expect("HEAD");
    std::fs::write(tmp.path().join("topic.txt"), "topic\n").expect("write");
    let topic = commit_all(&repo, "topic");
    (tmp, repo, side, topic)
}

const IDENT: &str = "T <t@t> 0 +0000";

fn commit_all(repo: &Repository, msg: &str) -> grit_lib::objects::ObjectId {
    commit_with_scope(
        repo,
        &grit_test_support::commit_request(msg, IDENT),
        CommitScope::AutoStage,
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit")
    .oid
}

fn run_rebase(
    repo: &Repository,
    side: grit_lib::objects::ObjectId,
    topic: grit_lib::objects::ObjectId,
    cancel_at_checkout: bool,
) -> Result<Duration, RebaseSessionError> {
    let opts = StartRebaseOptions {
        request: RebaseRequest {
            upstream: side,
            onto: None,
            branch_tip: topic,
        },
        rebase: RebaseOntoOptions {
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 0 +0000".parse().expect("sig"),
                preserve_committer: false,
                sign: false,
            },
            skip_applied: false,
            flatten_leaf_merges: false,
            favor: grit_lib::diff::merge_file::MergeFavor::default(),
            drop: Vec::new(),
        },
        apply: ApplyOptions {
            identity: "T <t@t> 0 +0000".parse().expect("sig"),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::command_runner::hooks::PostOperationHooks::Skip,
        },
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };
    let start = Instant::now();
    let result = if cancel_at_checkout {
        let mut sink = CancelAfterCheckoutUnits::new();
        let cancel = sink.flag.clone();
        start_rebase(repo, &opts, &mut sink, &cancel)
    } else {
        start_rebase(
            repo,
            &opts,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
    };
    let elapsed = start.elapsed();
    match result {
        Ok(_) if !cancel_at_checkout => Ok(elapsed),
        Err(e) if cancel_at_checkout && e.is_cancelled() => Ok(elapsed),
        Ok(_) => Err(RebaseSessionError::Library(
            grit_lib::error::Error::Message("expected cancelled rebase".into()),
        )),
        Err(e) => Err(e),
    }
}

#[test]
fn rebase_late_cancel_rollback_wall_time_bounded() {
    if std::env::var("GRIT_REBASE_CANCEL_PERF").ok().as_deref() != Some("1") {
        eprintln!("skip rebase_cancel_rollback_wide_perf (set GRIT_REBASE_CANCEL_PERF=1)");
        return;
    }
    let (_tmp, repo, side, topic) = repo_for_rebase();
    let (_tmp2, repo2, side2, topic2) = repo_for_rebase();

    let baseline = run_rebase(&repo, side, topic, false).expect("uncancelled rebase");
    eprintln!("uncancelled rebase: {baseline:?}");

    let cancelled = run_rebase(&repo2, side2, topic2, true).expect("cancelled rebase");
    eprintln!("cancel at CheckingOutFiles start: {cancelled:?}");

    assert!(
        cancelled.as_secs_f64() <= baseline.as_secs_f64() * MAX_LATE_CANCEL_RATIO,
        "cancel took {:?}, uncancelled {:?}, limit {MAX_LATE_CANCEL_RATIO}",
        cancelled,
        baseline
    );
}
