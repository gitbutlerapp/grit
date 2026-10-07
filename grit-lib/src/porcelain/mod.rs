//! Porcelain operations — user-facing engines that assemble a structured result
//! model from [`plumbing`](crate::plumbing) pieces (e.g. `status`, `log`,
//! `merge`, `rebase`, `checkout`).
//!
//! # The library/CLI contract
//!
//! Each operation follows the same triple:
//!
//! 1. an **options struct** of plain data (translated by the CLI from clap args
//!    + config) — no `clap` types here;
//! 2. an **operation function** that computes and returns a **result model**,
//!    reporting progress through a caller-supplied
//!    [`ProgressSink`](crate::progress::ProgressSink) and never touching the
//!    terminal; and
//! 3. a **result model** of plain `struct`/`enum`s that the CLI renders.
//!
//! The library makes no presentation decisions — colour, pager, tty detection,
//! column layout, and exit codes all live in the `grit` binary. This module is
//! the home for logic extracted out of `grit/src/commands/`.
//!
//! Operation modules are added one command at a time; this file is the
//! scaffolding they attach to.

pub mod checkout;
pub mod cherry_pick;
pub mod format_patch;
pub mod log;
pub mod merge;
pub mod rebase;
pub mod revert;
pub mod stage_tracked;
pub mod staging;
pub mod stash;
pub mod status;
pub mod tag;
