//! Hard reset at HEAD must not retain multiple copies of a wide index.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_test_support::configure_git_env;

const PROBE_TOOL_ENV: &str = "GRIT_RESET_MEMORY_PROBE_TOOL";
const CHILD_REPO_ENV: &str = "GRIT_RESET_MEMORY_CHILD_REPO";
const GRIT_BIN_ENV: &str = "GRIT_RESET_MEMORY_GRIT_BIN";
const STABILITY_RUNS: usize = 20;

fn run(mut cmd: Command) {
    let status = cmd.status().expect("spawn");
    assert!(status.success(), "{cmd:?} failed with {status}");
}

fn git_in(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    cmd.current_dir(root);
    cmd
}

fn wide_repo(root: &Path, files: usize) {
    for d in 0..files / 100 {
        let dir = root.join(format!(
            "top{}/mid{}/pkg{}/leaf{}",
            d % 30,
            (d / 30) % 10,
            d / 300,
            d
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..100 {
            let path = dir.join(format!("f{f}.txt"));
            let mut out = std::fs::File::create(&path).expect("create");
            for l in 0..3 + f % 20 {
                writeln!(out, "line {l} of {d}/{f}").expect("write");
            }
        }
    }
    git_in(root)
        .args(["init", "-q", "-b", "master"])
        .status()
        .expect("init");
    git_in(root)
        .args(["config", "user.name", "T"])
        .status()
        .expect("name");
    git_in(root)
        .args(["config", "user.email", "t@e.x"])
        .status()
        .expect("email");
    git_in(root).args(["add", "-A"]).status().expect("add");
    git_in(root)
        .args(["commit", "-qm", "base"])
        .status()
        .expect("commit");
    git_in(root)
        .args(["update-index", "-q", "--refresh"])
        .status()
        .expect("refresh");
}

fn grit_bin() -> PathBuf {
    let grit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/grit");
    assert!(
        grit.is_file(),
        "build release grit first: cargo build --release -p grit-cli"
    );
    grit
}

/// Peak RSS in KiB of `reset --hard` at HEAD in `root` via `tool` (`git` or `grit`).
///
/// Runs the command in a child of a fresh test process so `getrusage(RUSAGE_CHILDREN)` is
/// not polluted by the parent's RSS.
fn reset_peak_rss_kib(tool: &str, root: &Path, grit: &Path) -> u64 {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "reset_hard_memory::rss_probe",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .args(["--test-threads=1", "-q"])
        .env(PROBE_TOOL_ENV, tool)
        .env(CHILD_REPO_ENV, root)
        .env(GRIT_BIN_ENV, grit)
        .output()
        .expect("spawn rss probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{tool} probe failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("peak_kib="))
        .and_then(|kib| kib.trim().parse().ok())
        .unwrap_or_else(|| panic!("{tool} probe printed no peak: {stdout}"))
}

#[test]
fn hard_reset_at_head_peak_rss_scales_like_git_on_wide_tree() {
    let grit = grit_bin();
    let small_git_root = tempfile::tempdir().expect("small git");
    wide_repo(small_git_root.path(), 2_000);
    let wide_git_root = tempfile::tempdir().expect("wide git");
    wide_repo(wide_git_root.path(), 30_000);
    let small_grit_root = tempfile::tempdir().expect("small grit");
    wide_repo(small_grit_root.path(), 2_000);
    let wide_grit_root = tempfile::tempdir().expect("wide grit");
    wide_repo(wide_grit_root.path(), 30_000);

    let small_git = reset_peak_rss_kib("git", small_git_root.path(), &grit);
    let wide_git = reset_peak_rss_kib("git", wide_git_root.path(), &grit);
    let small_grit = reset_peak_rss_kib("grit", small_grit_root.path(), &grit);
    let wide_grit_baseline = reset_peak_rss_kib("grit", wide_grit_root.path(), &grit);
    let git_growth = wide_git.saturating_sub(small_git);
    let grit_growth = wide_grit_baseline.saturating_sub(small_grit);

    let mut wide_grit_samples = Vec::with_capacity(STABILITY_RUNS);
    for run in 0..STABILITY_RUNS {
        let root = tempfile::tempdir().expect("wide grit stability");
        wide_repo(root.path(), 30_000);
        let peak = reset_peak_rss_kib("grit", root.path(), &grit);
        wide_grit_samples.push(peak);
        let limit = wide_git * 3 / 2 + 8 * 1024;
        assert!(
            peak <= limit,
            "run {run}: grit {peak} KiB vs git {wide_git} KiB on 30k files (limit {limit})"
        );
    }
    let wide_grit = *wide_grit_samples.iter().max().expect("samples");
    eprintln!(
        "git {small_git} -> {wide_git} KiB (+{git_growth}); grit {small_grit} -> {wide_grit} KiB (+{grit_growth}); grit 30k peaks (max of {STABILITY_RUNS}): {wide_grit_samples:?}"
    );
    assert!(
        grit_growth <= git_growth * 3 / 2 + 8 * 1024,
        "grit growth {grit_growth} KiB vs git {git_growth} KiB"
    );
    assert!(
        wide_grit <= wide_git * 3 / 2 + 8 * 1024,
        "grit {wide_grit} KiB vs git {wide_git} KiB on 30k files"
    );
}

/// Measuring side: run one reset in a child and print peak RSS via `getrusage(RUSAGE_CHILDREN)`.
#[test]
#[ignore = "run by hard_reset_at_head_peak_rss_scales_like_git_on_wide_tree in a child process"]
fn rss_probe() {
    let (Some(tool), Some(root), Some(grit_bin)) = (
        std::env::var_os(PROBE_TOOL_ENV),
        std::env::var_os(CHILD_REPO_ENV),
        std::env::var_os(GRIT_BIN_ENV),
    ) else {
        return;
    };
    let root = Path::new(&root);
    let mut cmd = if tool == "git" {
        let mut cmd = git_in(root);
        cmd.args(["reset", "--hard", "-q"]);
        cmd
    } else {
        let mut cmd = Command::new(grit_bin);
        cmd.current_dir(root).args(["reset", "--hard"]);
        cmd
    };
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    run(cmd);
    // SAFETY: plain FFI call filling a zeroed, correctly sized `rusage`.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) },
        0
    );
    println!("peak_kib={}", usage.ru_maxrss);
}
