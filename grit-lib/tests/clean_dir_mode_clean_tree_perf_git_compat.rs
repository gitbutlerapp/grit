//! Issue #2429: `grit clean --dry-run` in directory mode must not scan the index
//! linearly for every directory on a clean tree.
//!
//! ```bash
//! cargo build --release -p grit-cli
//! GRIT_CLEAN_PERF=1 cargo test -p grit-lib --test integration clean_dir_mode_clean_tree_perf_git_compat --release --features full-tests -- --test-threads=1
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::clean::CleanOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

const MAX_DOUBLING_SCALING: f64 = 2.5;
const TIMING_SLACK: Duration = Duration::from_millis(80);
const RUNS: usize = 3;

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Tracked-only tree with `dirs` top-level directories and 100 files each (`issue #2429` layout).
fn fixture(root: &Path, dirs: usize) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "a@b"]);
    git(root, &["config", "user.name", "a"]);
    for d in 0..dirs {
        let sub = format!("s{}", d % 7);
        let dir = root.join(format!("d{d:04}")).join(&sub);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..100 {
            std::fs::write(dir.join(format!("f{f:03}.txt")), format!("file {d} {f}\n"))
                .expect("write");
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn dry_run_clean(root: &Path) -> Duration {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let t0 = Instant::now();
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions {
                directories: true,
                dry_run: true,
                ..CleanOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("clean dry-run");
    assert!(
        outcome.removed.is_empty(),
        "clean tree must list nothing to remove: {:?}",
        outcome.removed
    );
    t0.elapsed()
}

fn median_dry_run(root: &Path) -> Duration {
    let mut samples: Vec<Duration> = (0..RUNS).map(|_| dry_run_clean(root)).collect();
    samples.sort();
    samples[RUNS / 2]
}

#[test]
fn clean_dir_mode_dry_run_on_clean_tree_scales_linearly_with_tracked_files() {
    if !have_git() {
        return;
    }
    let small = tempfile::tempdir().expect("tempdir");
    let large = tempfile::tempdir().expect("tempdir");
    fixture(small.path(), 200);
    fixture(large.path(), 400);

    let t_small = median_dry_run(small.path());
    let t_large = median_dry_run(large.path());
    eprintln!("clean dir-mode dry-run on clean tree: 20k files {t_small:?}, 40k files {t_large:?}");
    assert!(
        t_large.as_secs_f64()
            <= MAX_DOUBLING_SCALING * t_small.as_secs_f64() + TIMING_SLACK.as_secs_f64(),
        "doubling tracked files from 20k to 40k took {t_small:?} -> {t_large:?}; \
         directory-mode clean dry-run on a clean tree must be linear in file count (issue #2429)"
    );
}

#[test]
fn clean_dir_mode_dry_run_stays_near_git_on_100k_clean_tree() {
    if std::env::var("GRIT_CLEAN_PERF").ok().as_deref() != Some("1") || !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fixture(root, 1000);

    let grit_bin = std::env::var("GRIT_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let target = std::env::var("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target")
                });
            target
                .join("release")
                .join(format!("grit{}", std::env::consts::EXE_SUFFIX))
        });
    assert!(
        grit_bin.is_file(),
        "grit binary not found at {}; run `cargo build --release -p grit-cli` or set GRIT_BIN",
        grit_bin.display()
    );

    let time_cmd = |program: &Path, args: &[&str]| {
        let mut samples: Vec<Duration> = (0..RUNS)
            .map(|_| {
                let t0 = Instant::now();
                let st = Command::new(program)
                    .args(args)
                    .current_dir(root)
                    .stdout(std::process::Stdio::null())
                    .status()
                    .expect("spawn");
                assert!(st.success());
                t0.elapsed()
            })
            .collect();
        samples.sort();
        samples[RUNS / 2]
    };

    let grit_t = time_cmd(&grit_bin, &["clean", "--dry-run"]);
    let git_t = time_cmd(Path::new("git"), &["clean", "-n"]);
    eprintln!("100k clean tree dry-run: grit {grit_t:?}, git {git_t:?}");
    assert!(
        grit_t.as_secs_f64() < 5.0 * git_t.as_secs_f64() + 0.15,
        "grit clean --dry-run ({grit_t:?}) should stay within a small multiple of git clean -n ({git_t:?})"
    );
}
