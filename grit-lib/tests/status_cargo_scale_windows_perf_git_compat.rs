//! Issue #2566: warm `grit status` on ~3k tracked files should stay within ~2× of `git status` on Windows.
//!
//! Mirrors dogfood on rust-lang/cargo scale (~3081 tracked paths). Run on win-x64 with:
//! `cargo build --release -p grit-cli && GRIT_STATUS_PERF=1 cargo test -p grit-lib --test status_cargo_scale_windows_perf_git_compat --release -- --test-threads=1`

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grit_test_support::git;

static PERF_SERIAL: Mutex<()> = Mutex::new(());

const FILE_COUNT: usize = 3_081;
const WARM_RUNS: usize = 8;
const MAX_GIT_RATIO: f64 = 2.0;

fn grit_binary() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let release = workspace.join("target/release/grit.exe");
    assert!(release.is_file(), "build grit-cli release first");
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

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "true"]);
    let script = format!(
        "import os\nroot=r'''{r}'''\nfor i in range({FILE_COUNT}):\n d=i%40; p=f'crates/pkg{{d}}/src/lib_{{i}}.rs'\n path=os.path.join(root,p); os.makedirs(os.path.dirname(path),exist_ok=True); open(path,'w').write(f'mod m{{i}};\\n')\n",
        r = root.to_string_lossy()
    );
    std::process::Command::new("python3")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("python fixture");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn time_cmd(bin: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> Duration {
    let start = Instant::now();
    let out = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("status");
    assert!(out.success(), "{bin:?} {args:?}");
    start.elapsed()
}

#[test]
fn warm_clean_status_within_twice_git_on_cargo_scale() {
    if std::env::var("GRIT_STATUS_PERF").is_err() {
        return;
    }
    let _lock = PERF_SERIAL.lock().expect("perf lock");
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let grit = grit_binary();
    for _ in 0..2 {
        let _ = time_cmd(&grit, root, &["status"]);
    }
    let mut grit_samples = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        grit_samples.push(time_cmd(&grit, root, &["status"]));
    }
    let git_bin = std::path::Path::new("git");
    for _ in 0..2 {
        let _ = time_cmd(git_bin, root, &["status"]);
    }
    let mut git_samples = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        git_samples.push(time_cmd(git_bin, root, &["status"]));
    }
    let grit_med = median(grit_samples);
    let git_med = median(git_samples);
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64();
    assert!(
        ratio <= MAX_GIT_RATIO,
        "grit status {grit_med:?} vs git {git_med:?} (ratio {ratio:.2} > {MAX_GIT_RATIO})"
    );
}
