//! Daily performance census report schema, scenario registry, and direct-spawn runner.

pub mod areas;
pub mod helpers;
pub mod matrix_registry;
pub mod plan;
pub mod registry;
pub mod run;
pub mod runner;
pub mod schema;
pub mod validate;

pub use run::{run_daily, DailyRunOptions};

pub use matrix_registry::MATRIX_SCENARIOS;
pub use registry::{Area, HeadlineScenario, RepoShape, ScenarioSpec, HEADLINE_SCENARIOS};
pub use schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, ScenarioId, SCHEMA_TAG,
};
pub use validate::validate_daily_report;
