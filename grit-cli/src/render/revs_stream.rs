//! Incremental `grit revs` listing on stdout.

use std::io::Write;

use anyhow::Result;
use grit_lib::revwalk::CommitExtent;

use crate::commits_json::{fetch_limit_plus_one, CommitJsonLoader};
use crate::context;
use crate::output::outln;
use crate::pathspec_args::{promote_long_magic_pathspecs_from_revs, TrailingPathspecArgs};
use crate::render::revs::RevsEmitKind;
use crate::render::revs::{RevsCommit, RevsMarks};
use crate::rev_select::RevSelectArgs;
use crate::stream_commit_list::{
    begin_commits_json_document, finish_commits_json_document, flush_or_broken_pipe,
    load_batch_summaries, open_plain_stream, pull_batch, write_all_or_broken_pipe,
    write_json_array_element, BATCH, FIRST_BATCH,
};

fn write_empty_streaming_document(w: &mut dyn Write, emit: RevsEmitKind) -> Result<()> {
    match emit {
        RevsEmitKind::Json => {
            begin_commits_json_document(w)?;
            finish_commits_json_document(w, false, ",\"count\":null")?;
        }
        RevsEmitKind::Markdown => {
            outln!(w, "# Commits");
            outln!(w);
            outln!(w, "_No commits._");
        }
        RevsEmitKind::Human => {}
    }
    Ok(())
}

/// List commits incrementally on a plain date-order walk (see [`crate::commands::revs::listing_can_stream`]).
///
/// # Errors
///
/// Propagates walk, load, and write errors other than a closed stdout pipe.
pub fn stream_listing(
    mut rev: RevSelectArgs,
    mut pathspecs: TrailingPathspecArgs,
    emit: RevsEmitKind,
    _ctx: &crate::render::RenderContext,
    w: &mut dyn Write,
) -> Result<()> {
    if rev.max_count() == Some(0) {
        return write_empty_streaming_document(w, emit);
    }
    let repo = context::discover()?;
    let env = repo.environment();
    promote_long_magic_pathspecs_from_revs(&mut rev, &mut pathspecs);
    let walk_pathspec = pathspecs.rev_walk_pathspec(&repo)?;
    let opts = rev.to_rev_walk_options(&repo, env, &[], walk_pathspec)?;
    let token_refs = rev.rev_tokens(&[]);
    let fetch_cap = fetch_limit_plus_one(rev.max_count());
    let extent = if emit == RevsEmitKind::Json {
        CommitExtent::Whole
    } else {
        CommitExtent::ThroughSubject
    };
    let mut stream = open_plain_stream(&repo, &token_refs, &opts, fetch_cap, extent)?;
    let loader = CommitJsonLoader::for_listed_commits();
    let now = context::wall_clock_now(env).unix_timestamp();
    let max = rev.max_count();
    let mut truncated = false;
    let mut emitted = 0usize;
    let mut pulled_from_stream = 0usize;
    let mut first_json = true;
    let mut first_batch = true;
    if emit == RevsEmitKind::Json {
        begin_commits_json_document(w)?;
    } else if emit == RevsEmitKind::Markdown {
        outln!(w, "# Commits");
        outln!(w);
    }
    loop {
        let cap = if first_batch { FIRST_BATCH } else { BATCH };
        let Some(mut batch) = pull_batch(&mut stream, cap)? else {
            break;
        };
        first_batch = false;
        pulled_from_stream += batch.oids.len();
        if let Some(limit) = max {
            if pulled_from_stream > limit {
                truncated = true;
            }
            if emitted + batch.oids.len() >= limit {
                let keep = limit.saturating_sub(emitted);
                batch.oids.truncate(keep);
                let keep: std::collections::HashSet<_> = batch.oids.iter().copied().collect();
                batch.walked.retain(|oid| keep.contains(oid));
            }
        }
        if batch.oids.is_empty() {
            break;
        }
        match emit {
            RevsEmitKind::Human => {
                let summaries = load_batch_summaries(&repo, &mut batch)?;
                let mut buf = Vec::new();
                for row in summaries {
                    outln!(buf, "{}  {}", row.oid, row.subject);
                }
                if write_all_or_broken_pipe(w, &buf)? {
                    return Ok(());
                }
            }
            RevsEmitKind::Markdown => {
                let summaries = load_batch_summaries(&repo, &mut batch)?;
                let mut buf = Vec::new();
                for row in summaries {
                    outln!(buf, "- `{}` {}", row.oid, row.subject);
                }
                if write_all_or_broken_pipe(w, &buf)? {
                    return Ok(());
                }
            }
            RevsEmitKind::Json => {
                let commits = repo.odb().with_pack_read_context(|| {
                    loader.load_reusing(&repo, &batch.oids, now, batch.walked)
                })?;
                for commit in commits {
                    let row = RevsCommit::new(commit, RevsMarks::default());
                    write_json_array_element(w, &mut first_json, &row)?;
                }
            }
        }
        emitted += batch.oids.len();
        if flush_or_broken_pipe(w)? {
            return Ok(());
        }
        if max.is_some_and(|limit| emitted >= limit) {
            break;
        }
    }
    if emit == RevsEmitKind::Json {
        finish_commits_json_document(w, truncated, ",\"count\":null")?;
    }
    Ok(())
}
