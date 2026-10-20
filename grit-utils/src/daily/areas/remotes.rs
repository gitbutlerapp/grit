//! Remote headline scenarios (stubbed pending until step 6).

use super::super::helpers::headline_spec;
use super::super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use super::super::runner::BenchArgv;

fn pending_setup(
    _setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    anyhow::bail!("remote headline is pending implementation")
}

fn pending_argv(_ctx: &super::super::plan::DailyHeadlineContext) -> BenchArgv {
    BenchArgv::new("false", Vec::<&str>::new())
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    [
        "fetch/incremental@git-git",
        "pull/fast-forward@git-git",
        "push/incremental@git-git",
        "clone/local@synth-10k",
    ]
    .into_iter()
    .map(|id| DailyHeadlinePlan {
        spec: headline_spec(id),
        pending: true,
        setup: pending_setup,
        prepare: DailyPrepare::None,
        build_git: pending_argv,
        build_grit: pending_argv,
    })
    .collect()
}
