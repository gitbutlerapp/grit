//! Issue #1518: warm no-op `grit add` on a 50k-file dense-leaf tree stays within ~3× of `git add -A`.
//!
//! Layout matches dogfood #1041 / #1518: many files per leaf directory (grit-utils `DenseLeafDogfoodSpec::production`).
//!
//! `cargo build --release -p grit-cli && GRIT_ADD_PERF=1 cargo test -p grit-lib --test add_50k_perf_git_compat --release -- --test-threads=1`

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grit_test_support::git;

static PERF_SERIAL: Mutex<()> = Mutex::new(());

const LEAF_DIRS: usize = 1_000;
const FILES_PER_LEAF: usize = 50;
const TRACKED: usize = LEAF_DIRS * FILES_PER_LEAF;
const WARM_RUNS: usize = 6;
const GRIT_REWARM_AFTER_GIT: usize = 2;

#[cfg(windows)]
const MAX_GIT_RATIO: f64 = 3.0;
#[cfg(not(windows))]
const MAX_GIT_RATIO: f64 = 4.0;

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
    if release.is_file() {
        return release;
    }
    #[cfg(windows)]
    {
        let unix_name = target_dir.join("release/grit");
        if unix_name.is_file() {
            return unix_name;
        }
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_grit") {
        let path = std::path::PathBuf::from(&p);
        if path.is_absolute() && path.is_file() {
            return path;
        }
        let from_workspace = workspace.join(&path);
        if from_workspace.is_file() {
            return from_workspace;
        }
    }
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

fn dense_leaf_prefix(leaf: usize) -> String {
    let depth = 5 + (leaf % 7);
    let mut path = String::new();
    for level in 0..depth {
        path.push_str(&format!(
            "p{}/q{}/r{}/s{}/n{}/",
            leaf % 10,
            level % 10,
            level % 8,
            level % 6,
            level % 5
        ));
    }
    path
}

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "true"]);
    std::fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");

    for leaf in 0..LEAF_DIRS {
        let prefix = dense_leaf_prefix(leaf);
        let leaf_dir = format!("{prefix}leaf{leaf:04}");
        let abs_leaf = root.join(&leaf_dir);
        std::fs::create_dir_all(&abs_leaf).expect("mkdir");
        for f in 0..FILES_PER_LEAF {
            let path = abs_leaf.join(format!("f{f}-file.txt"));
            std::fs::write(path, format!("leaf={leaf} f={f}\n")).expect("write");
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn time_noop_add(
    root: &std::path::Path,
    grit: &std::path::Path,
    runs: usize,
) -> (Duration, Duration) {
    let root_s = root.to_str().expect("utf-8");
    let null = || Stdio::null();
    for _ in 0..runs {
        let st = Command::new("git")
            .args(["-C", root_s, "add", "-A"])
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git add warm");
        assert!(st.success());
    }
    for _ in 0..runs {
        let st = Command::new(grit)
            .args(["add"])
            .current_dir(root)
            .stdout(null())
            .stderr(null())
            .status()
            .expect("grit add warm");
        assert!(st.success());
    }
    let mut git_samples = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let st = Command::new("git")
            .args(["-C", root_s, "add", "-A"])
            .stdout(null())
            .stderr(null())
            .status()
            .expect("git add");
        assert!(st.success());
        git_samples.push(t0.elapsed());
    }
    for _ in 0..(runs * GRIT_REWARM_AFTER_GIT) {
        let st = Command::new(grit)
            .args(["add"])
            .current_dir(root)
            .stdout(null())
            .stderr(null())
            .status()
            .expect("grit add re-warm");
        assert!(st.success());
    }
    let mut grit_samples = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let st = Command::new(grit)
            .args(["add"])
            .current_dir(root)
            .stdout(null())
            .stderr(null())
            .status()
            .expect("grit add");
        assert!(st.success());
        grit_samples.push(t0.elapsed());
    }
    (median(git_samples), median(grit_samples))
}

#[test]
fn add_50k_dense_leaf_noop_within_git_ratio() {
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
    build_fixture(root);
    let tracked = git(root, &["ls-files"]).lines().count();
    assert!(
        tracked >= TRACKED,
        "fixture should have at least {TRACKED} tracked paths, got {tracked}"
    );

    let (git_med, grit_med) = time_noop_add(root, &grit, WARM_RUNS);
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64().max(1e-9);
    eprintln!("noop add clean: git {git_med:?}, grit {grit_med:?}, ratio {ratio:.2}×");
    assert!(
        ratio < MAX_GIT_RATIO,
        "noop add should stay within {MAX_GIT_RATIO}× system git (got {ratio:.2}×)"
    );
}
