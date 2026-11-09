//! `grit cat` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::list::{format_inline_code, print_fenced_text};
use crate::render::table::escape_table_cell;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// Blob payload in JSON (`--json` contract).
#[derive(Serialize, JsonSchema)]
pub struct BlobJson {
    pub encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// When the blob is too large to embed inline in JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub omitted_inline: Option<bool>,
}

/// One tree line in JSON (same fields as human `ls-tree` output).
#[derive(Serialize, JsonSchema)]
pub struct TreeLineJson {
    pub mode: String,
    #[serde(rename = "type")]
    pub object_type: String,
    pub oid: String,
    pub path: String,
}

/// Commit or tag body in JSON (raw object payload bytes).
#[derive(Serialize, JsonSchema)]
pub struct TextObjectJson {
    pub encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

/// Object payload shape in `--json` output.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatContentJson {
    Blob(BlobJson),
    Tree { entries: Vec<TreeLineJson> },
    Commit(TextObjectJson),
    Tag(TextObjectJson),
}

/// Result of `grit cat`.
#[derive(Serialize, JsonSchema)]
pub struct CatOutcome {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,
    pub spec: String,
    pub oid: String,
    pub kind: String,
    pub size: u64,
    pub content: CatContentJson,
    /// Raw bytes written to stdout in human mode (omitted from JSON). Empty when streamed.
    #[serde(skip)]
    #[schemars(skip)]
    pub human_raw: Option<Vec<u8>>,
}

impl HumanRender for CatOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(bytes) = &self.human_raw {
            w.write_all(bytes)
                .map_err(|e| anyhow::anyhow!("writing object: {e}"))?;
        }
        Ok(())
    }
}

impl MarkdownRender for CatOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Cat\n");
        outln!(w, "- **spec:** {}", format_inline_code(&self.spec));
        outln!(w, "- **oid:** `{}`", self.oid);
        outln!(w, "- **kind:** {}", self.kind);
        outln!(w, "- **size:** {}", self.size);
        outln!(w);
        match &self.content {
            CatContentJson::Blob(b) => {
                if b.omitted_inline == Some(true) {
                    outln!(
                        w,
                        "_large binary blob (streamed in human mode; omitted from JSON)_"
                    );
                } else if let Some(text) = &b.text {
                    print_fenced_text(w, text)?;
                } else {
                    outln!(w, "_binary blob (base64 in JSON)_");
                }
            }
            CatContentJson::Tree { entries } => {
                outln!(w, "| Mode | Type | OID | Path |");
                outln!(w, "| --- | --- | --- | --- |");
                for e in entries {
                    let path_cell = format_inline_code(&escape_table_cell(&e.path));
                    outln!(
                        w,
                        "| {} | {} | `{}` | {path_cell} |",
                        e.mode,
                        e.object_type,
                        e.oid,
                    );
                }
            }
            CatContentJson::Commit(t) | CatContentJson::Tag(t) => {
                if let Some(text) = &t.text {
                    print_fenced_text(w, text.trim_end())?;
                } else {
                    outln!(w, "_non-UTF-8 object payload (base64 in JSON)_");
                }
            }
        }
        Ok(())
    }
}
