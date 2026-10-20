//! Executable plan for one daily headline scenario.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::registry::HeadlineScenario;
use super::remote_state::DailyRemoteState;
use super::runner::BenchArgv;
use crate::scenarios::PrepareKind;

/// One headline row the daily runner can execute (or mark pending).
pub struct DailyHeadlinePlan {
    pub spec: &'static HeadlineScenario,
    pub pending: bool,
    pub setup: fn(&DailySetup<'_>) -> Result<DailyHeadlineContext>,
    pub prepare: DailyPrepare,
    pub build_git: fn(&DailyHeadlineContext) -> BenchArgv,
    pub build_grit: fn(&DailyHeadlineContext) -> BenchArgv,
    /// Optional git vs grit ref parity check before timed runs.
    pub verify: Option<fn(&DailyHeadlineContext, &Path) -> Result<()>>,
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
    /// Network transport state (bare remote, URL, HTTP servers).
    pub remote: Option<DailyRemoteState>,
}

impl DailyHeadlineContext {
    pub fn new(repo: PathBuf, git: &Path, grit: &Path) -> Self {
        Self {
            repo,
            git: git.to_path_buf(),
            grit: grit.to_path_buf(),
            blame_path: None,
            hot_path_meta: None,
            remote: None,
        }
    }
}

/// Reset hook via `grit-bench prepare-network-*` subcommands.
pub enum DailyPrepare {
    None,
    Hook(PrepareKind),
    Network(&'static str),
}
