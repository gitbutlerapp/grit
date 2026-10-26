//! `grit cat` — show object contents (blob, tree, commit, or tag).

use std::io::{self, Write};

use crate::revision_errors::map_resolve_error;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use grit_lib::object_inspect::{format_tree_cat_file_p, tree_entry_mode_cat_file_p};
use grit_lib::objects::{Object, ObjectKind};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};

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
        outln!(w, "- **spec:** `{}`", self.spec);
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

/// Maximum blob size whose bytes are embedded in `--json` output.
const JSON_INLINE_BLOB_MAX: u64 = 1024 * 1024;

pub fn run(spec: String, output: OutputMode) -> Result<CatOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let resolved = repo
        .resolve(&spec)
        .map_err(|e| map_resolve_error(&spec, e))?;
    let info = repo
        .odb()
        .read_info(&resolved.oid)
        .with_context(|| format!("read object {}", resolved.oid.to_hex()))?;

    match info.kind {
        ObjectKind::Blob => run_blob(
            &repo,
            spec,
            resolved.oid,
            resolved.size,
            output,
            sink.warnings(),
        ),
        kind => {
            let obj = repo
                .read_object_raw_for_display(&resolved.oid)
                .with_context(|| format!("read object {}", resolved.oid.to_hex()))?;
            let (content, human_raw) = build_non_blob_content(&repo, &obj)?;
            Ok(CatOutcome {
                warnings: sink.warnings(),
                spec,
                oid: resolved.oid.to_hex(),
                kind: kind_label(kind).to_owned(),
                size: resolved.size,
                content,
                human_raw: Some(human_raw),
            })
        }
    }
}

fn run_blob(
    repo: &grit_lib::repo::Repository,
    spec: String,
    oid: grit_lib::objects::ObjectId,
    size: u64,
    output: OutputMode,
    warnings: Vec<crate::diagnostics::WarningRecord>,
) -> Result<CatOutcome> {
    let large = size >= JSON_INLINE_BLOB_MAX;
    let stream_human = matches!(output, OutputMode::Human) && large;
    if stream_human {
        repo.stream_object_payload_to(&oid, &mut io::stdout())?
            .ok_or_else(|| anyhow::anyhow!("object not found: {}", oid.to_hex()))?;
    }

    let omitted_blob = || CatContentJson::Blob(BlobJson {
        encoding: "base64".to_owned(),
        text: None,
        data_base64: None,
        omitted_inline: Some(true),
    });

    let (content, human_raw) = if large && (stream_human || !matches!(output, OutputMode::Human)) {
        (omitted_blob(), None)
    } else {
        let data = repo
            .read_object_raw_for_display(&oid)
            .with_context(|| format!("read object {}", oid.to_hex()))?
            .data;
        let json = blob_payload_json(&data, size);
        let raw = matches!(output, OutputMode::Human).then(|| data);
        (CatContentJson::Blob(json), raw)
    };

    Ok(CatOutcome {
        warnings,
        spec,
        oid: oid.to_hex(),
        kind: "blob".to_owned(),
        size,
        content,
        human_raw,
    })
}

fn build_non_blob_content(
    repo: &grit_lib::repo::Repository,
    obj: &Object,
) -> Result<(CatContentJson, Vec<u8>)> {
    match obj.kind {
        ObjectKind::Blob => unreachable!("handled in run_blob"),
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
    let b = blob_payload_json(data, u64::try_from(data.len()).unwrap_or(u64::MAX));
    TextObjectJson {
        encoding: b.encoding,
        text: b.text,
        data_base64: b.data_base64,
    }
}

fn blob_payload_json(data: &[u8], size: u64) -> BlobJson {
    if size > JSON_INLINE_BLOB_MAX {
        return BlobJson {
            encoding: "base64".to_owned(),
            text: None,
            data_base64: None,
            omitted_inline: Some(true),
        };
    }
    if data.is_empty() || std::str::from_utf8(data).is_ok() {
        BlobJson {
            encoding: "utf-8".to_owned(),
            text: Some(String::from_utf8_lossy(data).into_owned()),
            data_base64: None,
            omitted_inline: None,
        }
    } else {
        BlobJson {
            encoding: "base64".to_owned(),
            text: None,
            data_base64: Some(STANDARD.encode(data)),
            omitted_inline: None,
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
