//! `grit resolve` — resolve revision specs to object ids and types.

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::object_inspect::{abbreviate_for_display, ResolveError, ResolvedObject};
use grit_lib::objects::ObjectKind;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// One resolved spec (`--json` row).
#[derive(Serialize, JsonSchema)]
pub struct ResolveEntry {
    pub spec: String,
    pub oid: String,
    pub kind: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
}

/// Result of `grit resolve`.
#[derive(Serialize, JsonSchema)]
pub struct ResolveOutcome {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,
    pub results: Vec<ResolveEntry>,
}

impl HumanRender for ResolveOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for row in &self.results {
            outln!(w, "{} {}", row.oid, row.kind);
        }
        Ok(())
    }
}

impl MarkdownRender for ResolveOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Resolve\n");
        if self.results.is_empty() {
            outln!(w, "No specs resolved.");
            return Ok(());
        }
        outln!(w, "| Spec | OID | Kind | Size | Ref |");
        outln!(w, "| --- | --- | --- | ---: | --- |");
        for row in &self.results {
            let ref_cell = row.ref_name.as_deref().unwrap_or("");
            outln!(
                w,
                "| `{}` | `{}` | {} | {} | {} |",
                row.spec.replace('|', "\\|"),
                row.oid,
                row.kind,
                row.size,
                ref_cell.replace('|', "\\|")
            );
        }
        Ok(())
    }
}

pub fn run(specs: Vec<String>, short: bool) -> Result<ResolveOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        let resolved = repo
            .resolve(&spec)
            .map_err(|e| map_resolve_error(&spec, e))?;
        let oid = abbreviate_for_display(&repo, resolved.oid, short)
            .with_context(|| format!("abbreviate oid for '{spec}'"))?;
        results.push(entry_from_resolved(&spec, &resolved, oid));
    }
    Ok(ResolveOutcome {
        warnings: sink.warnings(),
        results,
    })
}

fn entry_from_resolved(spec: &str, resolved: &ResolvedObject, oid: String) -> ResolveEntry {
    ResolveEntry {
        spec: spec.to_owned(),
        oid,
        kind: kind_label(resolved.kind).to_owned(),
        size: resolved.size,
        ref_name: resolved.ref_name.clone(),
    }
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

fn map_resolve_error(spec: &str, err: ResolveError) -> anyhow::Error {
    match err {
        ResolveError::AmbiguousShortObject { candidates, .. } => {
            let mut msg = format!("ambiguous short object id '{spec}'");
            for hint in candidates {
                let kind = hint.kind.unwrap_or("object");
                msg.push_str(&format!("\n  {} {}", kind, hint.oid_hex));
            }
            anyhow::anyhow!(msg)
        }
        ResolveError::AmbiguousChangeId { candidates, .. } => {
            let mut msg = format!("ambiguous change id '{spec}'");
            for hint in candidates {
                let short_id = hint.change_id.chars().take(8).collect::<String>();
                msg.push_str(&format!(
                    "\n  {} {} {}",
                    hint.oid_hex, short_id, hint.subject
                ));
            }
            anyhow::anyhow!(msg)
        }
        ResolveError::PathNotInTree { path, revision } => {
            anyhow::anyhow!("path '{path}' does not exist in '{revision}'")
        }
        ResolveError::UnknownRevision { .. } => {
            anyhow::anyhow!("unknown revision '{spec}'")
        }
        _ => anyhow::anyhow!("could not resolve '{spec}'"),
    }
}
