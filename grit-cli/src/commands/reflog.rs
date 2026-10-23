//! `grit reflog` — inspect and maintain reference logs.

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::refs::reflog::reflog_identity_timestamp;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{ExpireOptions, IndexedReflogEntry};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, Person, RenderContext};

/// Result of `grit reflog` (list/show).
#[derive(Serialize, JsonSchema)]
pub struct ReflogListOutcome {
    pub r#ref: String,
    pub entries: Vec<ReflogListEntry>,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogListEntry {
    pub index: usize,
    /// Hex `new_oid` (unchanged v1 field).
    pub oid: String,
    pub old_oid: String,
    pub new_oid: String,
    /// RFC 3339 timestamp with the recorded zone offset.
    pub date: String,
    pub relative_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub committer: Option<Person>,
    pub message: String,
    /// Abbreviated `new_oid` for human/markdown lines (not in JSON).
    #[serde(skip)]
    pub abbreviated_oid: String,
}

impl HumanRender for ReflogListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.entries {
            let line = format_entry_line(&self.r#ref, entry, false);
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
            let line = format_entry_line(&self.r#ref, entry, true);
            outln!(w, "- {line}");
        }
        Ok(())
    }
}

fn format_entry_line(ref_name: &str, entry: &ReflogListEntry, absolute_date: bool) -> String {
    let when = if absolute_date {
        entry.date.as_str()
    } else {
        entry.relative_date.as_str()
    };
    format!(
        "{ref_name}@{{{}}} {} {} {}",
        entry.index, entry.abbreviated_oid, when, entry.message
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

/// Result of `grit reflog expire`.
#[derive(Serialize, JsonSchema)]
pub struct ReflogExpireOutcome {
    pub entries_pruned: usize,
}

impl HumanRender for ReflogExpireOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(
            w,
            "Pruned {} reflog {}",
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
        outln!(
            w,
            "Pruned **{}** reflog {}.",
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
        .map(|e| indexed_to_json(&repo, &e, now))
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

pub fn run_expire(
    all: bool,
    expire: Option<String>,
    expire_unreachable: Option<String>,
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
        })
        .context("reflog expire failed")?;
    Ok(ReflogExpireOutcome {
        entries_pruned: outcome.entries_pruned,
    })
}

fn indexed_to_json(repo: &Repository, entry: &IndexedReflogEntry, now: i64) -> ReflogListEntry {
    let ts = reflog_identity_timestamp(&entry.entry.identity).unwrap_or(0);
    let signature = entry.entry.signature();
    let date = signature
        .as_ref()
        .and_then(Signature::format_rfc3339)
        .unwrap_or_default();
    let committer = signature.as_ref().map(person_from_signature);
    let new_hex = entry.entry.new_oid.to_hex();
    let abbreviated_oid =
        grit_lib::object_inspect::abbreviate_for_display(repo, entry.entry.new_oid, true)
            .unwrap_or_else(|_| new_hex.clone());
    ReflogListEntry {
        index: entry.index,
        oid: new_hex.clone(),
        old_oid: entry.entry.old_oid.to_hex(),
        new_oid: new_hex,
        date,
        relative_date: context::relative_date_from(ts, now),
        committer,
        message: entry.entry.message.clone(),
        abbreviated_oid,
    }
}

fn person_from_signature(sig: &Signature) -> Person {
    Person {
        name: sig.name.clone(),
        email: sig.email.clone(),
        date: sig.format_rfc3339().unwrap_or_default(),
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
