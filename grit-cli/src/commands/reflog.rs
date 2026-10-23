//! `grit reflog` — inspect and maintain reference logs.

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::error::Error;
use grit_lib::refs::reflog::reflog_identity_timestamp;
use grit_lib::refs::store::RefName;
use grit_lib::repo_handles::{ExpireOptions, IndexedReflogEntry};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit reflog` (list/show).
#[derive(Serialize, JsonSchema)]
pub struct ReflogListOutcome {
    pub r#ref: String,
    pub entries: Vec<ReflogListEntry>,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogListEntry {
    pub index: usize,
    pub oid: String,
    pub relative_date: String,
    pub message: String,
}

impl HumanRender for ReflogListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.entries {
            let line = format_entry_line(&self.r#ref, entry);
            outln!(w, "{line}");
        }
        Ok(())
    }
}

impl MarkdownRender for ReflogListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.entries.is_empty() {
            outln!(w, "_No reflog entries for `{}`._", self.r#ref);
            return Ok(());
        }
        for entry in &self.entries {
            let line = format_entry_line(&self.r#ref, entry);
            outln!(w, "- {line}");
        }
        Ok(())
    }
}

fn format_entry_line(ref_name: &str, entry: &ReflogListEntry) -> String {
    format!(
        "{ref_name}@{{{}}} {} {} {}",
        entry.index, entry.oid, entry.relative_date, entry.message
    )
}

/// Result of `grit reflog delete`.
#[derive(Serialize, JsonSchema)]
pub struct ReflogDeleteOutcome {
    pub deleted: Vec<String>,
}

impl HumanRender for ReflogDeleteOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for spec in &self.deleted {
            outln!(w, "Deleted {spec}");
        }
        Ok(())
    }
}

impl MarkdownRender for ReflogDeleteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for spec in &self.deleted {
            outln!(w, "- Deleted `{spec}`");
        }
        Ok(())
    }
}

/// Result of `grit reflog list` (refs that currently have a log).
#[derive(Serialize, JsonSchema)]
pub struct ReflogRefsListOutcome {
    pub refs: Vec<ReflogRefRow>,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogRefRow {
    pub r#ref: String,
}

impl HumanRender for ReflogRefsListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for row in &self.refs {
            outln!(w, "{}", row.r#ref);
        }
        Ok(())
    }
}

impl MarkdownRender for ReflogRefsListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.refs.is_empty() {
            outln!(w, "_No reflogs._");
            return Ok(());
        }
        for row in &self.refs {
            outln!(w, "- `{}`", row.r#ref);
        }
        Ok(())
    }
}

/// Result of `grit reflog expire`.
#[derive(Serialize, JsonSchema)]
pub struct ReflogExpireOutcome {
    pub entries_pruned: usize,
    pub dry_run: bool,
    pub refs: Vec<ReflogExpireRefOutcome>,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogExpireRefOutcome {
    pub r#ref: String,
    pub pruned: usize,
}

impl HumanRender for ReflogExpireOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let verb = if self.dry_run {
            "Would prune"
        } else {
            "Pruned"
        };
        outln!(
            w,
            "{verb} {} reflog {}",
            self.entries_pruned,
            if self.entries_pruned == 1 {
                "entry"
            } else {
                "entries"
            }
        );
        Ok(())
    }
}

impl MarkdownRender for ReflogExpireOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let verb = if self.dry_run {
            "Would prune"
        } else {
            "Pruned"
        };
        outln!(
            w,
            "{verb} **{}** reflog {}.",
            self.entries_pruned,
            if self.entries_pruned == 1 {
                "entry"
            } else {
                "entries"
            }
        );
        Ok(())
    }
}

pub fn run_show(r#ref: Option<String>, limit: Option<usize>) -> Result<ReflogListOutcome> {
    let repo = context::discover()?;
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let spec = r#ref.as_deref().unwrap_or("HEAD");
    let ref_name = reflog_ref_name(spec)?;
    let mut entries: Vec<ReflogListEntry> = repo
        .reflogs()
        .entries(&ref_name)
        .map_err(map_reflog_error)?
        .into_iter()
        .map(|e| indexed_to_json(&e, now))
        .collect();
    if let Some(n) = limit {
        entries.truncate(n);
    }
    Ok(ReflogListOutcome {
        r#ref: spec.to_owned(),
        entries,
    })
}

pub fn run_delete(specs: Vec<String>) -> Result<ReflogDeleteOutcome> {
    let repo = context::discover()?;
    let mut deleted = Vec::new();
    for spec in specs {
        let (ref_part, index) = parse_reflog_selector(&spec)?;
        let ref_name = reflog_ref_name(&ref_part)?;
        repo.reflogs()
            .delete(&ref_name, &[index], false)
            .map_err(map_reflog_error)?;
        deleted.push(spec);
    }
    Ok(ReflogDeleteOutcome { deleted })
}

pub fn run_list_refs(limit: Option<usize>) -> Result<ReflogRefsListOutcome> {
    let repo = context::discover()?;
    let mut refs: Vec<ReflogRefRow> = repo
        .reflogs()
        .refs()
        .context("reflog list failed")?
        .into_iter()
        .map(|name| ReflogRefRow {
            r#ref: name.as_str().to_owned(),
        })
        .collect();
    if let Some(n) = limit {
        refs.truncate(n);
    }
    Ok(ReflogRefsListOutcome { refs })
}

pub fn run_expire(
    all: bool,
    expire: Option<String>,
    expire_unreachable: Option<String>,
    dry_run: bool,
) -> Result<ReflogExpireOutcome> {
    let repo = context::discover()?;
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let outcome = repo
        .reflogs()
        .expire(&ExpireOptions {
            now,
            expire,
            expire_unreachable,
            all,
            dry_run,
        })
        .context("reflog expire failed")?;
    Ok(ReflogExpireOutcome {
        entries_pruned: outcome.entries_pruned,
        dry_run: outcome.dry_run,
        refs: outcome
            .refs
            .into_iter()
            .map(|r| ReflogExpireRefOutcome {
                r#ref: r.ref_name,
                pruned: r.pruned,
            })
            .collect(),
    })
}

fn indexed_to_json(entry: &IndexedReflogEntry, now: i64) -> ReflogListEntry {
    let ts = reflog_identity_timestamp(&entry.entry.identity).unwrap_or(0);
    ReflogListEntry {
        index: entry.index,
        oid: entry.entry.new_oid.to_hex(),
        relative_date: context::relative_date_from(ts, now),
        message: entry.entry.message.clone(),
    }
}

/// Parse `ref@{n}` or bare `n` with a separate ref (not used here).
pub fn parse_reflog_selector(spec: &str) -> Result<(String, usize)> {
    let spec = spec.trim();
    let Some(at) = spec.find("@{") else {
        anyhow::bail!("expected ref@{{n}} form, got '{spec}'");
    };
    let ref_part = spec[..at].trim();
    let rest = spec[at + 2..]
        .strip_suffix('}')
        .ok_or_else(|| anyhow::anyhow!("expected ref@{{n}} form, got '{spec}'"))?;
    let index = rest
        .parse::<usize>()
        .with_context(|| format!("invalid reflog index in '{spec}'"))?;
    if ref_part.is_empty() {
        anyhow::bail!("expected ref@{{n}} form, got '{spec}'");
    }
    Ok((ref_part.to_owned(), index))
}

fn reflog_ref_name(spec: &str) -> Result<RefName, Error> {
    if let Ok(name) = RefName::try_from(spec) {
        return Ok(name);
    }
    if spec != "HEAD" && !spec.starts_with("refs/") {
        if let Ok(name) = RefName::try_from(&format!("refs/heads/{spec}")) {
            return Ok(name);
        }
    }
    Err(Error::ReflogUnknownRef {
        refname: spec.to_owned(),
    })
}

fn map_reflog_error(err: Error) -> anyhow::Error {
    match err {
        Error::ReflogUnknownRef { refname } => anyhow::anyhow!("unknown ref '{refname}'"),
        Error::ReflogIndexOutOfRange { refname, index } => {
            anyhow::anyhow!("reflog entry {refname}@{{{index}}} not found")
        }
        other => anyhow::Error::new(other),
    }
}
