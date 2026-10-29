//! Issue #2311: warm `grit merge --abort` on a wide merge should stay near system `git`.
//!
//! Build release grit first (`cargo build --release -p grit-cli`) or set `GRIT_BIN`.
//!
//! ```console
//! GRIT_MERGE_ABORT_PERF=1 cargo test -p grit-lib --test integration merge_abort_wide_near_git --release --features full-tests
//! ```

use std::process::Command;
use std::time::{Duration, Instant};

use grit_test_support::git;

const DIRS: u32 = 40;
const FILES_PER_DIR: u32 = 400;
const MAX_GIT_RATIO: f64 = 2.0;
const WARM_RUNS: usize = 3;

fn grit_binary() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let release = workspace.join("target/release/grit");
    assert!(
        release.is_file(),
        "release grit missing at {} — build with `cargo build --release -p grit-cli`",
        release.display()
    );
    release
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let mid = samples.len() / 2;
    if samples.len() % 2 == 0 {
        (samples[mid - 1] + samples[mid]) / 2
    } else {
        samples[mid]
    }
}

fn run_cmd(root: &std::path::Path, program: &std::path::Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", program.display()));
    assert!(
        out.status.success(),
        "{} {:?} failed: {}",
        program.display(),
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn timed(root: &std::path::Path, program: &std::path::Path, args: &[&str]) -> Duration {
    let start = Instant::now();
    run_cmd(root, program, args);
    start.elapsed()
}

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "t@example.com"]);
    for d in 0..DIRS {
        std::fs::create_dir_all(root.join(format!("d{d}"))).expect("mkdir");
        for i in 0..FILES_PER_DIR {
            std::fs::write(
                root.join(format!("d{d}/f{i}.txt")),
                format!("base {d} {i}\n"),
            )
            .expect("write");
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    let base = git_out(root, &["rev-parse", "HEAD"]);

    git(root, &["checkout", "-q", "-B", "ba", &base]);
    for d in 0..DIRS / 2 {
        for i in 0..FILES_PER_DIR {
            std::fs::write(root.join(format!("d{d}/f{i}.txt")), format!("ba {d} {i}\n"))
                .expect("write");
        }
    }
    git(root, &["commit", "-qam", "ba"]);

    git(root, &["checkout", "-q", "-B", "bb", &base]);
    for d in DIRS / 2..DIRS {
        for i in 0..FILES_PER_DIR {
            std::fs::write(root.join(format!("d{d}/f{i}.txt")), format!("bb {d} {i}\n"))
                .expect("write");
        }
    }
    for d in 0..6 {
        for i in 0..FILES_PER_DIR {
            std::fs::write(
                root.join(format!("d{d}/f{i}.txt")),
                format!("bb overlap {d} {i}\n"),
            )
            .expect("write");
        }
    }
    git(root, &["commit", "-qam", "bb"]);
}

fn git_out(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn warm_abort_median(
    root: &std::path::Path,
    program: &std::path::Path,
    merge_args: &[&str],
    abort_args: &[&str],
) -> Duration {
    for _ in 0..WARM_RUNS {
        git(root, &["checkout", "-q", "ba"]);
        let _ = Command::new(program)
            .args(merge_args)
            .current_dir(root)
            .output();
        let _ = timed(root, program, abort_args);
    }
    let mut samples = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        git(root, &["checkout", "-q", "ba"]);
        let _ = Command::new(program)
            .args(merge_args)
            .current_dir(root)
            .output();
        samples.push(timed(root, program, abort_args));
    }
    median(samples)
}

#[test]
fn merge_abort_wide_near_git() {
    if std::env::var("GRIT_MERGE_ABORT_PERF").ok().as_deref() != Some("1") {
        eprintln!("skip merge_abort_wide_perf (set GRIT_MERGE_ABORT_PERF=1 to run)");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    build_fixture(root);
    let grit = grit_binary();

    let git_abort = warm_abort_median(
        root,
        std::path::Path::new("git"),
        &["merge", "--no-commit", "--no-ff", "bb"],
        &["merge", "--abort"],
    );
    let grit_abort = warm_abort_median(root, &grit, &["merge", "bb"], &["merge", "--abort"]);

    git(root, &["checkout", "-q", "ba"]);
    let _ = Command::new(&grit)
        .args(["merge", "bb"])
        .current_dir(root)
        .output();
    let status_start = Instant::now();
    run_cmd(
        root,
        std::path::Path::new("git"),
        &["status", "--porcelain"],
    );
    let status_after_abort = status_start.elapsed();

    let ratio = grit_abort.as_secs_f64() / git_abort.as_secs_f64();
    eprintln!(
        "wide merge --abort: git={git_abort:?} grit={grit_abort:?} ratio={ratio:.2} \
         git status after abort={status_after_abort:?}"
    );
    assert!(
        ratio <= MAX_GIT_RATIO,
        "grit merge --abort should stay within {MAX_GIT_RATIO}× git (got {ratio:.2}×)"
    );
}
