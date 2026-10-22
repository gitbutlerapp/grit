//! Executable plan for one daily headline scenario.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::registry::ScenarioSpec;
use super::remote_state::DailyRemoteState;
use super::runner::BenchArgv;
use crate::scenarios::PrepareKind;

/// How to prepare the repo before each timed iteration.
pub enum DailyPrepare {
    None,
    Hook(PrepareKind),
    /// Run a shell prepare snippet (after optional hook prepare is not composed; use one variant).
    Shell(BenchArgv),
    /// Reset via `grit-bench prepare-network-*` using `GRIT_BENCH_DAILY_NETWORK_BARE`.
    Network(&'static str),
}

/// One daily scenario row the runner can execute (or mark pending).
pub struct DailyHeadlinePlan {
    pub spec: &'static ScenarioSpec,
    pub pending: bool,
    pub setup: fn(&DailySetup<'_>) -> Result<DailyHeadlineContext>,
    pub prepare: fn(&DailyHeadlineContext) -> DailyPrepare,
    pub build_git: fn(&DailyHeadlineContext) -> BenchArgv,
    pub build_grit: fn(&DailyHeadlineContext) -> BenchArgv,
    pub correctness: Option<fn(&DailyHeadlineContext) -> Result<()>>,
}

/// Inputs shared while building plans for a run.
pub struct DailySetup<'a> {
    pub git: &'a Path,
    pub grit: &'a Path,
    /// `grit-bench` executable (library/ref workloads such as `refs-driver`).
    pub bench: &'a Path,
    pub smoke: bool,
    pub offline: bool,
}

/// Resolved repo and tool paths for one scenario.
pub struct DailyHeadlineContext {
    pub repo: PathBuf,
    pub git: PathBuf,
    pub grit: PathBuf,
    pub bench: PathBuf,
    /// Optional blame target for `blame/file@git-git`.
    pub blame_path: Option<String>,
    /// Hot-path fixture metadata for switch / pick / merge headlines.
    pub hot_path_meta: Option<crate::hot_path_fixture::HotPathMeta>,
    /// Smoke mode (smaller fixtures, single timed sample).
    pub smoke: bool,
    /// Path for log/blame/restore matrix rows.
    pub matrix_path: Option<String>,
    pub merge_base_a: Option<String>,
    pub merge_base_b: Option<String>,
    pub shortlog_git_range: Option<String>,
    /// Blob object id for `cat/blob@git-git` (from `git rev-parse HEAD:<path>`).
    pub cat_blob: Option<String>,
    /// Change-id prefix for `resolve/change-id@git-git` (derived from `HEAD~N`).
    pub change_id_bench_prefix: Option<String>,
    /// History rewrite fixture metadata for `history/reword@synth-10k`.
    pub history_meta: Option<crate::history_rewrite_fixture::HistoryRewriteMeta>,
    /// Path to a bundle file for bundle scenarios.
    pub bundle_path: Option<PathBuf>,
    /// Linked worktree path for worktree add/remove scenarios.
    pub linked_worktree: Option<PathBuf>,
    /// Branch name targeted by create/delete benchmarks on many-refs fixtures.
    pub branch_bench_name: Option<String>,
    /// Tag name targeted by create benchmarks on many-refs fixtures.
    pub tag_bench_name: Option<String>,
    /// Commit oid for `tag/contains@git-git` (`--contains` target).
    pub ref_query_contains: Option<String>,
    /// Commit oid for `branch/merged@many-refs` (`--merged` target).
    pub ref_query_merged: Option<String>,
    /// Network transport state (bare remote, URL, HTTP servers).
    pub remote: Option<DailyRemoteState>,
}

impl DailyHeadlineContext {
    pub fn new(repo: PathBuf, git: &Path, grit: &Path, bench: &Path, smoke: bool) -> Self {
        Self {
            repo,
            git: git.to_path_buf(),
            grit: grit.to_path_buf(),
            bench: bench.to_path_buf(),
            blame_path: None,
            hot_path_meta: None,
            smoke,
            matrix_path: None,
            merge_base_a: None,
            merge_base_b: None,
            shortlog_git_range: None,
            cat_blob: None,
            change_id_bench_prefix: None,
            history_meta: None,
            bundle_path: None,
            linked_worktree: None,
            branch_bench_name: None,
            tag_bench_name: None,
            ref_query_contains: None,
            ref_query_merged: None,
            remote: None,
        }
    }
}
