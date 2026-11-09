//! Incremental `grit log` listing on stdout.

use std::io::Write;

use anyhow::Result;
use grit_lib::revwalk::CommitExtent;

use crate::commands::log::{
    build_commit_outcome, empty_outcome, rows_need_commit_details, LogDisplayOptions, LogEmitKind,
    LogGraphBuild, LogStreamPlan,
};
use crate::commits_json::fetch_limit_plus_one;
use crate::diff_format::DiffFormatArgs;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::rev_select::RevSelectArgs;
use crate::stream_commit_list::{
    begin_commits_json_document, finish_commits_json_document, flush_or_broken_pipe,
    open_plain_stream, pull_batch, write_all_or_broken_pipe, write_json_array_element,
};
use crate::ui;

fn write_empty_log_stream(
    w: &mut dyn Write,
    emit: LogEmitKind,
    display: LogDisplayOptions,
    ctx: &RenderContext,
) -> Result<()> {
    let outcome = empty_outcome(display)?;
    match emit {
        LogEmitKind::Json => {
            begin_commits_json_document(w)?;
            finish_commits_json_document(w, false, "")?;
        }
        LogEmitKind::Human => outcome.render_human(w, ctx)?,
        LogEmitKind::Markdown => outcome.render_markdown(w, ctx)?,
    }
    Ok(())
}

/// Walk and render commits in batches for plain date-order log queries.
pub fn stream_listing(
    plan: LogStreamPlan,
    rev: RevSelectArgs,
    format: DiffFormatArgs,
    display: LogDisplayOptions,
    emit: LogEmitKind,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<()> {
    if rev.max_count() == Some(0) {
        return write_empty_log_stream(w, emit, display, ctx);
    }
    let LogStreamPlan {
        repo,
        walk_pathspec,
        default_tips,
        walk_opts,
    } = plan;
    let path_args = walk_pathspec.specs().to_vec();
    let default_tips: Vec<&str> = default_tips.iter().map(String::as_str).collect();
    let token_refs = rev.rev_tokens(&default_tips);
    let fetch_cap = fetch_limit_plus_one(rev.max_count());
    let extent = if rows_need_commit_details(display, &format, emit) {
        CommitExtent::Whole
    } else {
        CommitExtent::ThroughSubject
    };
    let mut stream = open_plain_stream(&repo, &token_refs, &walk_opts, fetch_cap, extent)?;
    let max = rev.max_count();
    let mut emitted = 0usize;
    let mut pulled = 0usize;
    let mut truncated = false;
    let mut first_json = true;
    let mut first_batch = true;
    let mut columns = display.columns;
    if emit == LogEmitKind::Json {
        begin_commits_json_document(w)?;
    }
    loop {
        let cap = if first_batch {
            crate::stream_commit_list::FIRST_BATCH
        } else {
            crate::stream_commit_list::BATCH
        };
        let Some(mut batch) = pull_batch(&mut stream, cap)? else {
            break;
        };
        first_batch = false;
        pulled += batch.oids.len();
        if let Some(limit) = max {
            if pulled > limit {
                truncated = true;
            }
            if emitted + batch.oids.len() > limit {
                let keep = limit.saturating_sub(emitted);
                batch.oids.truncate(keep);
                let keep_set: std::collections::HashSet<_> = batch.oids.iter().copied().collect();
                batch.walked.retain(|oid| keep_set.contains(oid));
            }
        }
        if batch.oids.is_empty() {
            break;
        }
        let batch_truncated = truncated || max.is_some_and(|n| pulled > n);
        let outcome = build_commit_outcome(
            &repo,
            &batch.oids,
            batch.walked,
            format,
            &path_args,
            None,
            batch_truncated && max.is_some(),
            display,
            emit,
            None,
            LogGraphBuild::default(),
        )?;
        if columns.is_none() && !outcome.commit_rows.is_empty() {
            columns = Some(ui::CommitColumns::for_stream(
                &outcome.commit_rows,
                ctx.now.unix_timestamp(),
            ));
        }
        let mut batch_display = display;
        batch_display.columns = columns;
        let mut batch_outcome = outcome;
        batch_outcome.display = batch_display;
        match emit {
            LogEmitKind::Human | LogEmitKind::Markdown => {
                let mut buf = Vec::new();
                if emit == LogEmitKind::Human {
                    batch_outcome.render_human(&mut buf, ctx)?;
                } else {
                    batch_outcome.render_markdown(&mut buf, ctx)?;
                }
                if write_all_or_broken_pipe(w, &buf)? {
                    return Ok(());
                }
            }
            LogEmitKind::Json => {
                for commit in &batch_outcome.commits {
                    write_json_array_element(w, &mut first_json, commit)?;
                }
            }
        }
        emitted += batch.oids.len();
        if flush_or_broken_pipe(w)? {
            return Ok(());
        }
        if max.is_some_and(|n| emitted >= n) {
            break;
        }
    }
    if emit == LogEmitKind::Json {
        let final_truncated = truncated || max.is_some_and(|n| pulled > n);
        finish_commits_json_document(w, final_truncated, "")?;
    }
    Ok(())
}
