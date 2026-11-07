//! Peak-memory gate for staging a large new tree into an empty index (issue #2169).
//!
//! Writes a wide tree of small files, then measures the peak resident set size of
//! `git add -A` and of grit's [`stage`] on it, each in a fresh repository and its own child
//! process (the grit side re-runs this test binary on the ignored `stage_child` test). The
//! gate fails when grit needs more than [`MAX_RATIO`] times git's peak. Afterwards the
//! index grit wrote must list the same entries as git's and pass `git fsck`.
//!
//! Staging used to keep a read request (attributes, metadata, absolute path) and a hashed
//! result for every file alive at once, about 1.4 KiB per path, several times git's peak.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::StageOptions;
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

/// Allowed grit / git peak-RSS ratio on the fixture tree (about 1.9x measured, 5.2x before
/// staging went chunk by chunk).
const MAX_RATIO: f64 = 2.5;
/// Files in the fixture, spread like the issue's tree (100 per leaf directory).
const FILES: usize = 60_000;
const CHILD_REPO_ENV: &str = "GRIT_ADD_MEMORY_CHILD_REPO";
const PROBE_TOOL_ENV: &str = "GRIT_ADD_MEMORY_PROBE_TOOL";

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).envs(GIT_ENV.iter().copied());
    cmd
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 output")
}

fn write_tree(root: &Path) {
    for leaf in 0..FILES / 100 {
        let dir = root.join(format!(
            "top{}/mid{}/pkg{}/leaf{leaf}",
            leaf % 30,
            (leaf / 30) % 10,
            leaf / 300
        ));
        std::fs::create_dir_all(&dir).expect("leaf dir");
        for file in 0..100 {
            std::fs::write(
                dir.join(format!("file{file}.txt")),
                format!(
                    "line one {leaf} {file}\nline two\nline three {}\n",
                    leaf * file
                ),
            )
            .expect("write file");
        }
    }
}

/// Start a fresh repository at `root`, dropping any earlier `.git`.
fn reinit(root: &Path) {
    let git_dir = root.join(".git");
    if git_dir.exists() {
        std::fs::remove_dir_all(&git_dir).expect("remove .git");
    }
    run(git(root).args(["init", "-q"]));
}

/// Peak RSS in KiB of staging the whole tree at `root` with `tool` (`git` or `grit`).
///
/// A child's `ru_maxrss` starts at the RSS of the process it was forked from, so the
/// command runs under a fresh copy of this test binary ([`rss_probe`]) that reports
/// `getrusage(RUSAGE_CHILDREN)` for it.
fn stage_peak_rss_kib(tool: &str, root: &Path) -> u64 {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args(["rss_probe", "--exact", "--ignored", "--nocapture"])
        .args(["--test-threads=1", "-q"])
        .env(PROBE_TOOL_ENV, tool)
        .env(CHILD_REPO_ENV, root)
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
fn bulk_add_peak_rss_stays_near_git_add() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_tree(root);

    reinit(root);
    let git_kib = stage_peak_rss_kib("git", root);
    let git_entries = run(git(root).args(["ls-files", "-s"]));

    reinit(root);
    let grit_kib = stage_peak_rss_kib("grit", root);

    let ratio = grit_kib as f64 / git_kib as f64;
    eprintln!(
        "{FILES} new files: git add -A peak {git_kib} KiB, grit stage peak {grit_kib} KiB ({ratio:.2}x)"
    );
    assert_eq!(run(git(root).args(["ls-files", "-s"])), git_entries);
    run(git(root).args(["fsck", "--strict", "--no-progress"]));
    assert!(
        run(git(root).args(["diff", "--name-only"])).is_empty(),
        "git sees worktree changes after grit staged everything"
    );
    assert!(
        run(git(root).args(["ls-files", "--others", "--exclude-standard"])).is_empty(),
        "git sees untracked files after grit staged everything"
    );
    assert!(
        ratio <= MAX_RATIO,
        "grit add peaked at {grit_kib} KiB, {ratio:.2}x git add's {git_kib} KiB on {FILES} \
         files (limit {MAX_RATIO}x)"
    );
}

/// Measuring side of the gate: stage the tree in a child and print its peak RSS.
#[test]
#[ignore = "run by bulk_add_peak_rss_stays_near_git_add in a child process"]
fn rss_probe() {
    let (Some(tool), Some(root)) = (
        std::env::var_os(PROBE_TOOL_ENV),
        std::env::var_os(CHILD_REPO_ENV),
    ) else {
        return;
    };
    let root = Path::new(&root);
    let mut cmd = if tool == "git" {
        let mut cmd = git(root);
        cmd.args(["add", "-A"]);
        cmd
    } else {
        let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
        cmd.args(["stage_child", "--exact", "--ignored", "--test-threads=1"])
            .env(CHILD_REPO_ENV, root);
        cmd
    };
    run(cmd.stdout(Stdio::null()));
    // SAFETY: plain FFI call filling a zeroed, correctly sized `rusage`.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) },
        0
    );
    println!("peak_kib={}", usage.ru_maxrss);
}

/// Child side of the gate: stage everything in the repository named by
/// `GRIT_ADD_MEMORY_CHILD_REPO`, the way `grit add` does.
#[test]
#[ignore = "run by bulk_add_peak_rss_stays_near_git_add in a child process"]
fn stage_child() {
    let Some(root) = std::env::var_os(CHILD_REPO_ENV) else {
        return;
    };
    let repo = Repository::discover(Some(Path::new(&root)), &RepositoryOptions::empty())
        .expect("open repository");
    let outcome = repo
        .index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stage");
    assert_eq!(outcome.added.len(), FILES);
}
