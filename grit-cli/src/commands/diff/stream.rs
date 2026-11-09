//! Stream tree patch diffs to stdout (human, Markdown, or JSON).

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::diff::{DiffEntry, DiffOptions};
use grit_lib::worktree::WorktreeError;

use super::{convert::cli_file_from_tree, DiffFormatArgs, DiffOutcome, PatchBodies};
use crate::output::with_streaming_stdout;
use crate::render::diff_stream::{FileDiffStream, FileDiffStreamSummary};
use crate::render::stream::{begin_diff_json_document, write_json_array_element, StreamRender};
use crate::render::RenderContext;
use crate::ui::PathDisplayOptions;

/// Stream ODB-backed tree diffs; returns a marker outcome with [`DiffOutcome::body_streamed`].
pub fn stream_tree_patch_to_stdout(
    repo: &grit_lib::repo::Repository,
    entries: &[DiffEntry],
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    bodies: PatchBodies,
    ctx: &RenderContext,
) -> Result<DiffOutcome> {
    let changed = !entries.is_empty();
    if !changed {
        return Ok(DiffOutcome {
            changed: false,
            files: Vec::new(),
            totals: None,
            format,
            path_display: None,
            body_streamed: false,
            preserve_changed: false,
        });
    }
    let path_display = PathDisplayOptions::from_repo(repo).ok();
    let summary = FileDiffStreamSummary { changed: true };
    with_streaming_stdout(|stdout| {
        let w: &mut dyn Write = stdout;
        let mut renderer = FileDiffStream::new(path_display.clone());
        let map_cli_err = |e: anyhow::Error| {
            grit_lib::error::Error::Worktree(WorktreeError::Path {
                detail: e.to_string(),
            })
        };
        let mut first_json = true;
        if matches!(bodies, PatchBodies::Collect) {
            begin_diff_json_document(w)?;
        } else {
            renderer.begin(w, ctx).map_err(map_cli_err)?;
        }
        repo.diff()
            .for_each_structured(entries, &diff_options, |structured| {
                let file = cli_file_from_tree(structured);
                match bodies {
                    PatchBodies::Collect => write_json_array_element(w, &mut first_json, &file)
                        .map_err(map_cli_err)
                        .map(|_| ()),
                    PatchBodies::Stream => renderer.item(w, ctx, &file).map_err(map_cli_err),
                    PatchBodies::MetadataOnly => Ok(()),
                }
            })
            .context("structured diff")?;
        match bodies {
            PatchBodies::Collect => {
                crate::render::stream::finish_diff_json_document(w, true, None::<&()>).map_err(
                    |e| {
                        grit_lib::error::Error::Worktree(WorktreeError::Path {
                            detail: e.to_string(),
                        })
                    },
                )?;
            }
            PatchBodies::Stream => renderer.finish(w, ctx, &summary).map_err(map_cli_err)?,
            PatchBodies::MetadataOnly => {}
        }
        Ok(())
    })?;
    Ok(DiffOutcome {
        changed: true,
        files: Vec::new(),
        totals: None,
        format,
        path_display,
        body_streamed: matches!(bodies, PatchBodies::Stream | PatchBodies::Collect),
        preserve_changed: true,
    })
}
