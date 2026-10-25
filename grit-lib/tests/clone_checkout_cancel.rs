//! Clone honours cancellation during the post-fetch checkout phase.

use grit_lib::error::Error;
use grit_lib::progress::{Cancel, FlagCancel, ProgressPhase, ProgressSink};
use grit_lib::remote::clone::{cleanup_failed_clone, clone, CloneError, CloneOptions};
use grit_lib::remote::fetch::Progress;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

struct CancelCheckoutOnFiles {
    flag: FlagCancel,
    seen: AtomicU64,
    limit: u64,
}

impl CancelCheckoutOnFiles {
    fn new(limit: u64) -> Self {
        Self {
            flag: FlagCancel::new(),
            seen: AtomicU64::new(0),
            limit,
        }
    }
}

impl ProgressSink for CancelCheckoutOnFiles {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        let _ = (phase, total);
    }

    fn inc(&mut self, units: u64) {
        if self.flag.is_cancelled() {
            return;
        }
        let prev = self.seen.fetch_add(units, Ordering::Relaxed);
        if prev + units >= self.limit {
            self.flag.cancel();
        }
    }

    fn should_cancel(&self) -> bool {
        self.flag.is_cancelled()
    }
}

impl Progress for CancelCheckoutOnFiles {
    fn message(&mut self, _msg: &[u8]) {}
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git");
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        dir.display()
    );
}

fn seed_many_file_repo(root: &Path, files: usize) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..files {
        std::fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", &format!("c{i}")]);
    }
}

#[test]
fn clone_returns_cancelled_when_checkout_progress_requests_abort() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    seed_many_file_repo(&upstream, 120);

    let dest = tmp.path().join("dest");
    let url = upstream.to_string_lossy().into_owned();
    let mut progress = CancelCheckoutOnFiles::new(5);
    let cancel = progress.flag.clone();
    let err = clone(
        CloneOptions::new(url, dest.clone()),
        &mut progress,
        &cancel,
        None,
    )
    .expect_err("cancel during checkout");
    assert!(
        matches!(err, CloneError::Library(Error::Cancelled)),
        "expected cancelled clone, got {err:?}"
    );
    cleanup_failed_clone(&dest, false);
    assert!(!dest.exists(), "caller cleanup removes partial clone");
}
