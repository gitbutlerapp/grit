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
//! let _ = grit_lib::test_support;
//! ```
