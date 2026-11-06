//! Unstable hooks for workspace tests and internal tooling.
//!
//! This module is **unstable** and outside semver: it is excluded from
//! [`public-api.txt`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/public-api.txt)
//! and `make semver-check`, and may change in any release. Enable the
//! [`test-support`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/Cargo.toml)
//! feature from dev-dependencies and internal crates only (see [`TESTING.md`](https://github.com/gitbutlerapp/grit/blob/main/TESTING.md)).
//!
//! ```rust,ignore
//! // Available with the `test-support` feature (workspace tests only).
//! use grit_lib::test_support::fixtures::synth::SynthTier;
//! let _ = SynthTier::S;
//! ```
//!
//! [`fixtures`] holds deterministic repo generators for `grit-bench` and workspace tests.
//! [`ewah`] exposes Criterion fixtures for EWAH bitmap benches.

/// Criterion fixtures for EWAH bitmap benches (`benches/ewah.rs`).
pub mod ewah;
/// Deterministic synthetic repositories for `grit-bench` and workspace tests.
pub mod fixtures;

#[cfg(any(test, feature = "test-support"))]
pub mod counters;

#[cfg(any(test, feature = "test-support"))]
pub mod inject;

#[cfg(any(test, feature = "test-support"))]
pub mod status;

pub mod commit_graph;
pub mod index_pack;
pub mod pack;
pub mod pack_store;
pub mod pathspec;

/// Expanded pack re-exports for integration tests (see module docs).
pub mod objects_pack;

#[cfg(any(test, feature = "test-support"))]
pub mod fuzz;

#[cfg(any(test, feature = "test-support"))]
pub mod knobs;

pub mod git_date;
