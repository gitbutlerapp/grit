//! `grit cat` — show object contents (blob, tree, commit, or tag).

use std::io::Write;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use grit_lib::object_inspect::{format_tree_cat_file_p, tree_entry_mode_cat_file_p, ResolveError};
use grit_lib::objects::{Object, ObjectKind};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

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
    /// Raw bytes written to stdout in human mode (omitted from JSON).
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
        outln!(w, "- **spec:** `{}`", self.spec);
        outln!(w, "- **oid:** `{}`", self.oid);
        outln!(w, "- **kind:** {}", self.kind);
        outln!(w, "- **size:** {}", self.size);
        outln!(w);
        match &self.content {
            CatContentJson::Blob(b) => {
                if let Some(text) = &b.text {
                    outln!(w, "```text");
                    outln!(w, "{text}");
                    outln!(w, "```");
                } else {
                    outln!(w, "_binary blob (base64 in JSON)_");
                }
            }
            CatContentJson::Tree { entries } => {
                outln!(w, "| Mode | Type | OID | Path |");
                outln!(w, "| --- | --- | --- | --- |");
                for e in entries {
                    outln!(
                        w,
                        "| {} | {} | `{}` | `{}` |",
                        e.mode,
                        e.object_type,
                        e.oid,
                        e.path.replace('|', "\\|")
                    );
                }
            }
            CatContentJson::Commit(t) | CatContentJson::Tag(t) => {
                if let Some(text) = &t.text {
                    outln!(w, "```text");
                    outln!(w, "{}", text.trim_end());
                    outln!(w, "```");
                } else {
                    outln!(w, "_non-UTF-8 object payload (base64 in JSON)_");
                }
            }
        }
        Ok(())
    }
}

pub fn run(spec: String) -> Result<CatOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let resolved = repo
        .resolve(&spec)
        .map_err(|e| map_resolve_error(&spec, e))?;
    let obj = repo
        .read_object_raw_for_display(&resolved.oid)
        .with_context(|| format!("read object {}", resolved.oid.to_hex()))?;
    let (content, human_raw) = build_content(&repo, &obj)?;
    Ok(CatOutcome {
        warnings: sink.warnings(),
        spec,
        oid: resolved.oid.to_hex(),
        kind: kind_label(obj.kind).to_owned(),
        size: resolved.size,
        content,
        human_raw: Some(human_raw),
    })
}

fn build_content(
    repo: &grit_lib::repo::Repository,
    obj: &Object,
) -> Result<(CatContentJson, Vec<u8>)> {
    match obj.kind {
        ObjectKind::Blob => {
            let json = blob_payload_json(&obj.data);
            Ok((CatContentJson::Blob(json), obj.data.clone()))
        }
        ObjectKind::Tree => {
            let human = format_tree_cat_file_p(repo, &obj.data)?;
            let parsed = grit_lib::objects::parse_tree(&obj.data)?;
            let entries = parsed
                .iter()
                .map(|e| TreeLineJson {
                    mode: tree_entry_mode_cat_file_p(e.mode),
                    object_type: grit_lib::object_inspect::tree_entry_cat_file_type(
                        repo, e.mode, &e.oid,
                    )
                    .to_owned(),
                    oid: e.oid.to_hex(),
                    path: String::from_utf8_lossy(&e.name).into_owned(),
                })
                .collect();
            Ok((CatContentJson::Tree { entries }, human))
        }
        ObjectKind::Commit => Ok((
            CatContentJson::Commit(text_object_json(&obj.data)),
            obj.data.clone(),
        )),
        ObjectKind::Tag => Ok((
            CatContentJson::Tag(text_object_json(&obj.data)),
            obj.data.clone(),
        )),
    }
}

fn text_object_json(data: &[u8]) -> TextObjectJson {
    let b = blob_payload_json(data);
    TextObjectJson {
        encoding: b.encoding,
        text: b.text,
        data_base64: b.data_base64,
    }
}

fn blob_payload_json(data: &[u8]) -> BlobJson {
    if data.is_empty() || std::str::from_utf8(data).is_ok() {
        BlobJson {
            encoding: "utf-8".to_owned(),
            text: Some(String::from_utf8_lossy(data).into_owned()),
            data_base64: None,
        }
    } else {
        BlobJson {
            encoding: "base64".to_owned(),
            text: None,
            data_base64: Some(STANDARD.encode(data)),
        }
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
        ResolveError::PathNotInTree { path, revision } => {
            anyhow::anyhow!("path '{path}' does not exist in '{revision}'")
        }
        ResolveError::UnknownRevision { .. } => {
            anyhow::anyhow!("unknown revision '{spec}'")
        }
        _ => anyhow::anyhow!("could not resolve '{spec}'"),
    }
}
