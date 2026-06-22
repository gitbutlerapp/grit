//! Output mode plumbing shared by every `grit` command.
//!
//! Each command computes a typed, [`serde::Serialize`] *outcome* and returns it;
//! the dispatcher in `main` then renders that outcome exactly once, either as the
//! command's normal human-readable text or as a single JSON object on stdout
//! (`--json`). An optional global `--filter` applies a jq-like expression to that
//! object so callers can select just the fields they need.
//!
//! ## Contract for `--json`
//!
//! * stdout carries exactly **one** JSON value: the command's outcome object on
//!   success (optionally narrowed by `--filter`), or `{"error": "…"}` on failure.
//! * the process still exits non-zero on failure, so consumers can branch on the
//!   exit code *or* the presence of an `error` key.
//! * progress / prompts / diagnostics go to **stderr** and never pollute stdout.

use anyhow::{bail, Result};
use grit_lib::diff::{DiffEntry, DiffStatus};
use serde::Serialize;

use crate::context::CommitSummary;
use crate::json_filter::apply_json_filter;
use crate::ui::entry_path;

/// How a command's outcome should be rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    /// Human-readable text (the default).
    Human,
    /// A single machine-readable JSON object on stdout.
    Json,
}

/// Rendering options for a command outcome.
#[derive(Clone, Debug)]
pub struct OutputOptions {
    /// Human text or full/filtered JSON on stdout.
    pub mode: OutputMode,
    /// Optional jq-like expression applied to JSON output (requires [`OutputMode::Json`]).
    pub filter: Option<String>,
}

impl OutputOptions {
    /// Reject `--filter` without `--json`.
    pub fn validate(&self) -> Result<()> {
        if self.filter.is_some() && self.mode != OutputMode::Json {
            bail!("--filter requires --json");
        }
        Ok(())
    }
}

/// Render a value as the command's human-readable output.
///
/// Implementations print to stdout directly (via `println!` and the `ui`
/// helpers), so the existing text output is reproduced byte-for-byte.
pub trait HumanRender {
    fn render_human(&self);
}

/// Render a command outcome to stdout in the chosen mode.
///
/// Generic (rather than `Box<dyn …>`) because `serde::Serialize` is not
/// object-safe; each dispatch arm calls this with its concrete outcome type.
pub fn emit<T: Serialize + HumanRender>(value: &T, opts: &OutputOptions) -> Result<()> {
    opts.validate()?;
    match opts.mode {
        OutputMode::Human => value.render_human(),
        OutputMode::Json => write_json(value, opts.filter.as_deref())?,
    }
    Ok(())
}

/// Serialize `value` to stdout, optionally applying a jq-like `filter`.
fn write_json<T: Serialize>(value: &T, filter: Option<&str>) -> Result<()> {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    if let Some(expr) = filter {
        let full = serde_json::to_value(value)
            .map_err(|e| anyhow::anyhow!("serializing JSON output: {e}"))?;
        let filtered = apply_json_filter(&full, expr)?;
        serde_json::to_writer_pretty(&mut lock, &filtered)
            .map_err(|e| anyhow::anyhow!("serializing filtered JSON output: {e}"))?;
    } else {
        serde_json::to_writer_pretty(&mut lock, value)
            .map_err(|e| anyhow::anyhow!("serializing JSON output: {e}"))?;
    }
    let _ = writeln!(lock);
    Ok(())
}

/// Report a command failure: `{"error": "…"}` on stdout in JSON mode, or the
/// usual `error: …` line on stderr in human mode. The caller still exits 1.
pub fn emit_error(err: &anyhow::Error, opts: &OutputOptions) {
    if let Err(filter_err) = opts.validate() {
        eprintln!("error: {filter_err:#}");
        return;
    }
    match opts.mode {
        OutputMode::Human => eprintln!("error: {err:#}"),
        OutputMode::Json => {
            let payload = if let Some(expr) = opts.filter.as_deref() {
                let full = serde_json::json!({ "error": format!("{err:#}") });
                match apply_json_filter(&full, expr) {
                    Ok(filtered) => filtered,
                    Err(filter_err) => {
                        serde_json::json!({ "error": format!("{filter_err:#}") })
                    }
                }
            } else {
                serde_json::json!({ "error": format!("{err:#}") })
            };
            println!("{payload}");
        }
    }
}

/// Emit a human-only progress line to **stderr**. Suppressed in JSON mode so
/// stdout stays a single clean object. Use for mid-operation status that isn't
/// part of the command's result (e.g. clone's "Cloning into …").
pub fn progress(mode: OutputMode, msg: &str) {
    if mode == OutputMode::Human {
        eprintln!("{msg}");
    }
}

// ---------------------------------------------------------------------------
// Shared JSON DTOs — small, stable serializations of grit-lib data. Defined
// here (not in grit-lib) so the JSON schema stays decoupled from internal types.
// ---------------------------------------------------------------------------

/// A commit in JSON output: full hex `oid` and its `subject` line.
#[derive(Serialize)]
pub struct CommitJson {
    pub oid: String,
    pub subject: String,
}

impl CommitJson {
    /// Build from a [`CommitSummary`] (status/shortlog ahead-lists).
    pub fn from_summary(commit: &CommitSummary) -> Self {
        Self {
            oid: commit.oid.to_hex(),
            subject: commit.subject.clone(),
        }
    }
}

/// A single worktree/index change in JSON output.
#[derive(Serialize)]
pub struct ChangeJson {
    pub path: String,
    pub status: String,
}

/// Stable machine-readable name for a diff status (snake_case).
pub fn change_status_str(status: &DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "added",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::TypeChanged => "type_changed",
        DiffStatus::Renamed => "renamed",
        DiffStatus::Copied => "copied",
        DiffStatus::Unmerged => "unmerged",
    }
}

/// Serialize a [`DiffEntry`] as a [`ChangeJson`].
pub fn change_json(entry: &DiffEntry) -> ChangeJson {
    ChangeJson {
        path: entry_path(entry).into_owned(),
        status: change_status_str(&entry.status).to_owned(),
    }
}
