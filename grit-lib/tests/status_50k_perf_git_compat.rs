//! Issue #1231: warm `grit status` on a 50k-file tree should stay within ~3× of `git status`.
//!
//! Fixture matches dogfood #1184/#1185 and `diff_50k_perf_git_compat`: depth 3..8, fanout 0..6,
//! 50_000 files.
//!
//! Build the CLI first, then run:
//! `cargo build --release -p grit-cli && GRIT_STATUS_PERF=1 cargo test -p grit-lib --test status_50k_perf_git_compat --release`

use std::process::Command;
use std::time::{Duration, Instant};

use grit_test_support::git;

const FILE_COUNT: usize = 50_000;
const EDIT_COUNT: usize = 100;
const WARM_RUNS: usize = 5;
const MAX_GIT_RATIO: f64 = 3.0;

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
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_grit") {
        let path = std::path::PathBuf::from(&p);
        if path.is_absolute() && path.is_file() {
            return path;
        }
        let from_workspace = workspace.join(&path);
        if from_workspace.is_file() {
            return from_workspace;
        }
        let from_target = target_dir.join("release/grit");
        if from_target.is_file() {
            return from_target;
        }
    }
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

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let mut script = String::from("import os,random\nrandom.seed(42)\nroot=r'''");
    script.push_str(&root.to_string_lossy());
    script.push_str("'''\n");
    script.push_str(&format!(
        "for i in range({FILE_COUNT}):\n depth=random.randint(3,8)\n p='/'.join([f'd{{random.randint(0,6)}}' for _ in range(depth)])+f'/f{{i}}.txt'\n path=os.path.join(root,p); os.makedirs(os.path.dirname(path),exist_ok=True); open(path,'w').write(f'content {{i}}\\n'*3)\n"
    ));
    std::process::Command::new("python3")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("python fixture");

    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn sample_edits(root: &std::path::Path, seed: u64) {
    let root_s = root.to_string_lossy();
    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import os,random; random.seed({seed}); root=r'{root_s}'; files=[]
for dp,_,fns in os.walk(root):
 if '.git' in dp.split(os.sep): continue
 for fn in fns: files.append(os.path.join(dp,fn))
for p in random.sample(files,{EDIT_COUNT}):
 open(p,'a').write('edit\\n')"
        ))
        .status()
        .expect("edits");
    assert!(out.success());
}

fn touch_mtimes_only(root: &std::path::Path, seed: u64) {
    let root_s = root.to_string_lossy();
    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import os,random; random.seed({seed}); root=r'{root_s}'; files=[]
for dp,_,fns in os.walk(root):
 if '.git' in dp.split(os.sep): continue
 for fn in fns: files.append(os.path.join(dp,fn))
for p in random.sample(files,{EDIT_COUNT}):
 os.utime(p, None)"
        ))
        .status()
        .expect("touch");
    assert!(out.success());
}

fn time_status(
    root: &std::path::Path,
    grit: &std::path::Path,
    runs: usize,
) -> (Duration, Duration) {
    let root_s = root.to_str().expect("utf-8");
    let mut git_samples = Vec::new();
    let mut grit_samples = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let st = Command::new("git")
            .args(["-C", root_s, "status"])
            .status()
            .expect("git status");
        assert!(st.success());
        git_samples.push(t0.elapsed());

        let t0 = Instant::now();
        let st = Command::new(grit)
            .args(["status"])
            .current_dir(root)
            .status()
            .expect("grit status");
        assert!(st.success());
        grit_samples.push(t0.elapsed());
    }
    (median(git_samples), median(grit_samples))
}

fn assert_ratio(label: &str, git_med: Duration, grit_med: Duration) {
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64().max(1e-9);
    eprintln!("{label}: git {git_med:?}, grit {grit_med:?}, ratio {ratio:.2}×");
    assert!(
        ratio < MAX_GIT_RATIO,
        "{label}: grit status should stay within {MAX_GIT_RATIO}× system git (got {ratio:.2}×)"
    );
}

#[test]
fn status_50k_clean_tree_within_three_times_git() {
    if std::env::var("GRIT_STATUS_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);

    for _ in 0..2 {
        let _ = time_status(root, &grit, 1);
    }
    let (git_med, grit_med) = time_status(root, &grit, WARM_RUNS);
    assert_ratio("clean tree", git_med, grit_med);
}

#[test]
fn status_50k_hundred_edits_within_three_times_git() {
    if std::env::var("GRIT_STATUS_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    sample_edits(root, 99);

    for _ in 0..2 {
        let _ = time_status(root, &grit, 1);
    }
    let (git_med, grit_med) = time_status(root, &grit, WARM_RUNS);
    assert_ratio("100 edits", git_med, grit_med);
}

#[test]
fn status_50k_mtime_touch_within_three_times_git() {
    if std::env::var("GRIT_STATUS_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    touch_mtimes_only(root, 101);

    for _ in 0..2 {
        let _ = time_status(root, &grit, 1);
    }
    let (git_med, grit_med) = time_status(root, &grit, WARM_RUNS);
    assert_ratio("mtime-only touch", git_med, grit_med);
}

#[test]
fn status_50k_mtime_touch_warm_second_run_within_three_times_git() {
    if std::env::var("GRIT_STATUS_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    touch_mtimes_only(root, 102);

    let _ = time_status(root, &grit, 2);

    let (git_med, grit_med) = time_status(root, &grit, WARM_RUNS);
    assert_ratio("mtime-only touch warm second pass", git_med, grit_med);
}
