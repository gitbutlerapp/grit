//! Daily performance census report schema, scenario registry, and direct-spawn runner.

pub mod areas;
pub mod coverage;
pub mod helpers;
pub mod matrix_helpers;
pub mod matrix_registry;
pub mod plan;
pub mod registry;
pub mod remote_fixture;
pub mod remote_state;
pub mod revision_selection_matrix_registry;
pub mod run;
pub mod runner;
pub mod schema;
pub mod validate;

pub use run::{run_daily, DailyRunOptions};

pub use coverage::all_registry_scenarios;
pub use matrix_registry::MATRIX_SCENARIOS;
pub use registry::{
    Area, HeadlineScenario, RepoShape, ScenarioSpec, HEADLINE_SCENARIOS, REMOTE_MATRIX_SCENARIOS,
};
pub use revision_selection_matrix_registry::REVISION_SELECTION_MATRIX_SCENARIOS;
pub use schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, ScenarioId, SCHEMA_TAG,
};
pub use validate::validate_daily_report;
