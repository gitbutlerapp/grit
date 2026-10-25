//! Gate guard for issue #1690: `grit repack` (and `--force`) on a cargo-shaped
//! repository must stay within twice the peak memory of `git repack -ad` /
//! `-adf`, and within twice the time in optimized builds, with a pack no more
//! than 1.2x the size of git's.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use grit_utils::repack_fixture::{build_cargo_shape_repo, CargoShape};
use tempfile::TempDir;

const TIME_SLACK: Duration = Duration::from_millis(500);
const RSS_SLACK: u64 = 8 * 1024 * 1024;

fn resolve_git() -> PathBuf {
    grit_utils::binary::resolve_binary("git", None).expect("git")
}

/// Build the `grit` binary with the same profile as this test.
fn build_grit() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let release = !cfg!(debug_assertions);
    let mut args = vec!["build", "-p", "grit-cli", "--bin", "grit", "-q"];
    if release {
        args.push("--release");
    }
    let status = Command::new(env!("CARGO"))
        .args(&args)
        .current_dir(&workspace)
        .status()
        .expect("cargo build grit");
    assert!(status.success());
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    target
        .join(if release { "release" } else { "debug" })
        .join(format!("grit{}", std::env::consts::EXE_SUFFIX))
}

/// Peak RSS and wall time of `command`, run in a fresh `grit-bench measure-rss` helper.
fn measure(cwd: &Path, command: &str) -> (u64, Duration) {
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_grit-bench"))
        .arg("measure-rss")
        .arg("--cwd")
        .arg(cwd)
        .env("GRIT_BENCH_MEASURE_CMD", command)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("measure-rss");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rss = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss");
    (rss, elapsed)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("mkdir");
    for entry in std::fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy");
        }
    }
}

fn pack_bytes(repo: &Path) -> u64 {
    std::fs::read_dir(repo.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .map(|e| e.metadata().expect("metadata").len())
        .sum()
}

fn git_fsck(git: &Path, repo: &Path) {
    let out = Command::new(git)
        .args(["fsck", "--strict", "--no-dangling"])
        .current_dir(repo)
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn quote(path: &Path) -> String {
    grit_utils::shell::shell_quote(path.to_str().expect("utf-8 path"))
}

#[test]
fn repack_stays_within_git_budgets_on_cargo_shape() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let git = resolve_git();
    let grit = build_grit();
    let tmp = TempDir::new().expect("tempdir");
    let fixture = tmp.path().join("fixture");
    build_cargo_shape_repo(&git, &fixture, CargoShape::SMOKE).expect("fixture");

    // Code pages and start-up allocations do not scale with the repository (and
    // are much larger for a debug build), so memory is compared above each
    // tool's own baseline: reading one commit in the same repository.
    let (grit_base, _) = measure(&fixture, &format!("{} log -n 1", quote(&grit)));
    let (git_base, _) = measure(&fixture, &format!("{} log -1", quote(&git)));
    let optimized = !cfg!(debug_assertions);
    for (label, git_args, grit_args) in [
        ("repack", "repack -adq", "repack -q"),
        ("repack --force", "repack -adfq", "repack -q --force"),
    ] {
        let git_repo = tmp.path().join(format!("git-{}", label.replace(' ', "")));
        let grit_repo = tmp.path().join(format!("grit-{}", label.replace(' ', "")));
        copy_dir(&fixture, &git_repo);
        copy_dir(&fixture, &grit_repo);

        let (git_rss, git_time) = measure(&git_repo, &format!("{} {git_args}", quote(&git)));
        let (grit_rss, grit_time) = measure(&grit_repo, &format!("{} {grit_args}", quote(&grit)));
        git_fsck(&git, &grit_repo);
        let (git_size, grit_size) = (pack_bytes(&git_repo), pack_bytes(&grit_repo));
        eprintln!(
            "{label}: git {git_time:?} {git_rss} B rss (base {git_base} B) {git_size} B pack; \
             grit {grit_time:?} {grit_rss} B rss (base {grit_base} B) {grit_size} B pack"
        );

        let grit_excess = grit_rss.saturating_sub(grit_base);
        let git_excess = git_rss.saturating_sub(git_base);
        assert!(
            grit_excess <= 2 * git_excess + RSS_SLACK,
            "{label}: grit peak RSS {grit_rss} B ({grit_excess} B over `log -n 1`) \
             vs git {git_rss} B ({git_excess} B over `log -1`)"
        );
        assert!(
            grit_size * 10 <= git_size * 12,
            "{label}: grit pack {grit_size} B vs git {git_size} B"
        );
        if optimized {
            assert!(
                grit_time <= 2 * git_time + TIME_SLACK,
                "{label}: grit took {grit_time:?} vs git {git_time:?}"
            );
        }
    }
}
