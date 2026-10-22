//! Regression tests for daily maintenance preflight (grit output + system git fsck).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use grit_utils::daily::matrix_helpers::{
    commit_graph_present, correctness_grit_commit_graph, count_loose_objects,
    preflight_grit_gc_then_fsck, run_prepare_daily_gc_seed, run_prepare_daily_reset,
    setup_synth10k_gc, verify_git_fsck,
};
use grit_utils::daily::plan::DailySetup;
use grit_utils::fixture_registry::FixtureKind;
use grit_utils::worktree_fixture::set_worktree_bench_cwd;
use tempfile::TempDir;

static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn lock_fixtures() -> MutexGuard<'static, ()> {
    std::env::remove_var("GRIT_BENCH_DAILY_ROOT");
    FIXTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn resolve_git() -> std::path::PathBuf {
    grit_utils::binary::resolve_binary("git", None).expect("git")
}

fn bench_exe() -> std::path::PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"))
}

fn build_grit() -> std::path::PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "grit-cli", "--bin", "grit", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("cargo build grit");
    assert!(status.success());
    workspace.join("target/debug/grit")
}

#[test]
fn preflight_grit_repack_reduces_loose_objects_and_fscks() {
    let _guard = lock_fixtures();
    let git = resolve_git();
    let grit = build_grit();
    let bench = bench_exe();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    grit_utils::fixture_registry::ensure_cached(&git, FixtureKind::Synth10kSmoke, true)
        .expect("synth10k smoke");

    let setup = DailySetup {
        git: &git,
        grit: &grit,
        bench: &bench,
        smoke: true,
        offline: true,
    };
    let ctx = setup_synth10k_gc(&setup).expect("setup");
    set_worktree_bench_cwd(&ctx.repo).expect("pin cwd");
    run_prepare_daily_gc_seed(&git).expect("seed gc state");

    let loose_before = count_loose_objects(&ctx.repo).expect("count loose before");
    assert!(
        loose_before > 0,
        "expected seeded loose objects before repack preflight"
    );

    preflight_grit_gc_then_fsck(&ctx, &["gc", "repack"]).expect("grit repack preflight");

    let loose_after = count_loose_objects(&ctx.repo).expect("count loose after");
    assert!(
        loose_after < loose_before,
        "grit gc repack should pack loose objects (before={loose_before}, after={loose_after})"
    );
}

#[test]
fn preflight_fsck_only_does_not_pack_loose_objects() {
    let _guard = lock_fixtures();
    let git = resolve_git();
    let grit = build_grit();
    let bench = bench_exe();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    grit_utils::fixture_registry::ensure_cached(&git, FixtureKind::Synth10kSmoke, true)
        .expect("synth10k smoke");

    let setup = DailySetup {
        git: &git,
        grit: &grit,
        bench: &bench,
        smoke: true,
        offline: true,
    };
    let ctx = setup_synth10k_gc(&setup).expect("setup");
    set_worktree_bench_cwd(&ctx.repo).expect("pin cwd");
    run_prepare_daily_gc_seed(&git).expect("seed gc state");

    let loose_before = count_loose_objects(&ctx.repo).expect("count loose before");
    assert!(loose_before > 0);

    grit_utils::daily::matrix_helpers::run_prepare_daily_reset(&git).expect("reset");
    grit_utils::daily::matrix_helpers::verify_git_fsck(&ctx).expect("fsck only");

    let loose_after = count_loose_objects(&ctx.repo).expect("count loose after");
    assert_eq!(
        loose_after, loose_before,
        "fsck alone must not pack objects; preflight must run grit gc repack"
    );
}

#[test]
fn preflight_grit_commit_graph_writes_file_and_verifies() {
    let _guard = lock_fixtures();
    let git = resolve_git();
    let grit = build_grit();
    let bench = bench_exe();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    grit_utils::fixture_registry::ensure_cached(&git, FixtureKind::Synth10kSmoke, true)
        .expect("synth10k smoke");

    let setup = DailySetup {
        git: &git,
        grit: &grit,
        bench: &bench,
        smoke: true,
        offline: true,
    };
    let ctx = setup_synth10k_gc(&setup).expect("setup");
    set_worktree_bench_cwd(&ctx.repo).expect("pin cwd");
    run_prepare_daily_gc_seed(&git).expect("seed gc state");
    run_prepare_daily_reset(&git).expect("reset before commit-graph preflight");
    assert!(
        !commit_graph_present(&ctx.repo),
        "fixture should start without a commit-graph file"
    );

    correctness_grit_commit_graph(&ctx).expect("grit commit-graph preflight");

    assert!(
        commit_graph_present(&ctx.repo),
        "grit gc commit-graph must write objects/info/commit-graph"
    );
}

#[test]
fn preflight_fsck_only_does_not_create_commit_graph() {
    let _guard = lock_fixtures();
    let git = resolve_git();
    let grit = build_grit();
    let bench = bench_exe();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    grit_utils::fixture_registry::ensure_cached(&git, FixtureKind::Synth10kSmoke, true)
        .expect("synth10k smoke");

    let setup = DailySetup {
        git: &git,
        grit: &grit,
        bench: &bench,
        smoke: true,
        offline: true,
    };
    let ctx = setup_synth10k_gc(&setup).expect("setup");
    set_worktree_bench_cwd(&ctx.repo).expect("pin cwd");
    run_prepare_daily_gc_seed(&git).expect("seed gc state");
    run_prepare_daily_reset(&git).expect("reset");
    assert!(!commit_graph_present(&ctx.repo));

    verify_git_fsck(&ctx).expect("fsck only");

    assert!(
        !commit_graph_present(&ctx.repo),
        "fsck alone must not create commit-graph; preflight must run grit gc commit-graph"
    );
}
