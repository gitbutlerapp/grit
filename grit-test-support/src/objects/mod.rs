//! Byte-level pack fixtures and system-`git` validators for object-database tests.
//!
//! [`PackBuilder`] writes PACK v2 streams with full objects and deltas; helpers install
//! packs through system `git index-pack` and run structured compatibility checks.

mod corrupt;
mod delta_ops;
mod git_check;
mod hash;
mod install;
mod loose;
mod pack_builder;
mod repo_fixture;

pub use corrupt::{flip_byte_at, overwrite_range, truncate_file};
pub use delta_ops::DeltaOps;
pub use git_check::{
    git_cat_file_batch_check, git_commit_graph_verify, git_fsck, git_midx_verify,
    git_supports_sha256, git_verify_pack, BatchCheckOutcome, FsckOutcome, GitToolOutcome,
};
pub use hash::HashAlgo;
pub use install::{write_pack_and_index, IndexPackOptions, IndexVersion, PackInstallOutcome};
pub use loose::{hash_loose_object, write_loose_object};
pub use pack_builder::{ObjectKind, PackBuilder, PackBuilt, PackOffsetLabel};
pub use repo_fixture::RepoFixture;

#[cfg(test)]
mod tests;
