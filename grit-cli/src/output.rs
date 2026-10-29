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
use grit_lib::command_runner::hooks::HookError;
use grit_lib::diff::{DiffEntry, DiffStatus};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{ErrorKind, IsTerminal, Write};
use time::OffsetDateTime;

use crate::cli_messages::human_error_message;
use crate::json_error::structured_json_payload;
use crate::json_filter::apply_json_filter;
use crate::stdio;
use crate::ui::entry_path;
use grit_lib::porcelain::log::DecorationKind;

/// Wall-clock instant captured once per process invocation for relative dates.
#[derive(Clone, Copy, Debug)]
pub struct RenderContext {
    /// Current time with repository timezone semantics applied at capture.
    pub now: OffsetDateTime,
}

/// Write one line to `w`, treating a closed pipe as success.
#[macro_export]
macro_rules! outln {
    ($w:expr $(, $($rest:tt)*)?) => {{
        $crate::stdio::io_result(::std::writeln!($w $(, $($rest)*)*))
            .map_err(|e| ::anyhow::anyhow!("writing output: {e}"))?
    }};
}

pub(crate) use outln;

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
pub trait HumanRender {
    /// Write human-oriented text to `w`.
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()>;
}

/// Render a command outcome as agent-friendly Markdown on stdout.
pub trait MarkdownRender: Serialize {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()>;
}

/// Render a command outcome to stdout in the chosen mode.
///
/// Generic (rather than `Box<dyn …>`) because `serde::Serialize` is not
/// object-safe; each dispatch arm calls this with its concrete outcome type.
pub fn emit<T: Serialize + HumanRender + MarkdownRender>(
    value: &T,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<()> {
    opts.validate()?;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let render = |w: &mut dyn Write| -> Result<()> {
        match opts.mode {
            OutputMode::Human => value.render_human(w, ctx),
            OutputMode::Json => write_json(w, value, opts.filter.as_deref()),
            OutputMode::Markdown => value.render_markdown(w, ctx),
        }
    };
    // Rust's stdout flushes every line, so a large single-line JSON document, or a long list,
    // costs a write per kilobyte or per line. Buffer like C stdio does: always for the single
    // JSON value, and for text unless a terminal is watching it appear.
    if opts.mode != OutputMode::Json && lock.is_terminal() {
        return render(&mut lock);
    }
    let mut buffered = std::io::BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, lock);
    render(&mut buffered)?;
    stdio::io_result(buffered.flush()).map_err(|e| anyhow::anyhow!("writing output: {e}"))
}

/// Stdout buffer size for [`emit`] when output is not shown on a terminal.
const OUTPUT_BUFFER_BYTES: usize = 64 * 1024;

/// Render an outcome into an in-memory string (for snapshot tests).
#[cfg(test)]
pub fn render_to_string<T: Serialize + HumanRender + MarkdownRender>(
    value: &T,
    mode: OutputMode,
    ctx: &RenderContext,
) -> Result<String> {
    let mut buf = Vec::new();
    match mode {
        OutputMode::Human => value.render_human(&mut buf, ctx)?,
        OutputMode::Json => write_json(&mut buf, value, None)?,
        OutputMode::Markdown => value.render_markdown(&mut buf, ctx)?,
    }
    Ok(String::from_utf8(buf)?)
}

/// Serialize `value` to `w`, optionally applying a jq-like `filter`.
fn write_json<T: Serialize>(w: &mut dyn Write, value: &T, filter: Option<&str>) -> Result<()> {
    if let Some(expr) = filter {
        let full = serde_json::to_value(value)
            .map_err(|e| anyhow::anyhow!("serializing JSON output: {e}"))?;
        let filtered = apply_json_filter(&full, expr)?;
        json_write(serde_json::to_writer(&mut *w, &filtered))?;
    } else {
        json_write(serde_json::to_writer(&mut *w, value))?;
    }
    stdio::io_result(writeln!(w)).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
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
    let hook_output = crate::json_error::hook_error_from_chain(err).and_then(HookError::output);
    let hint = structured_json_payload(err).and_then(|payload| payload.hint);
    match opts.mode {
        OutputMode::Human => {
            if let Some(output) = hook_output {
                let mut stderr = std::io::stderr().lock();
                let _ = stdio::io_result(stderr.write_all(&output.stdout));
                let _ = stdio::io_result(stderr.write_all(&output.stderr));
            }
            if human.starts_with("fatal:") || human.starts_with("error:") {
                eprintln!("{human}");
            } else {
                eprintln!("error: {human}");
            }
            if let Some(hint) = &hint {
                eprintln!("hint: {hint}");
            }
        }
        OutputMode::Markdown => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = stdio::io_result(writeln!(lock, "**Error:** {human}"));
            if let Some(hint) = &hint {
                let _ = stdio::io_result(writeln!(lock, "\n**Hint:** {hint}"));
            }
            if let Some(output) = hook_output {
                let _ = crate::markdown::render_hook_output(
                    &mut lock,
                    &String::from_utf8_lossy(&output.stdout),
                    &String::from_utf8_lossy(&output.stderr),
                );
            }
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

// ---------------------------------------------------------------------------
// Shared JSON DTOs — small, stable serializations of grit-lib data. Defined
// here (not in grit-lib) so the JSON schema stays decoupled from internal types.
// ---------------------------------------------------------------------------

/// A parsed identity line (`Name <email> <epoch> <tz>`) for stable `--json` output.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
    /// Author or committer timestamp in RFC 3339.
    pub date: String,
}

impl Person {
    /// Parse a raw `Name <email> <epoch> <tz>` identity line.
    pub fn from_identity_line(ident: &str) -> Self {
        let parsed = crate::context::parse_identity_line(ident);
        Self {
            name: parsed.name,
            email: parsed.email,
            date: crate::dates::rfc3339_from_identity_when(&parsed.when_suffix),
        }
    }

    /// From a library [`grit_lib::ident::Signature`].
    pub fn from_signature(sig: &grit_lib::ident::Signature) -> Self {
        Self {
            name: sig.name.clone(),
            email: sig.email.clone(),
            date: sig
                .format_rfc3339()
                .unwrap_or_else(|| sig.time.unix_timestamp().to_string()),
        }
    }
}

/// Ref decoration attached to a commit in JSON output.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CommitRefJson {
    pub name: String,
    pub kind: CommitRefKind,
}

