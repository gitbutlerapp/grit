//! Issue #1185: warm `grit diff` on a 50k-file tree should stay within ~3× of `git diff`.
//!
//! Fixture matches dogfood #1184/#1185: depth 3..8, fanout 0..6 per level, 50_000 files.
//!
//! Build the CLI first, then run:
//! `cargo build --release -p grit-cli && GRIT_DIFF_PERF=1 cargo test -p grit-lib --test diff_50k_perf_git_compat --release`

use std::collections::BTreeSet;
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
    let release = target_dir.join("release/grit");
    if release.is_file() {
        return release;
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

fn git_diff_names(root: &std::path::Path) -> BTreeSet<String> {
    git(root, &["diff", "--name-only"])
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_diff_names(root: &std::path::Path, grit: &std::path::Path) -> BTreeSet<String> {
    let out = Command::new(grit)
        .args(["diff", "--json"])
        .current_dir(root)
        .output()
        .expect("grit diff --json");
    assert!(
        out.status.success(),
        "grit diff --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    v.get("files")
        .and_then(|f| f.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("path").and_then(|p| p.as_str()))
        .map(str::to_owned)
        .collect()
}

fn assert_diff_parity(root: &std::path::Path, grit: &std::path::Path, label: &str) {
    let git_names = git_diff_names(root);
    let grit_names = grit_diff_names(root, grit);
    assert_eq!(
        grit_names, git_names,
        "{label}: grit diff --json paths must match git diff --name-only"
    );
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

fn delete_one_tracked_file(root: &std::path::Path, seed: u64) {
    let root_s = root.to_string_lossy();
    std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import os,random; random.seed({seed}); root=r'{root_s}'; files=[]
for dp,_,fns in os.walk(root):
 if '.git' in dp.split(os.sep): continue
 for fn in fns: files.append(os.path.join(dp,fn))
os.remove(random.choice(files))"
        ))
        .status()
        .expect("delete one");
}

fn time_diff(root: &std::path::Path, grit: &std::path::Path, runs: usize) -> (Duration, Duration) {
    let root_s = root.to_str().expect("utf-8");
    let mut git_samples = Vec::new();
    let mut grit_samples = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let st = Command::new("git")
            .args(["-C", root_s, "diff"])
            .status()
            .expect("git diff");
        assert!(st.success());
        git_samples.push(t0.elapsed());

        let t0 = Instant::now();
        let st = Command::new(grit)
            .args(["diff"])
            .current_dir(root)
            .status()
            .expect("grit diff");
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
        "{label}: grit diff should stay within {MAX_GIT_RATIO}× system git (got {ratio:.2}×)"
    );
}

#[test]
fn diff_50k_clean_tree_within_three_times_git() {
    if std::env::var("GRIT_DIFF_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    assert_diff_parity(root, &grit, "clean tree");

    for _ in 0..2 {
        let _ = time_diff(root, &grit, 1);
    }
    assert_diff_parity(root, &grit, "clean tree before timing");
    let (git_med, grit_med) = time_diff(root, &grit, WARM_RUNS);
    assert_diff_parity(root, &grit, "clean tree after timing");
    assert_ratio("clean tree", git_med, grit_med);
}

#[test]
fn diff_50k_hundred_edits_within_three_times_git() {
    if std::env::var("GRIT_DIFF_PERF").ok().as_deref() != Some("1") {
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
    assert_diff_parity(root, &grit, "100 edits");

    for _ in 0..2 {
        let _ = time_diff(root, &grit, 1);
    }
    assert_diff_parity(root, &grit, "100 edits before timing");
    let (git_med, grit_med) = time_diff(root, &grit, WARM_RUNS);
    assert_diff_parity(root, &grit, "100 edits after timing");
    assert_ratio("100 edits", git_med, grit_med);
}

#[test]
fn diff_50k_mtime_touch_within_three_times_git() {
    if std::env::var("GRIT_DIFF_PERF").ok().as_deref() != Some("1") {
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
    assert_diff_parity(root, &grit, "mtime-only touch");

    for _ in 0..2 {
        let _ = time_diff(root, &grit, 1);
    }
    assert_diff_parity(root, &grit, "mtime-only touch before timing");
    let (git_med, grit_med) = time_diff(root, &grit, WARM_RUNS);
    assert_diff_parity(root, &grit, "mtime-only touch after timing");
    assert_ratio("mtime-only touch", git_med, grit_med);
}

#[test]
fn diff_50k_single_deletion_matches_git_before_timing() {
    if std::env::var("GRIT_DIFF_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = grit_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    delete_one_tracked_file(root, 77);
    assert_diff_parity(root, &grit, "single deletion");
}
