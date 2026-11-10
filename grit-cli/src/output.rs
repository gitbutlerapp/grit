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

use crate::json_filter::write_json_filter_val;
use crate::json_to_jaq;
use crate::stdio;
use crate::ui::entry_path;
use anyhow::{bail, Result};
use grit_lib::diff::{DiffEntry, DiffStatus};
use grit_lib::porcelain::log::DecorationKind;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{ErrorKind, IsTerminal, Write};
use std::path::Path;
#[cfg(test)]
use time::OffsetDateTime;

pub use crate::render::{HumanRender, MarkdownRender, OutputMode, RenderContext};

/// Write one line to `w`, treating a closed pipe as success.
#[macro_export]
macro_rules! outln {
    ($w:expr $(, $($rest:tt)*)?) => {{
        $crate::stdio::io_result(::std::writeln!($w $(, $($rest)*)*))
            .map_err(|e| ::anyhow::anyhow!("writing output: {e}"))?
    }};
}

pub(crate) use outln;

/// Rendering options for a command outcome.
#[derive(Clone, Debug)]
pub struct OutputOptions {
    /// Human text or full/filtered JSON on stdout.
    pub mode: OutputMode,
    /// Optional jq-like expression applied to JSON output (requires [`OutputMode::Json`]).
    pub filter: Option<String>,
}

impl OutputOptions {
    /// Whether this invocation should emit human progress lines to stderr.
    #[must_use]
    pub fn show_progress(&self) -> bool {
        self.mode == OutputMode::Human
    }

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

/// Render a command outcome to stdout in the chosen mode.
///
/// Generic (rather than `Box<dyn …>`) because `serde::Serialize` is not
/// object-safe; each dispatch arm calls this with its concrete outcome type.
pub fn emit<T: Serialize + HumanRender + MarkdownRender>(
    value: T,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<()> {
    opts.validate()?;
    let stdout = std::io::stdout();
    let lock = stdout.lock();
    let mode = opts.mode;
    let filter = opts.filter.clone();
    let render = move |w: &mut StreamingStdout<'_>| -> Result<()> {
        match mode {
            OutputMode::Human => value.render_human(w, ctx),
            OutputMode::Json => write_json(w, value, filter.as_deref()),
            OutputMode::Markdown => value.render_markdown(w, ctx),
        }
    };
    // Rust's stdout flushes every line, so a large single-line JSON document, or a long list,
    // costs a write per kilobyte or per line. Buffer like C stdio does: always for the single
    // JSON value, and for text unless a terminal is watching it appear.
    if mode != OutputMode::Json && lock.is_terminal() {
        let mut out = StreamingStdout::Terminal(lock);
        return render(&mut out);
    }
    with_streaming_writer(lock, render)
}

/// Stdout buffer size for [`emit`] and streamed command bodies when output is not a terminal.
pub(crate) const OUTPUT_BUFFER_BYTES: usize = 64 * 1024;

/// Locked stdout, optionally wrapped in a 64 KiB buffer when output is not a terminal.
pub(crate) enum StreamingStdout<'a> {
    /// Line-buffered terminal output.
    Terminal(std::io::StdoutLock<'a>),
    /// Buffered output for pipes and redirects.
    Buffered(std::io::BufWriter<std::io::StdoutLock<'a>>),
}

impl Write for StreamingStdout<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Terminal(w) => w.write(buf),
            Self::Buffered(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(w) => w.flush(),
            Self::Buffered(w) => w.flush(),
        }
    }
}

