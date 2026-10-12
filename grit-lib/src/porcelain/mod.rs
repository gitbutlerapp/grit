//! Porcelain operations — user-facing workflows that assemble structured result
//! models from domain modules (e.g. [`diff`](crate::diff), [`revwalk`](crate::revwalk),
//! `status`, `log`, `merge`, `rebase`, `checkout`, and related commands).
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
//! column layout, and exit codes all live in the `grit` binary. New commands add
//! a submodule here and a matching thin handler in `grit-cli`.
//!
//! - [`add::stage`] — stage worktree changes into the index without a full status pass.

pub mod add;
pub mod branch;
pub mod checkout;
pub mod checkout_reflog;
pub mod cherry_pick;
pub mod clean;
pub mod commit;
pub mod log;
pub mod merge;
pub mod paths;
pub mod rebase;
pub mod replay;
pub mod reset;
pub mod restore;
pub mod revert;
pub mod stage_tracked;
pub mod staging;
pub mod stash;
pub mod status;
pub mod switch;
pub mod tag;
pub mod worktree_guard;
