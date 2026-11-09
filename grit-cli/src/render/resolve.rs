//! Outcome types and renderers for `grit resolve`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

use crate::markdown::{escape_table_cell, format_inline_code};

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
