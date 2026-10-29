//! Replay checkout honours cancellation on the first step.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayOptions, ReplaySequenceRequest,
};
use grit_lib::progress::{Cancel, FlagCancel, ProgressPhase, ProgressSink};
use grit_lib::repo::Repository;

struct CancelReplayOnCheckout {
    flag: FlagCancel,
    checkout_incs: AtomicU64,
    limit: u64,
    in_checkout: AtomicBool,
}

impl CancelReplayOnCheckout {
    fn new(limit: u64) -> Self {
        Self {
            flag: FlagCancel::new(),
            checkout_incs: AtomicU64::new(0),
            limit,
            in_checkout: AtomicBool::new(false),
        }
    }
}

impl ProgressSink for CancelReplayOnCheckout {
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

impl Cancel for CancelReplayOnCheckout {
    fn is_cancelled(&self) -> bool {
        self.flag.is_cancelled()
    }
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse(root: &Path, rev: &str) -> ObjectId {
    let out = git_output(root, &["rev-parse", rev]);
    ObjectId::from_hex(out.trim()).expect("oid")
}

fn git_output(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seed_many_files(root: &Path, files: usize) {
    for i in 0..files {
        fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "seed"]);
}

#[test]
fn replay_pick_cancel_on_first_step_clears_state_and_locks() {
    const FILES: usize = 120;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    seed_many_files(root, FILES);
    let base = rev_parse(root, "HEAD");

    git(root, &["checkout", "-q", "-b", "side"]);
    for i in 0..FILES {
        fs::write(root.join(format!("f{i}.txt")), format!("picked {i}\n")).expect("write");
    }
    git(root, &["commit", "-qam", "pick"]);
    let pick = rev_parse(root, "HEAD");
    git(root, &["checkout", "-q", "main"]);

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let committer = "Test <t@example.com> 1700000000 +0000"
        .parse()
        .expect("ident");
    let req = ReplaySequenceRequest {
        commits: vec![pick],
        direction: ReplayDirection::Pick,
        committer,
        options: ReplayOptions::default(),
    };

    let mut progress = CancelReplayOnCheckout::new(5);
    let cancel = progress.flag.clone();
    let err = start_replay_sequence(&repo, &req, &mut progress, &cancel).expect_err("cancel");
    assert!(
        matches!(err, grit_lib::error::Error::Cancelled),
        "expected cancelled pick, got {err:?}"
    );
    assert!(
        !repo.git_dir().join("index.lock").exists(),
        "cancelled replay left index.lock"
    );
    assert!(
        !repo.git_dir().join("CHERRY_PICK_HEAD").exists(),
        "cancelled replay left CHERRY_PICK_HEAD"
    );
    assert_eq!(
        rev_parse(root, "HEAD"),
        base,
        "branch moved during cancelled pick"
    );
}
