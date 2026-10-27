//! Issue #2106: with `core.ignorecase=true` (the Windows and macOS default), staging a clean
//! tree must cost one pass over the work tree, not one `readdir` per path component.
//!
//! ```bash
//! cargo build --release -p grit-cli
//! GRIT_ADD_PERF=1 cargo test -p grit-lib --test add_ignorecase_clean_tree_perf_git_compat --release -- --test-threads=1
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageOptions, StageOutcome};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

/// Doubling the tracked file count may grow `stage` time by at most this factor.
const MAX_DOUBLING_SCALING: f64 = 2.5;
/// Absolute slack for timer noise on fast machines.
const TIMING_SLACK: Duration = Duration::from_millis(60);
const RUNS: usize = 3;

/// Repository with `files` tracked blobs committed by `git fast-import`, `core.ignorecase=true`.
///
/// `layout` maps a file number to its repository-relative path.
fn fixture(root: &Path, files: usize, layout: impl Fn(usize) -> String) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    let mut script = String::from(
        "commit refs/heads/main\ncommitter t <t@example.com> 1700000000 +0000\ndata 4\ninit\n",
    );
    for i in 0..files {
        let content = format!("file {i}\n");
        script.push_str(&format!(
            "M 100644 inline {}\ndata {}\n{content}\n",
            layout(i),
            content.len()
        ));
    }
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write fast-import stream");
    assert!(child.wait().expect("fast-import").success());
    git(root, &["checkout", "-q", "-f", "main"]);
}

/// The layout from the issue: `d{i%100}/sub{i%7}/f{i}.txt`.
fn issue_layout(i: usize) -> String {
    format!("d{}/sub{}/f{i}.txt", i % 100, i % 7)
}

/// Few wide directories, so a per-path `readdir` costs O(files) and the total O(files²).
fn wide_layout(i: usize) -> String {
    format!("w{}/f{i}.txt", i % 4)
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repository")
}

fn stage_all(root: &Path) -> (StageOutcome, Duration) {
    let repo = open(root);
    let t0 = Instant::now();
    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    (outcome, t0.elapsed())
}

fn assert_nothing_staged(outcome: &StageOutcome) {
    assert!(
        outcome.added.is_empty() && outcome.modified.is_empty() && outcome.removed.is_empty(),
        "clean tree must stage nothing: {outcome:?}"
    );
}

fn median_clean_stage(root: &Path) -> Duration {
    let mut samples: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let (outcome, elapsed) = stage_all(root);
            assert_nothing_staged(&outcome);
            elapsed
        })
        .collect();
    samples.sort();
    samples[RUNS / 2]
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn ls_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files"])
        .current_dir(root)
        .output()
        .expect("git ls-files");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn is_case_insensitive(dir: &Path) -> bool {
    let probe = dir.join(".grit_icase_probe");
    std::fs::write(&probe, b"").expect("write probe");
    let folded = dir.join(".GRIT_ICASE_PROBE").exists();
    std::fs::remove_file(&probe).expect("remove probe");
    folded
}

#[test]
fn clean_ignorecase_tree_stages_nothing_in_linear_time() {
    if !have_git() {
        return;
    }
    let small = tempfile::tempdir().expect("tempdir");
    let large = tempfile::tempdir().expect("tempdir");
    fixture(small.path(), 2_000, wide_layout);
    fixture(large.path(), 4_000, wide_layout);

    let first = stage_all(large.path());
    assert_nothing_staged(&first.0);
    let t_small = median_clean_stage(small.path());
    let t_large = median_clean_stage(large.path());
    eprintln!("clean ignorecase stage: 2000 files {t_small:?}, 4000 files {t_large:?}");
    assert!(
        t_large.as_secs_f64()
            <= MAX_DOUBLING_SCALING * t_small.as_secs_f64() + TIMING_SLACK.as_secs_f64(),
        "doubling the tree from 2000 to 4000 files took {t_small:?} -> {t_large:?}; \
         staging a clean tree must be linear in the number of files"
    );
    assert!(
        t_large <= first.1 * 2 + TIMING_SLACK,
        "repeated stage on an unchanged tree ({t_large:?}) must not be slower than the first ({:?})",
        first.1
    );
}

