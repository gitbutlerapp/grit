//! Fixed-visible-workset sparse reset/switch scaling scenarios.

use anyhow::{ensure, Result};

use super::super::helpers::scenario_spec;
use super::super::plan::{DailyHeadlineContext, DailyHeadlinePlan, DailyPrepare, DailySetup};
use super::super::runner::BenchArgv;
use crate::real_repo_fixture::run_git;
use crate::scenarios::sparse_transition_argv;

fn setup(setup: &DailySetup<'_>, paths: usize, unique: bool) -> Result<DailyHeadlineContext> {
    let dir = crate::fixture::scratch_dir()
        .join(format!("sparse-{paths}-{unique}-smoke-{}", setup.smoke));
    if !dir.join(".git").is_dir() {
        grit_test_support::sparse_scale::create(
            setup.git,
            &dir,
            if setup.smoke { 2400 } else { paths },
            unique,
            if unique {
                if setup.smoke {
                    400
                } else {
                    5000
                }
            } else {
                0
            },
        )?;
    }
    run_git(setup.git, &dir, &["reset", "--hard", "sparse-edited"])?;
    Ok(DailyHeadlineContext::new(
        dir,
        setup.git,
        setup.grit,
        setup.bench,
        setup.smoke,
    ))
}

fn setup120(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    self::setup(setup, 120_000, false)
}
fn setup240(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    self::setup(setup, 240_000, false)
}
fn setup_unique(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    self::setup(setup, 120_000, true)
}

fn verify(context: &DailyHeadlineContext) -> Result<()> {
    for args in [
        ["ls-files", "--sparse"],
        ["status", "--porcelain"],
        ["fsck", "--strict"],
    ] {
        let mut cmd = std::process::Command::new(&context.git);
        cmd.args(args).current_dir(&context.repo);
        crate::real_repo_fixture::git_env(&mut cmd);
        let output = cmd.output()?;
        ensure!(
            output.status.success(),
            "Git sparse verification failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args[0] == "ls-files" {
            ensure!(
                String::from_utf8_lossy(&output.stdout).lines().count() == 700,
                "sparse index must have 700 rows"
            );
        } else if args[0] == "status" {
            ensure!(output.stdout.is_empty(), "dirty sparse fixture");
        }
    }
    Ok(())
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    type Setup = fn(&DailySetup<'_>) -> Result<DailyHeadlineContext>;
    let mut plans = Vec::new();
    for (reset, switch, setup) in [
        (
            "reset/sparse@sparse-120k",
            "switch/sparse@sparse-120k",
            setup120 as Setup,
        ),
        (
            "reset/sparse@sparse-240k",
            "switch/sparse@sparse-240k",
            setup240 as Setup,
        ),
        (
            "reset/sparse@sparse-unique-120k",
            "switch/sparse@sparse-unique-120k",
            setup_unique as Setup,
        ),
    ] {
        plans.push(DailyHeadlinePlan {
            spec: scenario_spec(reset),
            pending: false,
            setup,
            prepare: |_| DailyPrepare::None,
            build_git: |c| BenchArgv::new("/bin/sh", sparse_transition_argv(&c.git, "reset")),
            build_grit: |c| BenchArgv::new("/bin/sh", sparse_transition_argv(&c.grit, "reset")),
            correctness: Some(verify),
        });
        plans.push(DailyHeadlinePlan {
            spec: scenario_spec(switch),
            pending: false,
            setup,
            prepare: |_| DailyPrepare::None,
            build_git: |c| BenchArgv::new("/bin/sh", sparse_transition_argv(&c.git, "switch")),
            build_grit: |c| BenchArgv::new("/bin/sh", sparse_transition_argv(&c.grit, "switch")),
            correctness: Some(verify),
        });
    }
    plans
}
