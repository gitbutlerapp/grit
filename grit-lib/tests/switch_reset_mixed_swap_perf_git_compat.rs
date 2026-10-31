//! Issue #2380: mixed delete/add checkout on a wide tree should stay near system `git`.
//!
//! `GRIT_SWAP_PERF=1 cargo test -p grit-lib --test switch_reset_mixed_swap_perf_git_compat --release`

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use grit_test_support::git;

const N: u32 = 100_000;
const K: u32 = 10_000;
const WARM_RUNS: usize = 3;
const MAX_GIT_SWITCH_RATIO: f64 = 2.0;
const MAX_GIT_RESET_RATIO: f64 = 2.5;
const MAX_RESET_SWITCH_RATIO: f64 = 2.0;

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

fn path(i: u32) -> String {
    format!(
        "d{}/s{}/m{}/file_{}.txt",
        i % 50,
        (i / 50) % 60,
        (i / 3000) % 10,
        i
    )
}

fn build_fixture(src: &std::path::Path) {
    git(src, &["init", "-q"]);
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet", "--force"])
        .current_dir(src)
        .stdin(Stdio::piped())
        .spawn()
        .expect("fast-import");
    let stdin = child.stdin.as_mut().expect("stdin");
    writeln!(stdin, "commit refs/heads/base").expect("write");
    writeln!(stdin, "mark :1").expect("write");
    writeln!(stdin, "committer T <t@x> 1700000000 +0000").expect("write");
    writeln!(stdin, "data 4").expect("write");
    writeln!(stdin, "base").expect("write");
    for i in 0..N {
        let content = format!("file {i}\n{}", "x".repeat((i % 200) as usize));
        writeln!(
            stdin,
            "M 100644 inline {}\ndata {}\n{}",
            path(i),
            content.len(),
            content
        )
        .expect("write blob");
    }
    writeln!(stdin, "commit refs/heads/swap").expect("write");
    writeln!(stdin, "committer T <t@x> 1700001000 +0000").expect("write");
    writeln!(stdin, "data 4").expect("write");
    writeln!(stdin, "swap").expect("write");
    writeln!(stdin, "from :1").expect("write");
    for i in (1..K * 6).step_by(6) {
        writeln!(stdin, "D {}", path(i)).expect("write delete");
    }
    for i in N..N + K {
        let content = format!("brand new {i}\n");
        writeln!(
            stdin,
            "M 100644 inline {}\ndata {}\n{}",
            path(i),
            content.len(),
            content
        )
        .expect("write add");
    }
    drop(child.stdin.take());
    let status = child.wait().expect("wait fast-import");
    assert!(status.success(), "fast-import failed");
    git(src, &["symbolic-ref", "HEAD", "refs/heads/base"]);
}

fn warm_median(
    root: &std::path::Path,
    program: &std::path::Path,
    args: &[&str],
    reset_branch: &str,
) -> Duration {
    for _ in 0..WARM_RUNS {
        git(root, &["checkout", "-q", reset_branch]);
        let _ = timed(root, program, args);
    }
    let mut samples = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        git(root, &["checkout", "-q", reset_branch]);
        samples.push(timed(root, program, args));
    }
    median(samples)
}

#[test]
fn switch_reset_mixed_swap_near_git() {
    if std::env::var("GRIT_SWAP_PERF").ok().as_deref() != Some("1") {
        eprintln!("skip switch_reset_mixed_swap perf (set GRIT_SWAP_PERF=1 to run)");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    build_fixture(&src);

    let work = tmp.path().join("work");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            src.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
        ],
    );
    git(&work, &["branch", "-q", "swap", "origin/swap"]);

    let grit = grit_binary();
    git(&work, &["checkout", "-q", "base"]);

    let git_switch = warm_median(
        &work,
        std::path::Path::new("git"),
        &["switch", "swap"],
        "base",
    );
    let grit_switch = warm_median(&work, &grit, &["switch", "swap"], "base");

    git(&work, &["checkout", "-q", "base"]);
    let git_reset = warm_median(
        &work,
        std::path::Path::new("git"),
        &["reset", "--hard", "swap"],
        "base",
    );
    let grit_reset = warm_median(&work, &grit, &["reset", "--hard", "swap"], "base");

    let switch_ratio = grit_switch.as_secs_f64() / git_switch.as_secs_f64();
    let reset_ratio = grit_reset.as_secs_f64() / git_reset.as_secs_f64();
    let reset_switch_ratio = grit_reset.as_secs_f64() / grit_switch.as_secs_f64();
    eprintln!(
        "mixed swap: git_switch={git_switch:?} grit_switch={grit_switch:?} \
         git_reset={git_reset:?} grit_reset={grit_reset:?} \
         switch_ratio={switch_ratio:.2} reset_ratio={reset_ratio:.2} \
         reset/switch={reset_switch_ratio:.2}"
    );
    assert!(
        switch_ratio <= MAX_GIT_SWITCH_RATIO,
        "grit switch should stay within {MAX_GIT_SWITCH_RATIO}× git (got {switch_ratio:.2}×)"
    );
    assert!(
        reset_ratio <= MAX_GIT_RESET_RATIO,
        "grit reset --hard should stay within {MAX_GIT_RESET_RATIO}× git (got {reset_ratio:.2}×)"
    );
    assert!(
        reset_switch_ratio <= MAX_RESET_SWITCH_RATIO,
        "grit reset --hard should stay within {MAX_RESET_SWITCH_RATIO}× grit switch \
         (got {reset_switch_ratio:.2}×)"
    );
}
