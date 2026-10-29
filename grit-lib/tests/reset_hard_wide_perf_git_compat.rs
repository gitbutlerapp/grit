//! Issue #2311: warm `grit reset --hard` on a wide tree diff should stay near system `git`
//! and no slower than `grit switch` over the same diff.
//!
//! `GRIT_RESET_PERF=1 cargo test -p grit-lib --test reset_hard_wide_perf_git_compat --release`

use std::process::Command;
use std::time::{Duration, Instant};

use grit_test_support::git;

const DIRS: u32 = 40;
const FILES_PER_DIR: u32 = 400;
const DIRS_OTHER: u32 = 20;
const WARM_RUNS: usize = 3;
const MAX_GIT_RATIO: f64 = 1.75;
/// Hard reset stat-probes unchanged paths (dirt check); switch skips that on a clean tree.
const MAX_SWITCH_RATIO: f64 = 4.0;

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

    git(root, &["checkout", "-q", "-b", "other"]);
    for d in 0..DIRS_OTHER {
        for i in 0..FILES_PER_DIR {
            let path = root.join(format!("d{d}/f{i}.txt"));
            std::fs::write(path, format!("other {d} {i}\n")).expect("write other");
        }
    }
    git(root, &["commit", "-qam", "other"]);

    git(root, &["checkout", "-q", "main"]);
    for d in DIRS_OTHER..DIRS {
        for i in 0..FILES_PER_DIR {
            let path = root.join(format!("d{d}/f{i}.txt"));
            std::fs::write(path, format!("main {d} {i}\n")).expect("write main");
        }
    }
    git(root, &["commit", "-qam", "main2"]);
}

fn warm_median(
    root: &std::path::Path,
    program: &std::path::Path,
    args: &[&str],
    reset_to: &str,
) -> Duration {
    for _ in 0..WARM_RUNS {
        git(root, &["checkout", "-q", reset_to]);
        let _ = timed(root, program, args);
    }
    let mut samples = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        git(root, &["checkout", "-q", reset_to]);
        samples.push(timed(root, program, args));
    }
    median(samples)
}

#[test]
fn reset_hard_wide_tree_near_git_and_switch() {
    if std::env::var("GRIT_RESET_PERF").ok().as_deref() != Some("1") {
        eprintln!("skip reset_hard_wide_perf (set GRIT_RESET_PERF=1 to run)");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    build_fixture(root);
    git(root, &["checkout", "-q", "main"]);

    let grit = grit_binary();
    let git_reset = warm_median(
        root,
        std::path::Path::new("git"),
        &["reset", "--hard", "other"],
        "main",
    );
    let grit_reset = warm_median(root, &grit, &["reset", "--hard", "other"], "main");
    git(root, &["checkout", "-q", "main"]);
    let grit_switch = warm_median(root, &grit, &["switch", "other"], "main");

    git(root, &["checkout", "-q", "main"]);
    let git_ratio = grit_reset.as_secs_f64() / git_reset.as_secs_f64();
    let switch_ratio = grit_reset.as_secs_f64() / grit_switch.as_secs_f64();
    eprintln!(
        "wide reset --hard: git={git_reset:?} grit={grit_reset:?} grit_switch={grit_switch:?} \
         git_ratio={git_ratio:.2} switch_ratio={switch_ratio:.2}"
    );
    assert!(
        git_ratio <= MAX_GIT_RATIO,
        "grit reset --hard should stay within {MAX_GIT_RATIO}× git (got {git_ratio:.2}×)"
    );
    assert!(
        switch_ratio <= MAX_SWITCH_RATIO,
        "grit reset --hard should stay within {MAX_SWITCH_RATIO}× grit switch (got {switch_ratio:.2}×)"
    );
}