/// Run `f` on a writer backed by locked stdout. When stdout is not a terminal, wrap it in a
/// 64 KiB buffer so redirected or piped output does not flush on every line.
pub(crate) fn with_streaming_stdout<R>(
    f: impl FnOnce(&mut StreamingStdout<'_>) -> Result<R>,
) -> Result<R> {
    let stdout = std::io::stdout();
    let lock = stdout.lock();
    with_streaming_writer(lock, f)
}

fn with_streaming_writer<R>(
    lock: std::io::StdoutLock<'_>,
    f: impl FnOnce(&mut StreamingStdout<'_>) -> Result<R>,
) -> Result<R> {
    if lock.is_terminal() {
        let mut out = StreamingStdout::Terminal(lock);
        return f(&mut out);
    }
    let mut out =
        StreamingStdout::Buffered(std::io::BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, lock));
    let result = f(&mut out)?;
    if let StreamingStdout::Buffered(w) = &mut out {
        stdio::io_result(w.flush()).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    }
    Ok(result)
}

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
        OutputMode::Json => {
            json_write(serde_json::to_writer(&mut buf, value))?;
            stdio::io_result(writeln!(&mut buf))
                .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
        }
        OutputMode::Markdown => value.render_markdown(&mut buf, ctx)?,
    }
    Ok(String::from_utf8(buf)?)
}

/// Serialize `value` to `w`, optionally applying a jq-like `filter`.
fn write_json<T: Serialize>(w: &mut dyn Write, value: T, filter: Option<&str>) -> Result<()> {
    if let Some(expr) = filter {
        // Build the jaq input tree in one pass from the typed outcome (interning
        // object keys). Do not serialize to JSON bytes and re-parse: that roughly
        // doubles work and peak RSS on large diff/log payloads (#2409).
        let input_val = json_to_jaq::serialize_value(value)
            .map_err(|e| anyhow::anyhow!("serializing JSON for filter: {e}"))?;
        write_json_filter_val(w, input_val, expr)?;
    } else {
        json_write(serde_json::to_writer(&mut *w, &value))?;
    }
    stdio::io_result(writeln!(w)).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    Ok(())
}

pub(crate) fn json_write(result: Result<(), serde_json::Error>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.io_error_kind() == Some(ErrorKind::BrokenPipe) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("serializing JSON output: {e}")),
    }
}

/// Report a command failure: `{"error": "…"}` on stdout in JSON mode, or the
/// usual `error: …` line on stderr in human mode. The caller still exits 1.
pub fn emit_error(err: &anyhow::Error, opts: &OutputOptions) {
    crate::render::error::emit(err, opts);
}

/// Whether human stdout should emit ANSI color for this process (TTY, ANSI, no `NO_COLOR`).
#[must_use]
pub fn stdout_color_enabled() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
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
            // `Other` and decoration kinds this CLI does not know yet.
            _ => Self::Other,
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

/// Submodule detail on a gitlink change in status/diff JSON output.
#[derive(Serialize, JsonSchema)]
pub struct SubmoduleChangeJson {
    pub new_commits: bool,
    pub modified: bool,
    pub untracked: bool,
}

/// A single worktree/index change in JSON output.
#[derive(Serialize, JsonSchema)]
pub struct ChangeJson {
    pub path: String,
    pub status: ChangeStatus,
    /// Previous path for renames and copies (`null` when not applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Present for gitlink worktree changes (`160000`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submodule: Option<SubmoduleChangeJson>,
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
            // Status letters this CLI does not know yet render as a modification.
            _ => Self::Modified,
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
#[allow(dead_code)] // thin wrapper kept for callers that do not have a work tree path
pub fn change_json(entry: &DiffEntry) -> ChangeJson {
    change_json_with_worktree(entry, None)
}

/// Like [`change_json`], with optional superproject work tree for gitlink submodule detail.
pub fn change_json_with_worktree(entry: &DiffEntry, work_tree: Option<&Path>) -> ChangeJson {
    let old_path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => entry.old_path.clone(),
        _ => None,
    };
    let submodule = work_tree.and_then(|wt| {
        if !entry.old_mode.is_gitlink() || entry.status != DiffStatus::Modified {
            return None;
        }
        let path = entry_path(entry);
        let flags = grit_lib::diff::submodule_porcelain_flags(wt, path, entry.old_oid);
        Some(SubmoduleChangeJson {
            new_commits: flags.new_commits,
            modified: flags.modified,
            untracked: flags.untracked,
        })
    });
    ChangeJson {
        path: entry_path(entry).to_owned(),
        status: entry.status.into(),
        old_path,
        submodule,
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
            mode: OutputMode::Human,
            color: false,
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
