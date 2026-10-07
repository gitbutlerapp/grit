//! grit-bench: scenario-based benchmarking of grit vs git via hyperfine.

pub mod bench_env;
pub mod binary;
pub mod compare;
pub mod fixture;
pub mod hot_path_fixture;
pub mod hyperfine;
pub mod machine;
pub mod render;
pub mod scenarios;
pub mod schema;
pub mod shell;
pub mod stats;

pub use compare::compare_reports;
pub use schema::{BenchReport, ScenarioResult};
