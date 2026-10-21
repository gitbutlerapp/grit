//! Resolve and cat benchmark scenarios (isolated from the main hot-path suite).

use std::path::Path;

use anyhow::Result;

use crate::fixture::create_repo_with_history;
use crate::scenarios::{build_report, run_scenario, Driver, RunConfig, Scenario};
use crate::schema::BenchReport;
use time::OffsetDateTime;

const RESOLVE_DEPTH: usize = 20;
const HISTORY_COMMITS: usize = 80;
const FILE_COUNT: usize = 500;

/// Run resolve/cat CLI scenarios against a synthetic repo with deep history.
pub fn run_resolve_cat_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let repo = create_repo_with_history(git, FILE_COUNT, HISTORY_COMMITS)?;
    let rev = format!("HEAD~{RESOLVE_DEPTH}");
    let blob_spec = "HEAD:d0000/f0000.txt".to_string();

    let scenarios = vec![
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("resolve-HEAD~{RESOLVE_DEPTH}"),
                group: "resolve-cat".into(),
                fixture: format!("synthetic-{FILE_COUNT}-history-{HISTORY_COMMITS}"),
                description: format!(
                    "resolve HEAD~{RESOLVE_DEPTH} vs git rev-parse (process start included)"
                ),
                grit_argv: vec!["resolve".into(), rev.clone()],
                git_argv: vec!["rev-parse".into(), rev],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: "cat-blob".into(),
                group: "resolve-cat".into(),
                fixture: format!("synthetic-{FILE_COUNT}-history-{HISTORY_COMMITS}"),
                description: "cat HEAD:path vs git cat-file -p (process start included)".into(),
                grit_argv: vec!["cat".into(), blob_spec.clone()],
                git_argv: vec!["cat-file".into(), "-p".into(), blob_spec],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?,
    ];

    Ok(build_report(git, grit, timestamp, scenarios))
}