/// Decoration category in JSON (`--json` contract).
#[derive(Clone, Copy, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitRefKind {
    Branch,
    RemoteBranch,
    Tag,
    Stash,
    Head,
    Grafted,
    Other,
}

impl From<DecorationKind> for CommitRefKind {
    fn from(kind: DecorationKind) -> Self {
        match kind {
            DecorationKind::Branch => Self::Branch,
            DecorationKind::RemoteBranch => Self::RemoteBranch,
            DecorationKind::Tag => Self::Tag,
            DecorationKind::Stash => Self::Stash,
            DecorationKind::Head => Self::Head,
            DecorationKind::Grafted => Self::Grafted,
            DecorationKind::Other => Self::Other,
        }
    }
}

/// Shared commit object in JSON output (`log`, `show`, `revs`, `shortlog`, …).
#[derive(Serialize, JsonSchema)]
pub struct CommitJson {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: Person,
    pub committer: Person,
    pub subject: String,
    /// Full message body with trailing whitespace trimmed.
    pub message: String,
    /// Ref decorations (branches, tags, `HEAD`, …) for this commit.
    pub refs: Vec<CommitRefJson>,
    /// Relative author date (e.g. `3 days ago`), when `now` is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub relative_date: Option<String>,
    /// Stable change id (reverse-hex; stored header or derived from commit id).
    pub change_id: String,
    /// `true` when `change_id` came from a `change-id` commit header.
    pub change_id_stored: bool,
}

/// A single worktree/index change in JSON output.
#[derive(Serialize, JsonSchema)]
pub struct ChangeJson {
    pub path: String,
    pub status: ChangeStatus,
    /// Previous path for renames and copies (`null` when not applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// Machine-readable diff change status (`--json` / `--markdown` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Deleted,
    Modified,
    TypeChanged,
    Renamed,
    Copied,
    Unmerged,
}

impl From<DiffStatus> for ChangeStatus {
    fn from(status: DiffStatus) -> Self {
        match status {
            DiffStatus::Added => Self::Added,
            DiffStatus::Deleted => Self::Deleted,
            DiffStatus::Modified => Self::Modified,
            DiffStatus::TypeChanged => Self::TypeChanged,
            DiffStatus::Renamed => Self::Renamed,
            DiffStatus::Copied => Self::Copied,
            DiffStatus::Unmerged => Self::Unmerged,
        }
    }
}

impl From<ChangeStatus> for DiffStatus {
    fn from(status: ChangeStatus) -> Self {
        match status {
            ChangeStatus::Added => Self::Added,
            ChangeStatus::Deleted => Self::Deleted,
            ChangeStatus::Modified => Self::Modified,
            ChangeStatus::TypeChanged => Self::TypeChanged,
            ChangeStatus::Renamed => Self::Renamed,
            ChangeStatus::Copied => Self::Copied,
            ChangeStatus::Unmerged => Self::Unmerged,
        }
    }
}

impl From<&DiffStatus> for ChangeStatus {
    fn from(status: &DiffStatus) -> Self {
        (*status).into()
    }
}

/// Serialize a [`DiffEntry`] as a [`ChangeJson`].
pub fn change_json(entry: &DiffEntry) -> ChangeJson {
    let old_path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => entry.old_path.clone(),
        _ => None,
    };
    ChangeJson {
        path: entry_path(entry).to_owned(),
        status: entry.status.into(),
        old_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::io::Write;

    #[derive(Serialize)]
    struct SampleOutcome {
        branch: &'static str,
        clean: bool,
    }

    impl HumanRender for SampleOutcome {
        fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
            outln!(w, "branch={} clean={}", self.branch, self.clean);
            Ok(())
        }
    }

    impl MarkdownRender for SampleOutcome {
        fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
            outln!(w, "- **branch**: `{}`", self.branch);
            outln!(w, "- **clean**: {}", self.clean);
            Ok(())
        }
    }

    fn sample_ctx() -> RenderContext {
        RenderContext {
            now: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
        }
    }

    #[test]
    fn render_to_string_human_json_markdown() {
        let value = SampleOutcome {
            branch: "main",
            clean: true,
        };
        let ctx = sample_ctx();
        let human = render_to_string(&value, OutputMode::Human, &ctx).expect("human");
        assert_eq!(human, "branch=main clean=true\n");
        let json = render_to_string(&value, OutputMode::Json, &ctx).expect("json");
        assert!(json.contains("\"branch\":\"main\""));
        let md = render_to_string(&value, OutputMode::Markdown, &ctx).expect("markdown");
        assert!(md.contains("**branch**"));
        assert!(md.contains("**clean**"));
    }

    #[test]
    fn filter_still_requires_json() {
        let opts = OutputOptions {
            mode: OutputMode::Human,
            filter: Some(".branch".to_owned()),
        };
        let err = opts.validate().expect_err("filter without json");
        assert!(err.to_string().contains("--filter requires --json"));
    }
}
