//! Standalone executable for `history_rebase.rs` (runs when the unified integration
//! binary does not build).

mod support;

#[path = "history_rebase.rs"]
mod history_rebase;
