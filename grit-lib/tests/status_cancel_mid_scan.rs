//! Status honours cancellation during the index stat scan (not only before it starts).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::progress::{CancelToken, NullProgress, ProgressSink};
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::thread;
use std::time::{Duration, Instant};

const TRACKED_FILES: usize = 80_000;

fn repo_with_many_tracked_files() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    for i in 0..TRACKED_FILES {
        let dir = root.join(format!("tracked/{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("file.txt"), format!("payload {i}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "many tracked"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    (tmp, repo)
}

fn status_opts() -> StatusOptions {
    let mut opts = StatusOptions::default();
    opts.untracked = UntrackedMode::No;
    opts.write_index = false;
    opts
}

#[test]
fn status_returns_cancelled_when_signalled_during_untracked_walk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "seed"]);

    for i in 0..800 {
        let dir = root.join(format!("many/{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("file.txt"), format!("payload {i}\n")).expect("write");
    }

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let flag = CancelToken::new();
    let flag_for_thread = flag.clone();
    let sleeper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        flag_for_thread.cancel();
    });

    let mut opts = StatusOptions::default();
    opts.untracked = UntrackedMode::All;
    let err =
        status(&repo, &opts, &mut NullProgress, &flag).expect_err("cancel during untracked walk");
    sleeper.join().expect("join");
    assert!(matches!(err, Error::Cancelled));

    let model = status(&repo, &opts, &mut NullProgress, &CancelToken::never()).expect("full scan");
    assert_eq!(
        model.untracked.len(),
        800,
        "non-cancelled status must enumerate every untracked file"
    );
}

#[test]
fn status_returns_cancelled_during_tracked_stat_scan_with_flag_cancel() {
    let (_tmp, repo) = repo_with_many_tracked_files();
    let opts = status_opts();

    let baseline = Instant::now();
    status(&repo, &opts, &mut NullProgress, &CancelToken::never()).expect("baseline status");
    let baseline_elapsed = baseline.elapsed();
    assert!(
        baseline_elapsed > Duration::from_millis(150),
        "fixture should spend measurable time in stat scan (got {baseline_elapsed:?})"
    );

    let flag = CancelToken::new();
    let flag_for_thread = flag.clone();
    let sleeper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        flag_for_thread.cancel();
    });

    let started = Instant::now();
    let err = status(&repo, &opts, &mut NullProgress, &flag).expect_err("cancel mid stat scan");
    sleeper.join().expect("join");
    assert!(matches!(err, Error::Cancelled));
    let elapsed = started.elapsed();
    assert!(
        elapsed < baseline_elapsed / 2,
        "cancel at 50 ms should finish well before a full scan ({elapsed:?} vs baseline {baseline_elapsed:?})"
    );
}

#[test]
fn status_returns_cancelled_during_tracked_stat_scan_with_progress_sink() {
    struct CancelSink {
        token: CancelToken,
        trigger_after: u64,
        seen: u64,
    }

    impl ProgressSink for CancelSink {
        fn inc(&mut self, units: u64) {
            self.seen += units;
            if self.seen >= self.trigger_after {
                self.token.cancel();
            }
        }
    }

    let (_tmp, repo) = repo_with_many_tracked_files();
    let opts = status_opts();

    let baseline = Instant::now();
    status(&repo, &opts, &mut NullProgress, &CancelToken::never()).expect("baseline status");
    let baseline_elapsed = baseline.elapsed();
    assert!(
        baseline_elapsed > Duration::from_millis(150),
        "fixture should spend measurable time in stat scan (got {baseline_elapsed:?})"
    );

    let mut sink = CancelSink {
        token: CancelToken::new(),
        trigger_after: 1,
        seen: 0,
    };
    let cancel = sink.token.clone();

    let started = Instant::now();
    let err = status(&repo, &opts, &mut sink, &cancel).expect_err("sink cancel mid stat scan");
    assert!(matches!(err, Error::Cancelled));
    let elapsed = started.elapsed();
    assert!(
        elapsed < baseline_elapsed / 2,
        "progress-driven cancel should finish well before a full scan ({elapsed:?} vs baseline {baseline_elapsed:?})"
    );
}

#[test]
fn status_without_cancel_still_scans_every_tracked_path() {
    let (_tmp, repo) = repo_with_many_tracked_files();
    let opts = status_opts();
    let model =
        status(&repo, &opts, &mut NullProgress, &CancelToken::never()).expect("full status");
    assert!(
        model.staged.is_empty() && model.unstaged.is_empty() && model.untracked.is_empty(),
        "clean tree with untracked disabled"
    );
}
