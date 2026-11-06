//! Clone honours cancellation during the post-fetch checkout phase.

use grit_lib::error::Error;
use grit_lib::progress::{CancelToken, ProgressPhase, ProgressSink};
use grit_lib::remote::clone::{cleanup_failed_clone, clone, CloneError, CloneOptions};
use grit_lib::remote::fetch::Progress;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct CancelCheckoutOnFiles {
    flag: CancelToken,
    checkout_incs: AtomicU64,
    limit: u64,
    in_checkout: AtomicBool,
}

impl CancelCheckoutOnFiles {
    fn new(limit: u64) -> Self {
        Self {
            flag: CancelToken::new(),
            checkout_incs: AtomicU64::new(0),
            limit,
            in_checkout: AtomicBool::new(false),
        }
    }
}

impl ProgressSink for CancelCheckoutOnFiles {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        let _ = total;
        self.in_checkout
            .store(phase == ProgressPhase::CheckingOutFiles, Ordering::Relaxed);
    }

    fn inc(&mut self, units: u64) {
        if self.flag.is_cancelled() {
            return;
        }
        if !self.in_checkout.load(Ordering::Relaxed) {
            return;
        }
        let prev = self.checkout_incs.fetch_add(units, Ordering::Relaxed);
        if prev + units >= self.limit {
            self.flag.cancel();
        }
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

/// Files under `dir` this process has memory-mapped, from `/proc/self/maps`.
///
/// Windows cannot delete a mapped file, so cleanup must not leave the clone's packs mapped.
#[cfg(target_os = "linux")]
fn mapped_files_under(dir: &Path) -> Vec<String> {
    let dir = dir.to_string_lossy().into_owned();
    std::fs::read_to_string("/proc/self/maps")
        .expect("read /proc/self/maps")
        .lines()
        .filter(|line| line.contains(&dir))
        .map(str::to_owned)
        .collect()
}

fn count_checkout_files(dest: &Path, total: usize) -> usize {
    (0..total)
        .filter(|i| dest.join(format!("f{i}.txt")).exists())
        .count()
}

#[test]
fn clone_returns_cancelled_when_checkout_progress_requests_abort() {
    const FILES: usize = 500;
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    seed_many_file_repo(&upstream, FILES);
    git(&upstream, &["repack", "-adq"]);

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
    let checkout_incs = progress.checkout_incs.load(Ordering::Relaxed);
    assert!(
        checkout_incs <= 15,
        "expected bounded CheckingOutFiles progress, saw {checkout_incs} increments"
    );
    let present = count_checkout_files(&dest, FILES);
    assert!(
        present < FILES,
        "checkout should stop before all {FILES} files exist, found {present}"
    );
    #[cfg(target_os = "linux")]
    assert!(
        !mapped_files_under(&dest).is_empty(),
        "the checkout read objects from the new pack"
    );
    cleanup_failed_clone(&dest, false);
    assert!(!dest.exists(), "caller cleanup removes partial clone");
    #[cfg(target_os = "linux")]
    assert_eq!(
        mapped_files_under(&dest),
        Vec::<String>::new(),
        "cleanup releases the clone's pack maps"
    );
}
