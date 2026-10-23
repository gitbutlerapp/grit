//! `grit resolve` — resolve revision specs to object ids and types.

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::object_inspect::{abbreviate_for_display, ResolvedObject};
use grit_lib::objects::ObjectKind;
use grit_lib::revwalk::rev_parse_error::RevParseError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::revision_errors::map_resolve_error;

/// Ref-name output mode for `grit resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefNameOutput {
    ObjectIds,
    AbbrevRef,
    SymbolicFullName,
}

/// One resolved spec (`--json` row).
#[derive(Serialize, JsonSchema)]
pub struct ResolveEntry {
    pub spec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub full_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ref_display: Option<String>,
}

/// Result of `grit resolve --verify` (`--json` when not `--quiet`).
#[derive(Serialize, JsonSchema)]
pub struct VerifyResolveOutcome {
    pub spec: String,
    pub oid: String,
}

impl HumanRender for VerifyResolveOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "{}", self.oid);
        Ok(())
    }
}

impl MarkdownRender for VerifyResolveOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Resolve (verify)\n");
        outln!(w, "| Spec | OID |");
        outln!(w, "| --- | --- |");
        outln!(
            w,
            "| `{}` | `{}` |",
            self.spec.replace('|', "\\|"),
            self.oid
        );
        Ok(())
    }
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
            if let Some(display) = &row.ref_display {
                outln!(w, "{}", display);
            } else if let (Some(oid), Some(kind)) = (&row.oid, &row.kind) {
                outln!(w, "{} {}", oid, kind);
            }
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
        let ref_mode = self.results.iter().any(|r| r.ref_display.is_some());
        if ref_mode {
            outln!(w, "| Spec | Ref |");
            outln!(w, "| --- | --- |");
            for row in &self.results {
                let ref_cell = row.ref_display.as_deref().unwrap_or("");
                outln!(
                    w,
                    "| `{}` | {} |",
                    row.spec.replace('|', "\\|"),
                    ref_cell.replace('|', "\\|")
                );
            }
        } else {
            outln!(w, "| Spec | OID | Kind | Size | Ref |");
            outln!(w, "| --- | --- | --- | ---: | --- |");
            for row in &self.results {
                let ref_cell = row.ref_name.as_deref().unwrap_or("");
                outln!(
                    w,
                    "| `{}` | `{}` | {} | {} | {} |",
                    row.spec.replace('|', "\\|"),
                    row.oid.as_deref().unwrap_or(""),
                    row.kind.as_deref().unwrap_or(""),
                    row.size.map(|s| s.to_string()).unwrap_or_default(),
                    ref_cell.replace('|', "\\|")
                );
            }
        }
        Ok(())
    }
}

pub fn run_verify(spec: String, short: bool) -> Result<VerifyResolveOutcome> {
    let (repo, _sink) = context::discover_with_warnings()?;
    let oid = repo
        .verify_object(&spec, None)
        .map_err(|e| map_resolve_error(&spec, e))?;
    let oid = abbreviate_for_display(&repo, oid, short)
        .with_context(|| format!("abbreviate oid for '{spec}'"))?;
    Ok(VerifyResolveOutcome { spec, oid })
}

/// Exit code for `grit resolve --verify` (0 = resolved, 1 = not).
#[must_use]
pub fn verify_exit_code(result: &Result<VerifyResolveOutcome, anyhow::Error>) -> i32 {
    if result.is_ok() {
        0
    } else {
        1
    }
}

pub fn run(specs: Vec<String>, short: bool, ref_output: RefNameOutput) -> Result<ResolveOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        match ref_output {
            RefNameOutput::AbbrevRef | RefNameOutput::SymbolicFullName => {
                match repo.resolve_ref_name(&spec) {
                    Ok(Some(names)) => {
                        let display = if ref_output == RefNameOutput::AbbrevRef {
                            names.short.clone()
                        } else {
                            names.full.clone()
                        };
                        results.push(ResolveEntry {
                            spec: spec.clone(),
                            oid: None,
                            kind: None,
                            size: None,
                            ref_name: Some(names.short),
                            full_ref: Some(names.full),
                            ref_display: Some(display),
                        });
                    }
                    Ok(None) => {}
                    Err(RevParseError::AmbiguousRefname { spec: amb }) => {
                        eprintln!("warning: refname '{amb}' is ambiguous.");
                        eprintln!("error: refname '{amb}' is ambiguous");
                    }
                    Err(e) => {
                        return Err(map_rev_parse_ref_error(&spec, e));
                    }
                }
            }
            RefNameOutput::ObjectIds => {
                let resolved = repo
                    .resolve(&spec)
                    .map_err(|e| map_resolve_error(&spec, e))?;
                let oid = abbreviate_for_display(&repo, resolved.oid, short)
                    .with_context(|| format!("abbreviate oid for '{spec}'"))?;
                results.push(entry_from_resolved(&spec, &resolved, oid));
            }
        }
    }
    Ok(ResolveOutcome {
        warnings: sink.warnings(),
        results,
    })
}

fn entry_from_resolved(spec: &str, resolved: &ResolvedObject, oid: String) -> ResolveEntry {
    ResolveEntry {
        spec: spec.to_owned(),
        oid: Some(oid),
        kind: Some(kind_label(resolved.kind).to_owned()),
        size: Some(resolved.size),
        ref_name: resolved.ref_name.clone(),
        full_ref: resolved.full_ref.clone(),
        ref_display: None,
    }
}

fn map_rev_parse_ref_error(spec: &str, err: RevParseError) -> anyhow::Error {
    let lib = grit_lib::error::Error::RevParse(err);
    if let Some(msg) = crate::revision_errors::map_lib_revision_error(&lib) {
        anyhow::anyhow!(msg)
    } else {
        anyhow::anyhow!("could not resolve ref '{spec}': {lib}")
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
