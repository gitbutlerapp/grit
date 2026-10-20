//! Daily benchmark core repo shapes: deterministic generation and `git fsck`.

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use grit_utils::binary::resolve_binary;
use grit_utils::fixture_registry::{self, FixtureKind, DEFAULT_GIT_GIT_HEAD};
use tempfile::TempDir;

static CACHE_LOCK: Mutex<()> = Mutex::new(());

fn lock_cache() -> MutexGuard<'static, ()> {
    CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn git_fsck_strict(git: &Path, repo: &Path) {
    let status = Command::new(git)
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            grit_utils::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git fsck");
    assert!(
        status.success(),
        "git fsck --strict failed for {}",
        repo.display()
    );
}

fn assert_shape_deterministic_and_fscks(kind: FixtureKind) {
    let _guard = lock_cache();
    let git = resolve_binary("git", None).expect("git binary");

    let cache_a = TempDir::new().expect("cache a");
    std::env::set_var("GRIT_BENCH_CACHE", cache_a.path());
    let (path_a, record_a) =
        fixture_registry::ensure_cached(&git, kind, true).expect("first prepare");

    let cache_b = TempDir::new().expect("cache b");
    std::env::set_var("GRIT_BENCH_CACHE", cache_b.path());
    let (path_b, record_b) =
        fixture_registry::ensure_cached(&git, kind, true).expect("second prepare");

    assert_eq!(
        record_a.head_oid, record_b.head_oid,
        "HEAD must be identical across regenerations for {kind:?}"
    );

    git_fsck_strict(&git, &path_a);
    git_fsck_strict(&git, &path_b);
}

#[test]
fn small_shape_deterministic_and_fscks() {
    assert_shape_deterministic_and_fscks(FixtureKind::Small);
}

#[test]
fn synth_10k_smoke_deterministic_and_fscks() {
    assert_shape_deterministic_and_fscks(FixtureKind::Synth10kSmoke);
}

#[test]
fn synth_100k_smoke_deterministic_and_fscks() {
    assert_shape_deterministic_and_fscks(FixtureKind::Synth100kSmoke);
}

#[test]
#[ignore = "network: clones upstream git.git"]
fn git_git_fixture_pinned_head() {
    let _guard = lock_cache();
    let git = resolve_binary("git", None).expect("git binary");
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());

    let (_, bare) =
        fixture_registry::ensure_cached(&git, FixtureKind::GitGit, false).expect("bare git-git");
    assert_eq!(bare.head_oid, DEFAULT_GIT_GIT_HEAD);

    let (_, wt) = fixture_registry::ensure_cached(&git, FixtureKind::GitGitWt, false)
        .expect("git-git worktree");
    assert_eq!(wt.head_oid, DEFAULT_GIT_GIT_HEAD);
}
