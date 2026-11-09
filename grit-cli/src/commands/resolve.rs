//! `grit resolve` — resolve revision specs to object ids and types.

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::revwalk::object_inspect::ResolvedObject;
use grit_lib::revwalk::rev_parse_error::RevParseError;
use grit_lib::revwalk::AbbrevLength;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::markdown::{escape_table_cell, format_inline_code};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::revision_errors::map_resolve_error;

/// Ref-name output mode for `grit resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefNameOutput {
    ObjectIds,
    AbbrevRef,
    SymbolicFullName,
}

/// One resolved spec in default oid mode (`--json` without ref-name flags).
#[derive(Serialize, JsonSchema)]
#[schemars(
    description = "One resolved spec in default oid mode (`--json` without ref-name flags)."
)]
pub struct ResolveEntryObjectId {
    /// Input spec.
    pub spec: String,
    /// Resolved object id (abbreviated when `--short` is set).
    pub oid: String,
    /// `blob`, `tree`, `commit`, or `tag`.
    pub kind: String,
    /// Uncompressed object size in bytes.
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    /// Short ref name when the spec named a ref.
    pub ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    /// Full symbolic ref when the spec named a ref.
    pub full_ref: Option<String>,
}

/// One resolved spec in `--abbrev-ref` / `--symbolic-full-name` mode.
#[derive(Serialize, JsonSchema)]
#[schemars(description = "One resolved spec in `--abbrev-ref` / `--symbolic-full-name` mode.")]
pub struct ResolveEntryRefName {
    /// Input spec.
    pub spec: String,
    /// Shortest unambiguous ref name.
    pub ref_name: String,
    /// Full symbolic ref name.
    pub full_ref: String,
    /// Line printed for this flag (`ref_name` or `full_ref`).
    pub ref_display: String,
}

/// One resolved spec (`--json` row).
#[derive(Serialize)]
pub struct ResolveEntry {
    pub spec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_display: Option<String>,
}

impl JsonSchema for ResolveEntry {
    fn schema_name() -> String {
        "ResolveEntry".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::{SchemaObject, SubschemaValidation};
        SchemaObject {
            subschemas: Some(Box::new(SubschemaValidation {
                one_of: Some(vec![
                    gen.subschema_for::<ResolveEntryObjectId>(),
                    gen.subschema_for::<ResolveEntryRefName>(),
                ]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
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
        let spec_cell = format_inline_code(&escape_table_cell(&self.spec));
        outln!(w, "| {spec_cell} | `{}` |", self.oid);
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
                let spec_cell = format_inline_code(&escape_table_cell(&row.spec));
                outln!(w, "| {spec_cell} | {} |", escape_table_cell(ref_cell));
            }
        } else {
            outln!(w, "| Spec | OID | Kind | Size | Ref |");
            outln!(w, "| --- | --- | --- | ---: | --- |");
            for row in &self.results {
                let ref_cell = row.ref_name.as_deref().unwrap_or("");
                let spec_cell = format_inline_code(&escape_table_cell(&row.spec));
                outln!(
                    w,
                    "| {spec_cell} | `{}` | {} | {} | {} |",
                    row.oid.as_deref().unwrap_or(""),
                    row.kind.as_deref().unwrap_or(""),
                    row.size.map(|s| s.to_string()).unwrap_or_default(),
                    escape_table_cell(ref_cell)
                );
            }
        }
        Ok(())
    }
}

pub fn run_verify(spec: String, short: bool, quiet: bool) -> Result<VerifyResolveOutcome> {
    let repo = if quiet {
        context::discover()?
    } else {
        context::discover_with_warnings()?.0
    };
    let oid = repo
        .verify_object(&spec, None)
        .map_err(|e| map_resolve_error(&spec, e))?;
    let oid =
        display_oid(&repo, oid, short).with_context(|| format!("abbreviate oid for '{spec}'"))?;
    Ok(VerifyResolveOutcome { spec, oid })
}

fn display_oid(repo: &Repository, oid: ObjectId, short: bool) -> grit_lib::error::Result<String> {
    if short {
        repo.revwalk().abbreviate(oid, AbbrevLength::Configured)
    } else {
        Ok(oid.to_hex())
    }
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
                    Err(e) => {
                        return Err(map_rev_parse_ref_error(&spec, e));
                    }
                }
            }
            RefNameOutput::ObjectIds => {
                let resolved = repo
                    .resolve(&spec)
                    .map_err(|e| map_resolve_error(&spec, e))?;
                let oid = display_oid(&repo, resolved.oid, short)
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
    crate::error_map::map_rev_parse_ref_error(spec, err)
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// Dispatch parsed CLI args for `Resolve`.
pub fn dispatch(
    args: crate::args::ResolveArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let ResolveArgs {
        specs,
        short,
        abbrev_ref,
        symbolic_full_name,
        verify_spec,
        quiet,
    } = args;
    {
        use commands::resolve::RefNameOutput;
        if let Some(spec) = verify_spec {
            let outcome = commands::resolve::run_verify(spec, short, quiet);
            let code = commands::resolve::verify_exit_code(&outcome);
            if quiet {
                std::process::exit(code);
            }
            emit_success(outcome?, opts, ctx)
        } else {
            if quiet {
                anyhow::bail!("--quiet requires --verify");
            }
            let ref_output = if abbrev_ref {
                RefNameOutput::AbbrevRef
            } else if symbolic_full_name {
                RefNameOutput::SymbolicFullName
            } else {
                RefNameOutput::ObjectIds
            };
            emit_success(commands::resolve::run(specs, short, ref_output)?, opts, ctx)
        }
    }
}
