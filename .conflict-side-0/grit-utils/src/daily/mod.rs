//! Daily performance census report schema, scenario registry, and direct-spawn runner.

pub mod registry;
pub mod runner;
pub mod schema;
pub mod validate;

pub use registry::{Area, HeadlineScenario, RepoShape, ScenarioSpec, HEADLINE_SCENARIOS};
pub use schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, ScenarioId, SCHEMA_TAG,
};
pub use validate::validate_daily_report;
