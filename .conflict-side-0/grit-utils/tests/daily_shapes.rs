//! Daily benchmark repo shapes: deterministic generation and `git fsck`.

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use grit_lib::bench_fixture::daily_special::DailySpecialShape;
use grit_utils::binary::resolve_binary;
use grit_utils::fixture_registry::{self, FixtureKind, DEFAULT_GIT_GIT_HEAD};
use tempfile::TempDir;

static CACHE_LOCK: Mutex<()> = Mutex::new(());

fn lock_cache() -> MutexGuard<'static, ()> {
    CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn git_ref_count(git: &Path, repo: &Path) -> u64 {
    let out = Command::new(git)
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            grit_utils::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git for-each-ref");
    assert!(out.status.success(), "git for-each-ref failed");
    out.stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .count() as u64
}

fn git_max_path_slash_depth(git: &Path, repo: &Path) -> usize {
    let out = Command::new(git)
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            grit_utils::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git ls-tree");
    assert!(out.status.success(), "git ls-tree failed");
    out.stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line.iter().filter(|&&b| b == b'/').count() + 1)
        .max()
        .unwrap_or(0)
}

fn git_fsck_strict(git: &Path, repo: &Path) {
    let out = Command::new(git)
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            grit_utils::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed for {}: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = combined.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.is_empty(),
        "git fsck --strict must be silent on a clean repo ({}), got:\n{combined}",
        repo.display()
    );
}

fn assert_many_refs_packed_layout(repo: &Path) {
    let git_dir = repo.join(".git");
    assert!(
        !git_dir.join("refs/heads/main").exists(),
        "many-refs packed fixture must not have loose refs/heads/main"
    );
    assert!(
        git_dir.join("packed-refs").is_file(),
        "many-refs packed fixture must use packed-refs"
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

#[test]
fn many_refs_smoke_packed_and_loose_match_git() {
    let _guard = lock_cache();
    let git = resolve_binary("git", None).expect("git binary");
    let expected_refs = DailySpecialShape::ManyRefs.expected_ref_count(true) as u64;

    let cache_packed = TempDir::new().expect("cache packed");
    std::env::set_var("GRIT_BENCH_CACHE", cache_packed.path());
    let (packed_path, packed_record) =
        fixture_registry::ensure_cached(&git, FixtureKind::ManyRefsSmoke, true)
            .expect("many-refs-smoke");

    let cache_loose = TempDir::new().expect("cache loose");
    std::env::set_var("GRIT_BENCH_CACHE", cache_loose.path());
    let (loose_path, loose_record) =
        fixture_registry::ensure_cached(&git, FixtureKind::ManyRefsLooseSmoke, true)
            .expect("many-refs-loose-smoke");

    assert_eq!(packed_record.head_oid, loose_record.head_oid);
    assert_eq!(packed_record.ref_count, expected_refs);
    assert_eq!(loose_record.ref_count, expected_refs);
    assert_eq!(git_ref_count(&git, &packed_path), expected_refs);
    assert_eq!(git_ref_count(&git, &loose_path), expected_refs);

    assert_many_refs_packed_layout(&packed_path);
    git_fsck_strict(&git, &packed_path);
    git_fsck_strict(&git, &loose_path);

    let cache_packed_b = TempDir::new().expect("cache packed b");
    std::env::set_var("GRIT_BENCH_CACHE", cache_packed_b.path());
    let (_, packed_record_b) =
        fixture_registry::ensure_cached(&git, FixtureKind::ManyRefsSmoke, true)
            .expect("many-refs-smoke regen");
    assert_eq!(packed_record.head_oid, packed_record_b.head_oid);
}

#[test]
fn big_blobs_smoke_fscks() {
    let _guard = lock_cache();
    let git = resolve_binary("git", None).expect("git binary");
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    let (path, record) = fixture_registry::ensure_cached(&git, FixtureKind::BigBlobsSmoke, true)
        .expect("big-blobs-smoke");
    assert!(record.file_count >= 10);
    git_fsck_strict(&git, &path);

    let cache_b = TempDir::new().expect("cache b");
    std::env::set_var("GRIT_BENCH_CACHE", cache_b.path());
    let (_, record_b) = fixture_registry::ensure_cached(&git, FixtureKind::BigBlobsSmoke, true)
        .expect("big-blobs-smoke regen");
    assert_eq!(record.head_oid, record_b.head_oid);
}

#[test]
fn deep_tree_smoke_fscks() {
    let _guard = lock_cache();
    let git = resolve_binary("git", None).expect("git binary");
    let (min_depth, max_depth) = DailySpecialShape::DeepTree
        .tree_depth_range(true)
        .expect("depth range");
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    let (path, record) = fixture_registry::ensure_cached(&git, FixtureKind::DeepTreeSmoke, true)
        .expect("deep-tree-smoke");
    let depth = git_max_path_slash_depth(&git, &path);
    assert!(
        depth >= min_depth + 1 && depth <= max_depth + 1,
        "path depth {depth} outside [{}, {}]",
        min_depth + 1,
        max_depth + 1
    );
    assert!(record.file_count >= 10);
    git_fsck_strict(&git, &path);

    let cache_b = TempDir::new().expect("cache b");
    std::env::set_var("GRIT_BENCH_CACHE", cache_b.path());
    let (_, record_b) = fixture_registry::ensure_cached(&git, FixtureKind::DeepTreeSmoke, true)
        .expect("deep-tree-smoke regen");
    assert_eq!(record.head_oid, record_b.head_oid);
}
