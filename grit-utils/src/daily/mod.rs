//! Daily performance census report schema, scenario registry, and direct-spawn runner.

pub mod areas;
pub mod helpers;
pub mod plan;
pub mod registry;
pub mod remote_fixture;
pub mod remote_state;
pub mod run;
pub mod runner;
pub mod schema;
pub mod validate;

pub use run::{run_daily, DailyRunOptions};

pub use registry::{Area, HeadlineScenario, RepoShape, ScenarioSpec, HEADLINE_SCENARIOS};
pub use schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, ScenarioId, SCHEMA_TAG,
};
pub use validate::validate_daily_report;
