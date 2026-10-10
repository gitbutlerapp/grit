//! grit-bench: scenario-based benchmarking of grit vs git via hyperfine.

pub mod bench_env;
pub mod binary;
pub mod bitmap_fixture;
pub mod bitmap_suite;
pub mod capped_run;
pub mod compare;
pub mod fixture;
pub mod hot_path_fixture;
pub mod hyperfine;
pub mod machine;
pub mod odb_driver;
pub mod odb_fixture;
pub mod odb_suite;
pub mod render;
pub mod resource;
pub mod scenarios;
pub mod schema;
pub mod serve_request;
pub mod shell;
pub mod stats;

pub use compare::compare_reports;
pub use schema::{BenchReport, ScenarioResult};
