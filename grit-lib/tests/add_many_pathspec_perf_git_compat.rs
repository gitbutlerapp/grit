//! Issue #1682: `grit add <many literal paths>` on a large index must not scale with full index size.
//!
//! `GRIT_ADD_PERF=1 cargo test -p grit-lib --test add_many_pathspec_perf_git_compat --release -- --test-threads=1`

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grit_test_support::git;

static PERF_SERIAL: Mutex<()> = Mutex::new(());

const FILES_PER_LEAF: usize = 20;
const LITERAL_PATHS: usize = 98;
const WARM_RUNS: usize = 4;

#[cfg(windows)]
const MAX_GIT_RATIO: f64 = 4.0;
#[cfg(not(windows))]
const MAX_GIT_RATIO: f64 = 5.0;

fn grit_binary() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    #[cfg(windows)]
    let release = target_dir.join("release/grit.exe");
    #[cfg(not(windows))]
    let release = target_dir.join("release/grit");
    assert!(
        release.is_file(),
        "grit binary not found at {} — run `cargo build --release -p grit-cli` or set GRIT_BIN",
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

fn build_deep_fixture(root: &std::path::Path) -> Vec<String> {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let mut n = 0usize;
    let mut all_paths = Vec::new();
    'outer: for a in 0..7 {
        for b in 0..7 {
            for c in 0..7 {
                for d in 0..7 {
                    let p = format!("d{a}/e{b}/f{c}/g{d}");
                    std::fs::create_dir_all(root.join(&p)).expect("mkdir");
                    for i in 0..FILES_PER_LEAF {
                        if n >= 8_000 {
                            break 'outer;
                        }
                        let rel = format!("{p}/file{i}.txt");
                        std::fs::write(root.join(&rel), format!("{n}\n")).expect("write");
                        all_paths.push(rel);
                        n += 1;
                    }
                }
            }
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    all_paths
}

fn pick_changed_paths(all: &[String], count: usize) -> Vec<String> {
    let step = (all.len() / count).max(1);
    all.iter().step_by(step).take(count).cloned().collect()
}

fn time_literal_add(
    root: &std::path::Path,
    grit: &std::path::Path,
    paths: &[String],
    runs: usize,
) -> (Duration, Duration) {
    let root_s = root.to_str().expect("utf-8");
    let null = || Stdio::null();
    for path in paths {
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(path))
            .expect("touch")
            .write_all(b"change\n")
            .expect("append");
    }
    for _ in 0..runs {
        let st = Command::new("git")
            .args(["-C", root_s, "reset", "-q"])
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git reset");
        assert!(st.success());
        let mut cmd = Command::new("git");
        cmd.args(["-C", root_s, "add"]);
        cmd.args(paths);
        let st = cmd
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git add warm");
        assert!(st.success());
    }
    for _ in 0..runs {
        let st = Command::new("git")
            .args(["-C", root_s, "reset", "-q"])
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git reset");
        assert!(st.success());
        let mut cmd = Command::new(grit);
        cmd.arg("add").current_dir(root);
        cmd.args(paths);
        let st = cmd
            .stdout(null())
            .stderr(null())
            .status()
            .expect("grit add warm");
        assert!(st.success());
    }
    let mut git_samples = Vec::new();
    for _ in 0..runs {
        let st = Command::new("git")
            .args(["-C", root_s, "reset", "-q"])
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git reset");
        assert!(st.success());
        let t0 = Instant::now();
        let mut cmd = Command::new("git");
        cmd.args(["-C", root_s, "add"]);
        cmd.args(paths);
        let st = cmd.stdout(null()).stderr(null()).status().expect("git add");
        assert!(st.success());
        git_samples.push(t0.elapsed());
    }
    let mut grit_samples = Vec::new();
    for _ in 0..runs {
        let st = Command::new("git")
            .args(["-C", root_s, "reset", "-q"])
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git reset");
        assert!(st.success());
        let t0 = Instant::now();
        let mut cmd = Command::new(grit);
        cmd.arg("add").current_dir(root);
        cmd.args(paths);
        let st = cmd
            .stdout(null())
            .stderr(null())
            .status()
            .expect("grit add");
        assert!(st.success());
        grit_samples.push(t0.elapsed());
    }
    (median(git_samples), median(grit_samples))
}

use std::io::Write;

#[test]
fn add_many_literal_pathspecs_matches_git_and_scales() {
    if std::env::var("GRIT_ADD_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let _serial = PERF_SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let all = build_deep_fixture(root);
    assert!(
        all.len() >= 5_000,
        "fixture should have thousands of tracked paths, got {}",
        all.len()
    );
    let changed = pick_changed_paths(&all, LITERAL_PATHS);
    assert_eq!(changed.len(), LITERAL_PATHS);

    let (git_med, grit_med) = time_literal_add(root, &grit, &changed, WARM_RUNS);
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64().max(1e-9);
    eprintln!(
        "add {LITERAL_PATHS} literal paths: git {git_med:?}, grit {grit_med:?}, ratio {ratio:.2}×"
    );
    assert!(
        ratio < MAX_GIT_RATIO,
        "many-path add should stay within {MAX_GIT_RATIO}× system git (got {ratio:.2}×)"
    );
}
