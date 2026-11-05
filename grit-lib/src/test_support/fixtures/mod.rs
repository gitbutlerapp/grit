//! Deterministic repositories and helpers for benchmark fixtures.
//!
//! The [`synth`](crate::test_support::fixtures::synth) module builds seeded repos for `grit-bench` without shelling out to `git`.
//!
//! ```rust,ignore
//! use grit_lib::test_support::fixtures::synth::SynthTier;
//! let _ = SynthTier::M;
//! ```

pub mod daily_special;
pub mod synth;
