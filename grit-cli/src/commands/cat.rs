//! `grit cat` — show object contents (blob, tree, commit, or tag).

use std::io::{self, Write};

use crate::revision_errors::map_resolve_error;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use grit_lib::objects::{Object, ObjectKind};
use grit_lib::porcelain::cat::{
    CatBlobDisplayPlan, CatBlobDisposition, CatPresentationOptions, CatStdoutFormat,
    JSON_INLINE_BLOB_MAX,
};
use grit_lib::revwalk::object_inspect::{
    format_tree_entries_cat_file_p, tree_entry_cat_file_type, tree_entry_mode_cat_file_p,
};

use crate::context;
use crate::output::OutputMode;
pub use crate::render::cat::{BlobJson, CatContentJson, CatOutcome, TextObjectJson, TreeLineJson};

fn presentation_from_mode(mode: OutputMode) -> CatPresentationOptions {
    CatPresentationOptions {
        format: match mode {
            OutputMode::Human => CatStdoutFormat::Human,
            OutputMode::Json => CatStdoutFormat::Json,
            OutputMode::Markdown => CatStdoutFormat::Markdown,
        },
    }
}

pub fn run(spec: String, presentation: CatPresentationOptions) -> Result<CatOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let resolved = repo
        .resolve(&spec)
        .map_err(|e| map_resolve_error(&spec, e))?;

    match resolved.kind {
        ObjectKind::Blob => run_blob(
            &repo,
            spec,
            resolved.oid,
            resolved.size,
            presentation,
            sink.warnings(),
        ),
        kind => {
            let obj = repo
                .read_object_raw_for_display(&resolved.oid)
                .with_context(|| format!("read object {}", resolved.oid.to_hex()))?;
            let (content, human_raw) = build_content_for_kind(kind, &obj, presentation)?;
            Ok(CatOutcome {
                warnings: sink.warnings(),
                spec,
                oid: resolved.oid.to_hex(),
                kind: kind_label(kind).to_owned(),
                size: resolved.size,
                content,
                human_raw,
            })
        }
    }
}

fn run_blob(
    repo: &grit_lib::repo::Repository,
    spec: String,
    oid: grit_lib::objects::ObjectId,
    size: u64,
    presentation: CatPresentationOptions,
    warnings: Vec<crate::diagnostics::WarningRecord>,
) -> Result<CatOutcome> {
    let plan = CatBlobDisplayPlan::for_blob(size, presentation);
    if plan.streams_on_human_stdout() {
        repo.with_object_stream_for_display(&oid, |mut stream| {
            std::io::copy(&mut stream.reader, &mut io::stdout()).map_err(grit_lib::error::Error::Io)
        })?
        .ok_or_else(|| anyhow::anyhow!("object not found: {}", oid.to_hex()))?;
    }

    let omitted_blob = || {
        CatContentJson::Blob(BlobJson {
            encoding: "base64".to_owned(),
            text: None,
            data_base64: None,
            omitted_inline: Some(true),
        })
    };

    let (content, human_raw) = match plan.disposition {
        CatBlobDisposition::StreamHumanStdout => (omitted_blob(), None),
        CatBlobDisposition::OmitInline => (omitted_blob(), None),
        CatBlobDisposition::Inline => {
            let data = repo
                .read_object_raw_for_display(&oid)
                .with_context(|| format!("read object {}", oid.to_hex()))?
                .data;
            let json = blob_payload_json(&data, size);
            let raw = matches!(presentation.format, CatStdoutFormat::Human).then_some(data);
            (CatContentJson::Blob(json), raw)
        }
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

fn build_content_for_kind(
    kind: ObjectKind,
    obj: &Object,
    presentation: CatPresentationOptions,
) -> Result<(CatContentJson, Option<Vec<u8>>)> {
    let human = matches!(presentation.format, CatStdoutFormat::Human);
    match kind {
        ObjectKind::Blob => {
            let json =
                blob_payload_json(&obj.data, u64::try_from(obj.data.len()).unwrap_or(u64::MAX));
            let raw = human.then(|| obj.data.clone());
            Ok((CatContentJson::Blob(json), raw))
        }
        ObjectKind::Tree => {
            let parsed = grit_lib::objects::parse_tree(&obj.data)?;
            if human {
                let human = format_tree_entries_cat_file_p(&parsed);
                Ok((CatContentJson::Tree { entries: vec![] }, Some(human)))
            } else {
                let entries = parsed
                    .iter()
                    .map(|e| TreeLineJson {
                        mode: tree_entry_mode_cat_file_p(e.mode),
                        object_type: tree_entry_cat_file_type(e.mode).to_owned(),
                        oid: e.oid.to_hex(),
                        path: String::from_utf8_lossy(&e.name).into_owned(),
                    })
                    .collect();
                Ok((CatContentJson::Tree { entries }, None))
            }
        }
        ObjectKind::Commit => {
            let json = CatContentJson::Commit(text_object_json(&obj.data));
            let raw = human.then(|| obj.data.clone());
            Ok((json, raw))
        }
        ObjectKind::Tag => {
            let json = CatContentJson::Tag(text_object_json(&obj.data));
            let raw = human.then(|| obj.data.clone());
            Ok((json, raw))
        }
    }
}

fn text_object_json(data: &[u8]) -> TextObjectJson {
    if data.is_empty() || std::str::from_utf8(data).is_ok() {
        TextObjectJson {
            encoding: "utf-8".to_owned(),
            text: Some(String::from_utf8_lossy(data).into_owned()),
            data_base64: None,
        }
    } else {
        TextObjectJson {
            encoding: "base64".to_owned(),
            text: None,
            data_base64: Some(STANDARD.encode(data)),
        }
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

/// Dispatch parsed CLI args for `Cat`.
pub fn dispatch(
    args: crate::args::CatArgs,
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
    let CatArgs { spec } = args;
    emit_success(
        commands::cat::run(spec, presentation_from_mode(opts.mode))?,
        opts,
        ctx,
    )
}
