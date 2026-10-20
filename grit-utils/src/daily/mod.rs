//! Daily performance census report schema and scenario registry.

pub mod registry;
pub mod schema;
pub mod validate;

pub use registry::{Area, HeadlineScenario, RepoShape, ScenarioSpec, HEADLINE_SCENARIOS};
pub use schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, ScenarioId, SCHEMA_TAG,
};
pub use validate::validate_daily_report;
