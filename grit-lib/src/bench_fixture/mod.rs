//! Deterministic repositories and helpers for benchmark fixtures.
//!
//! The [`synth`] module builds seeded repos for `grit-bench` without shelling out to `git`.
//!
//! ```rust
//! use grit_lib::bench_fixture::synth::SynthTier;
//! # fn main() {
//! let _ = SynthTier::M;
//! # }
//! ```

pub mod daily_special;
pub mod synth;
