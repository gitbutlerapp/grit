//! Issue #1682: `grit add <many literal paths>` must not scale linearly with pathspec count on large indexes.
//!
//! ```bash
//! cargo build --release -p grit-cli
//! GRIT_ADD_PERF=1 cargo test -p grit-lib --test add_many_pathspec_perf_git_compat --release -- --test-threads=1
//! ```

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grit_test_support::git;

static PERF_SERIAL: Mutex<()> = Mutex::new(());

const FILES_PER_LEAF: usize = 20;
const LITERAL_PATHS: usize = 98;
const WARM_RUNS: usize = 5;
/// Matches the issue #1682 reproduction script (`git ls-files | wc -l` → 48,020).
const TARGET_TRACKED: usize = 48_020;

/// Grit time for 98 paths must not grow this many times vs 1 path on the same fixture.
const MAX_GRIT_PATH_COUNT_SCALING: f64 = 4.0;

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

/// Build the issue #1682 tree with `config` applied before the base commit.
fn build_issue_fixture(root: &std::path::Path, config: &[(&str, &str)]) -> Vec<String> {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for (key, value) in config {
        git(root, &["config", key, value]);
    }
    let mut n = 0usize;
    let mut all_paths = Vec::new();
    'outer: for a in 0..7 {
        for b in 0..7 {
            for c in 0..7 {
                for d in 0..7 {
                    let p = format!("d{a}/e{b}/f{c}/g{d}");
                    std::fs::create_dir_all(root.join(&p)).expect("mkdir");
                    for i in 0..FILES_PER_LEAF {
                        if n >= TARGET_TRACKED {
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

fn dirty_paths(root: &std::path::Path, paths: &[String]) {
    for path in paths {
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(path))
            .expect("touch")
            .write_all(b"change\n")
            .expect("append");
    }
}

/// Median wall time of `program add <args>` after `git reset -q`, following `runs` warm-ups.
fn median_add(
    root: &std::path::Path,
    program: &std::path::Path,
    args: &[String],
    runs: usize,
) -> Duration {
    let root_s = root.to_str().expect("utf-8");
    let mut samples = Vec::new();
    for round in 0..runs * 2 {
        let st = Command::new("git")
            .args(["-C", root_s, "reset", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git reset");
        assert!(st.success());
        let t0 = Instant::now();
        let out = Command::new(program)
            .arg("add")
            .args(args)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("run add");
        let elapsed = t0.elapsed();
        assert!(
            out.status.success(),
            "{} add failed: {}",
            program.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        if round >= runs {
            samples.push(elapsed);
        }
    }
    median(samples)
}

fn median_grit_add(
    root: &std::path::Path,
    grit: &std::path::Path,
    paths: &[String],
    runs: usize,
) -> Duration {
    median_add(root, grit, paths, runs)
}

fn median_git_add(root: &std::path::Path, paths: &[String], runs: usize) -> Duration {
    median_add(root, std::path::Path::new("git"), paths, runs)
}

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
    let all = build_issue_fixture(root, &[]);
    assert!(
        all.len() >= TARGET_TRACKED - 500,
        "fixture should match issue #1682 scale (~{TARGET_TRACKED} paths), got {}",
        all.len()
    );
    let changed = pick_changed_paths(&all, LITERAL_PATHS);
    assert_eq!(changed.len(), LITERAL_PATHS);

    // Overhead check (#1682): one dirty file, many literal pathspecs — must not rescan the index per spec.
    dirty_paths(root, std::slice::from_ref(&changed[0]));
    let grit_one_spec = median_grit_add(root, &grit, &changed[..1], WARM_RUNS);
    let grit_many_specs = median_grit_add(root, &grit, &changed, WARM_RUNS);
    let overhead_scaling = grit_many_specs.as_secs_f64() / grit_one_spec.as_secs_f64().max(1e-9);
    eprintln!(
        "48k overhead (1 dirty file, 1 vs {LITERAL_PATHS} pathspecs): \
         grit {grit_one_spec:?} vs {grit_many_specs:?}, scaling {overhead_scaling:.2}×"
    );
    assert!(
        overhead_scaling < MAX_GRIT_PATH_COUNT_SCALING,
        "pathspec validation must not scan the full index per spec \
         (1 vs {LITERAL_PATHS} specs: {overhead_scaling:.2}×, limit {MAX_GRIT_PATH_COUNT_SCALING}×)"
    );

    // Throughput check: stage every modified file listed in 98 pathspecs.
    for path in &changed {
        std::fs::write(root.join(path), b"changed2\n").expect("re-dirty");
    }
    let grit_many = median_grit_add(root, &grit, &changed, WARM_RUNS);
    let git_many = median_git_add(root, &changed, WARM_RUNS);
    let git_ratio = grit_many.as_secs_f64() / git_many.as_secs_f64().max(1e-9);
    eprintln!(
        "48k fixture ({LITERAL_PATHS} dirty files): grit {grit_many:?}, git {git_many:?}, \
         grit/git {git_ratio:.2}×"
    );
    assert!(
        git_ratio < MAX_GIT_RATIO,
        "many-path add should stay within {MAX_GIT_RATIO}× system git (got {git_ratio:.2}×)"
    );
}

/// The macOS defaults (`core.ignorecase`, `core.precomposeunicode`) must not make staging
/// re-read every directory per index entry: `add .` and `add -u` took ~20× system git.
#[test]
fn add_with_macos_path_config_matches_git_and_scales() {
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
    let all = build_issue_fixture(
        root,
        &[
            ("core.ignorecase", "true"),
            ("core.precomposeunicode", "true"),
        ],
    );
    let changed = pick_changed_paths(&all, LITERAL_PATHS);
    dirty_paths(root, &changed);

    let grit_one = median_grit_add(root, &grit, &changed[..1], WARM_RUNS);
    let grit_many = median_grit_add(root, &grit, &changed, WARM_RUNS);
    let git_many = median_git_add(root, &changed, WARM_RUNS);
    let scaling = grit_many.as_secs_f64() / grit_one.as_secs_f64().max(1e-9);
    let many_ratio = grit_many.as_secs_f64() / git_many.as_secs_f64().max(1e-9);
    eprintln!(
        "ignorecase 48k: grit add 1 path {grit_one:?}, {LITERAL_PATHS} paths {grit_many:?} \
         ({scaling:.2}×), git {git_many:?} (grit/git {many_ratio:.2}×)"
    );
    assert!(
        scaling < MAX_GRIT_PATH_COUNT_SCALING,
        "1 vs {LITERAL_PATHS} paths: {scaling:.2}×, limit {MAX_GRIT_PATH_COUNT_SCALING}×"
    );
    assert!(
        many_ratio < MAX_GIT_RATIO,
        "{LITERAL_PATHS}-path add: {many_ratio:.2}× system git, limit {MAX_GIT_RATIO}×"
    );

    for (grit_args, git_args) in [(["."], ["-A"]), (["-u"], ["-u"])] {
        let grit_args: Vec<String> = grit_args.iter().map(|a| (*a).to_owned()).collect();
        let git_args: Vec<String> = git_args.iter().map(|a| (*a).to_owned()).collect();
        let grit_t = median_grit_add(root, &grit, &grit_args, WARM_RUNS);
        let git_t = median_git_add(root, &git_args, WARM_RUNS);
        let ratio = grit_t.as_secs_f64() / git_t.as_secs_f64().max(1e-9);
        eprintln!(
            "ignorecase 48k: grit add {grit_args:?} {grit_t:?} vs git add {git_args:?} {git_t:?} \
             ({ratio:.2}×)"
        );
        assert!(
            ratio < MAX_GIT_RATIO,
            "grit add {grit_args:?}: {ratio:.2}× system git, limit {MAX_GIT_RATIO}×"
        );
    }
}