#[test]
fn clean_ignorecase_issue_layout_stages_nothing_and_git_agrees() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fixture(root, 3_000, issue_layout);
    let before = ls_files(root);
    let (outcome, _) = stage_all(root);
    assert_nothing_staged(&outcome);
    let (again, _) = stage_all(root);
    assert_nothing_staged(&again);
    assert_eq!(ls_files(root), before);
    let st = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git diff --cached");
    assert!(st.success(), "index must still match HEAD");
    let fsck = Command::new("git")
        .args(["fsck", "--no-progress"])
        .current_dir(root)
        .status()
        .expect("git fsck");
    assert!(fsck.success());
}

#[test]
fn ignorecase_edit_stages_only_the_edited_file() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fixture(root, 700, issue_layout);
    let edited = issue_layout(3);
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join(&edited))
        .expect("open")
        .write_all(b"x\n")
        .expect("append");
    let (outcome, _) = stage_all(root);
    assert_eq!(outcome.modified, vec![edited.clone()]);
    assert!(outcome.added.is_empty() && outcome.removed.is_empty());
    let out = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(root)
        .output()
        .expect("git diff --cached");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), edited);
}

/// On a case-folding volume a case-only rename keeps the old name reachable, so the index
/// entry must be respelled to the on-disk name rather than removed.
#[test]
fn case_only_rename_restages_on_disk_spelling() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    if !is_case_insensitive(root) {
        return;
    }
    fixture(root, 300, issue_layout);
    let old = issue_layout(3);
    let new: PathBuf = root.join("d3/sub3/F3.txt");
    std::fs::rename(root.join(&old), &new).expect("case-only rename");
    let (outcome, _) = stage_all(root);
    assert_eq!(outcome.modified, vec!["d3/sub3/F3.txt".to_owned()]);
    let files = ls_files(root);
    assert!(files.iter().any(|p| p == "d3/sub3/F3.txt"), "{files:?}");
    assert!(!files.iter().any(|p| p == &old), "{files:?}");
}

#[test]
fn clean_48k_ignorecase_add_stays_near_git_add() {
    if std::env::var("GRIT_ADD_PERF").ok().as_deref() != Some("1") || !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fixture(root, 48_000, issue_layout);
    // Files checked out in the same second as the index are racily clean and get re-hashed
    // on every run; rewrite the index past that second so only the scan is measured.
    std::thread::sleep(Duration::from_millis(1100));
    git(root, &["update-index", "-q", "--refresh"]);
    let grit = std::env::var("GRIT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let target = std::env::var("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target"));
            target
                .join("release")
                .join(format!("grit{}", std::env::consts::EXE_SUFFIX))
        });
    assert!(
        grit.is_file(),
        "grit binary not found at {}; run `cargo build --release -p grit-cli` or set GRIT_BIN",
        grit.display()
    );
    let time = |program: &Path| {
        let mut samples: Vec<Duration> = (0..RUNS)
            .map(|_| {
                let t0 = Instant::now();
                let st = Command::new(program)
                    .args(["add", "."])
                    .current_dir(root)
                    .stdout(Stdio::null())
                    .status()
                    .expect("spawn add");
                assert!(st.success());
                t0.elapsed()
            })
            .collect();
        samples.sort();
        samples[RUNS / 2]
    };
    let grit_t = time(&grit);
    let git_t = time(Path::new("git"));
    eprintln!("48k clean add .: grit {grit_t:?}, git {git_t:?}");
    assert!(
        grit_t.as_secs_f64() < 5.0 * git_t.as_secs_f64() + 0.1,
        "grit add . ({grit_t:?}) should stay within a small multiple of git add . ({git_t:?})"
    );
}
