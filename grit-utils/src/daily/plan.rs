//! Executable plan for one daily headline scenario.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::registry::ScenarioSpec;
use super::runner::BenchArgv;
use crate::scenarios::PrepareKind;

/// How to prepare the repo before each timed iteration.
pub enum DailyPrepare {
    None,
    Hook(PrepareKind),
}

/// One headline row the daily runner can execute (or mark pending).
pub struct DailyHeadlinePlan {
    pub spec: &'static ScenarioSpec,
    pub pending: bool,
    pub setup: fn(&DailySetup<'_>) -> Result<DailyHeadlineContext>,
    pub prepare: DailyPrepare,
    pub build_git: fn(&DailyHeadlineContext) -> BenchArgv,
    pub build_grit: fn(&DailyHeadlineContext) -> BenchArgv,
}

/// Inputs shared while building plans for a run.
pub struct DailySetup<'a> {
    pub git: &'a Path,
    pub grit: &'a Path,
    pub smoke: bool,
    pub offline: bool,
}

/// Resolved repo and tool paths for one scenario.
pub struct DailyHeadlineContext {
    pub repo: PathBuf,
    pub git: PathBuf,
    pub grit: PathBuf,
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
}

impl DailyHeadlineContext {
    pub fn new(repo: PathBuf, git: &Path, grit: &Path, smoke: bool) -> Self {
        Self {
            repo,
            git: git.to_path_buf(),
            grit: grit.to_path_buf(),
            blame_path: None,
            hot_path_meta: None,
            smoke,
            matrix_path: None,
            merge_base_a: None,
            merge_base_b: None,
            shortlog_git_range: None,
        }
    }
}
