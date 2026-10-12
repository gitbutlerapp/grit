//! Shared integration-test helpers (refs backend harness, etc.).

mod attributes_harness;
mod config_oracle;
mod diff_harness;
mod history_harness;
mod refs_harness;
mod refstore_conformance;

pub use attributes_harness::*;
pub use config_oracle::*;
pub use diff_harness::*;
pub use history_harness::{
    build_commit_dag, commit_file, git_ls_files_stage, git_ls_files_stage0_map, git_rev_list,
    git_rev_parse, git_version, git_version_at_least, skip_unless_git_at_least, HistoryRepo,
};
pub use refs_harness::*;
pub use refstore_conformance::*;
