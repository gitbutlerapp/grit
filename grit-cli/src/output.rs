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
use std::io::{ErrorKind, Write as _};

use crate::cli_messages::human_error_message;
use crate::context::CommitSummary;
use crate::json_error::structured_json_payload;
use crate::json_filter::apply_json_filter;
use crate::stdio;
use crate::ui::entry_path;

/// How a command's outcome should be rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    /// Human-readable text (the default).
    Human,
    /// A single machine-readable JSON object on stdout.
    Json,
    /// Agent-friendly Markdown on stdout.
    Markdown,
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
    /// Reject incompatible output flag combinations.
    pub fn validate(&self) -> Result<()> {
        if self.filter.is_some() && self.mode != OutputMode::Json {
            bail!("--filter requires --json");
        }
        if self.mode == OutputMode::Markdown && self.filter.is_some() {
            bail!("--filter cannot be used with --markdown");
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

/// Render a command outcome as agent-friendly Markdown on stdout.
///
/// The default implementation lists top-level JSON fields; commands override this
/// when they have structured human-oriented Markdown (log, diff, status, …).
pub trait MarkdownRender: Serialize {
    fn render_markdown(&self)
    where
        Self: Sized,
    {
        render_value_markdown(self);
    }
}

/// Render a command outcome to stdout in the chosen mode.
///
/// Generic (rather than `Box<dyn …>`) because `serde::Serialize` is not
/// object-safe; each dispatch arm calls this with its concrete outcome type.
pub fn emit<T: Serialize + HumanRender + MarkdownRender>(
    value: &T,
    opts: &OutputOptions,
) -> Result<()> {
    opts.validate()?;
    match opts.mode {
        OutputMode::Human => value.render_human(),
        OutputMode::Json => write_json(value, opts.filter.as_deref())?,
        OutputMode::Markdown => value.render_markdown(),
    }
    Ok(())
}

/// Like [`emit`], but uses [`MarkdownRender`] for [`OutputMode::Markdown`].
pub fn emit_with_markdown<T: Serialize + HumanRender + MarkdownRender>(
    value: &T,
    opts: &OutputOptions,
) -> Result<()> {
    opts.validate()?;
    match opts.mode {
        OutputMode::Human => value.render_human(),
        OutputMode::Json => write_json(value, opts.filter.as_deref())?,
        OutputMode::Markdown => value.render_markdown(),
    }
    Ok(())
}

/// Serialize `value` to stdout, optionally applying a jq-like `filter`.
fn write_json<T: Serialize>(value: &T, filter: Option<&str>) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    if let Some(expr) = filter {
        let full = serde_json::to_value(value)
            .map_err(|e| anyhow::anyhow!("serializing JSON output: {e}"))?;
        let filtered = apply_json_filter(&full, expr)?;
        json_write(serde_json::to_writer_pretty(&mut lock, &filtered))?;
    } else {
        json_write(serde_json::to_writer_pretty(&mut lock, value))?;
    }
    let _ = stdio::io_result(writeln!(lock));
    Ok(())
}

fn json_write(result: Result<(), serde_json::Error>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.io_error_kind() == Some(ErrorKind::BrokenPipe) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("serializing JSON output: {e}")),
    }
}

/// Report a command failure: `{"error": "…"}` on stdout in JSON mode, or the
/// usual `error: …` line on stderr in human mode. The caller still exits 1.
pub fn emit_error(err: &anyhow::Error, opts: &OutputOptions) {
    if let Err(filter_err) = opts.validate() {
        eprintln!("error: {filter_err:#}");
        return;
    }
    let human = human_error_message(err);
    match opts.mode {
        OutputMode::Human => {
            if human.starts_with("fatal:") || human.starts_with("error:") {
                eprintln!("{human}");
            } else {
                eprintln!("error: {human}");
            }
        }
        OutputMode::Markdown => {
            println!("**Error:** {human}");
        }
        OutputMode::Json => {
            let payload = if let Some(structured) = structured_json_payload(err) {
                serde_json::to_value(structured)
                    .unwrap_or_else(|_| serde_json::json!({ "error": human.clone() }))
            } else if let Some(expr) = opts.filter.as_deref() {
                let full = serde_json::json!({ "error": human.clone() });
                match apply_json_filter(&full, expr) {
                    Ok(filtered) => filtered,
                    Err(filter_err) => {
                        serde_json::json!({ "error": format!("{filter_err:#}") })
                    }
                }
            } else {
                serde_json::json!({ "error": human })
            };
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = stdio::io_result(writeln!(lock, "{payload}"));
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

/// Default Markdown rendering: bullet list of top-level JSON fields.
pub fn render_value_markdown<T: Serialize>(value: &T) {
    let Ok(serde_json::Value::Object(map)) = serde_json::to_value(value) else {
        println!("(could not render outcome as Markdown)");
        return;
    };
    for (key, val) in map {
        println!("- **{key}**: {}", markdown_scalar(&val));
    }
}

fn markdown_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Shared JSON DTOs — small, stable serializations of grit-lib data. Defined
// here (not in grit-lib) so the JSON schema stays decoupled from internal types.
// ---------------------------------------------------------------------------

/// A commit in JSON output: full hex `oid`, subject, author, and dates.
#[derive(Serialize)]
pub struct CommitJson {
    pub oid: String,
    pub subject: String,
    /// Author display name (email local-part when present, matching human log).
    pub author: String,
    /// Author timestamp in RFC 3339.
    pub author_date: String,
    /// Relative author date (e.g. `3 days ago`), when `now` is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_date: Option<String>,
}

impl CommitJson {
    /// Build from a [`CommitSummary`] (status/shortlog ahead-lists).
    pub fn from_summary(commit: &CommitSummary, now: i64) -> Self {
        Self {
            oid: commit.oid.to_hex(),
            subject: commit.subject.clone(),
            author: commit.author.clone(),
            author_date: commit.author_date.clone(),
            relative_date: Some(crate::context::relative_date_from(commit.timestamp, now)),
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
        path: entry_path(entry).to_owned(),
        status: change_status_str(&entry.status).to_owned(),
    }
}
