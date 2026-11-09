//! `grit revs` — list or count commits matching revision ranges.

use anyhow::{bail, Result};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::log::LogStdoutFormat;
use grit_lib::revwalk::rev_list::RevListResult;
use grit_lib::revwalk::{CommitExtent, RevListPresentationOptions};
use std::io::Write;

use crate::commits_json::{fetch_limit_plus_one, truncate_oid_list, CommitJsonLoader};
use crate::context;
use crate::output::RenderContext;
use crate::pathspec_args::{promote_long_magic_pathspecs_from_revs, TrailingPathspecArgs};
use crate::render::revs::{
    encode_revs_stream_human_batch, encode_revs_stream_markdown_batch, write_revs_empty_stream,
    write_revs_stream_markdown_header, RevsLine, RevsMarks, RevsStreamRow,
};
pub use crate::render::revs::{RevsCommit, RevsLeftRightCount, RevsOutcome, RevsSide};
use crate::rev_select::RevSelectArgs;
use crate::stream_commit_list::{
    begin_commits_json_document, finish_commits_json_document, flush_or_broken_pipe,
    load_batch_summaries, open_plain_stream, pull_batch, write_all_or_broken_pipe,
    write_json_array_element, BATCH, FIRST_BATCH,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevsPresentationOptions {
    pub format: LogStdoutFormat,
}

impl RevsPresentationOptions {
    #[must_use]
    pub fn json_output(self) -> bool {
        self.format == LogStdoutFormat::Json
    }

    #[must_use]
    pub fn list_extent(self) -> CommitExtent {
        RevListPresentationOptions.commit_extent_for_full_json(self.json_output())
    }

    #[must_use]
    pub fn markdown_output(self) -> bool {
        self.format == LogStdoutFormat::Markdown
    }

}

/// Counting and side-marking switches for [`run`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RevsMode {
    pub count: bool,
    pub left_right: bool,
    pub cherry_mark: bool,
}

fn commit_marks(oids: &[ObjectId], result: &RevListResult, mode: RevsMode) -> Vec<RevsMarks> {
    let sides = mode.left_right || mode.cherry_mark;
    let matches = result.cherry_match_index();
    oids.iter()
        .map(|oid| {
            let side = sides
                .then(|| result.left_right_map.get(oid))
                .flatten()
                .map(|&left| {
                    if left {
                        RevsSide::Left
                    } else {
                        RevsSide::Right
                    }
                });
            let group = matches.get(oid);
            let counterpart = group.and_then(|g| g.counterpart(oid));
            RevsMarks {
                side,
                equivalent: mode.cherry_mark.then(|| group.is_some()),
                equivalent_to: counterpart.map(|o| o.to_hex()),
                message_changed: counterpart.and(group.and_then(|g| g.message_changed)),
            }
        })
        .collect()
}

/// Whether plain [`run`] can stream commits instead of collecting the full walk first.
pub fn listing_can_stream(
    rev: &RevSelectArgs,
    pathspecs: &TrailingPathspecArgs,
    mode: RevsMode,
) -> Result<bool> {
    if mode.count || mode.left_right || mode.cherry_mark || pathspecs.has_any() {
        return Ok(false);
    }
    let repo = context::discover()?;
    let env = repo.environment();
    let mut rev = rev.clone();
    let mut pathspecs = pathspecs.clone();
    promote_long_magic_pathspecs_from_revs(&mut rev, &mut pathspecs);
    let walk_pathspec = pathspecs.rev_walk_pathspec(&repo)?;
    let opts = rev.to_rev_walk_options(&repo, env, &[], walk_pathspec)?;
    Ok(crate::stream_commit_list::plain_walk_streamable(&opts))
}

pub fn stream_listing(
    mut rev: RevSelectArgs,
    mut pathspecs: TrailingPathspecArgs,
    presentation: RevsPresentationOptions,
    _ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<()> {
    if rev.max_count() == Some(0) {
        return write_empty_streaming_document(w, presentation);
    }
    let repo = context::discover()?;
    let env = repo.environment();
    promote_long_magic_pathspecs_from_revs(&mut rev, &mut pathspecs);
    let walk_pathspec = pathspecs.rev_walk_pathspec(&repo)?;
    let opts = rev.to_rev_walk_options(&repo, env, &[], walk_pathspec)?;
    let token_refs = rev.rev_tokens(&[]);
    let fetch_cap = fetch_limit_plus_one(rev.max_count());
    let extent = presentation.list_extent();
    let mut stream = open_plain_stream(&repo, &token_refs, &opts, fetch_cap, extent)?;
    let loader = CommitJsonLoader::for_listed_commits();
    let now = context::wall_clock_now(env).unix_timestamp();
    let max = rev.max_count();
    let mut truncated = false;
    let mut emitted = 0usize;
    let mut pulled_from_stream = 0usize;
    let mut first_json = true;
    let mut first_batch = true;
    if presentation.json_output() {
        begin_commits_json_document(w)?;
    } else if presentation.markdown_output() {
        write_revs_stream_markdown_header(w)?;
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
        match presentation.format {
            LogStdoutFormat::Human => {
                let summaries = load_batch_summaries(&repo, &mut batch)?;
                let rows: Vec<RevsStreamRow> = summaries
                    .into_iter()
                    .map(|row| RevsStreamRow {
                        oid_hex: row.oid.to_hex(),
                        subject: row.subject,
                    })
                    .collect();
                let mut buf = Vec::new();
                encode_revs_stream_human_batch(&rows, &mut buf)?;
                if write_all_or_broken_pipe(w, &buf)? {
                    return Ok(());
                }
            }
            LogStdoutFormat::Markdown => {
                let summaries = load_batch_summaries(&repo, &mut batch)?;
                let rows: Vec<RevsStreamRow> = summaries
                    .into_iter()
                    .map(|row| RevsStreamRow {
                        oid_hex: row.oid.to_hex(),
                        subject: row.subject,
                    })
                    .collect();
                let mut buf = Vec::new();
                encode_revs_stream_markdown_batch(&rows, &mut buf)?;
                if write_all_or_broken_pipe(w, &buf)? {
                    return Ok(());
                }
            }
            LogStdoutFormat::Json => {
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
    if presentation.json_output() {
        finish_commits_json_document(w, truncated, ",\"count\":null")?;
    }
    Ok(())
}

fn write_empty_streaming_document(
    w: &mut dyn Write,
    presentation: RevsPresentationOptions,
) -> Result<()> {
    match presentation.format {
        LogStdoutFormat::Json => {
            begin_commits_json_document(w)?;
            finish_commits_json_document(w, false, ",\"count\":null")?;
        }
        LogStdoutFormat::Human | LogStdoutFormat::Markdown => {
            write_revs_empty_stream(w, presentation.format)?;
        }
    }
    Ok(())
}

pub fn run(
    mut rev: RevSelectArgs,
    mut pathspecs: TrailingPathspecArgs,
    mode: RevsMode,
    presentation: RevsPresentationOptions,
) -> Result<RevsOutcome> {
    let RevsMode {
        count, left_right, ..
    } = mode;
    if rev.revs.is_empty()
        && !rev.all
        && rev.branches.is_none()
        && rev.tags.is_none()
        && rev.remotes.is_none()
    {
        bail!("at least one revision range is required");
    }
    let repo = context::discover()?;
    let env = repo.environment();
    promote_long_magic_pathspecs_from_revs(&mut rev, &mut pathspecs);
    let walk_pathspec = pathspecs.rev_walk_pathspec(&repo)?;
    let opts = rev.to_rev_walk_options(&repo, env, &[], walk_pathspec)?;
    let walk = repo.revwalk();
    let token_refs = rev.rev_tokens(&[]);
    if (left_right || mode.cherry_mark) && rev.max_count() == Some(0) {
        walk.symmetric_range_tips(&token_refs)?;
    }
    if count {
        if rev.max_count() == Some(0) {
            return Ok(RevsOutcome {
                commits: Vec::new(),
                lines: Vec::new(),
                count: if left_right { None } else { Some(0) },
                left_right: left_right.then_some(RevsLeftRightCount { left: 0, right: 0 }),
                truncated: false,
                next: None,
            });
        }
        if left_right {
            let lr = walk.left_right_counts(&token_refs, &opts)?;
            return Ok(RevsOutcome {
                commits: Vec::new(),
                lines: Vec::new(),
                count: None,
                left_right: Some(lr.into()),
                truncated: false,
                next: None,
            });
        }
        let n = walk.count(&token_refs, &opts)?;
        return Ok(RevsOutcome {
            commits: Vec::new(),
            lines: Vec::new(),
            count: Some(n),
            left_right: None,
            truncated: false,
            next: None,
        });
    }

    if rev.max_count() == Some(0) {
        return Ok(RevsOutcome {
            commits: Vec::new(),
            lines: Vec::new(),
            count: None,
            left_right: None,
            truncated: false,
            next: None,
        });
    }
    let mut fetch_opts = opts.clone();
    fetch_opts.limit = fetch_limit_plus_one(rev.max_count());
    fetch_opts.retain_commit_data = true;
    fetch_opts.left_right = left_right;
    fetch_opts.cherry_mark = mode.cherry_mark;
    let mut list_result = walk.rev_list_result(&token_refs, &fetch_opts)?;
    let (oids, truncated) = truncate_oid_list(
        std::mem::take(&mut list_result.commits),
        rev.max_count(),
        rev.reverse,
    );
    let marks = commit_marks(&oids, &list_result, mode);
    let mut parsed = std::mem::take(&mut list_result.commit_data);
    let mut outcome = RevsOutcome {
        commits: Vec::new(),
        lines: Vec::new(),
        count: None,
        left_right: None,
        truncated,
        next: None,
    };
    if presentation.json_output() {
        let now = context::wall_clock_now(env).unix_timestamp();
        let commits = repo
            .odb()
            .with_pack_read_context(|| {
                CommitJsonLoader::for_listed_commits().load_with_summaries_reusing(
                    &repo,
                    &oids,
                    now,
                    &mut parsed,
                )
            })?
            .0;
        outcome.commits = commits
            .into_iter()
            .zip(marks)
            .map(|(c, m)| RevsCommit::new(c, m))
            .collect();
    } else {
        let summaries = repo.odb().with_pack_read_context(|| {
            CommitJsonLoader::load_summaries_only(&repo, &oids, &mut parsed)
        })?;
        outcome.lines = summaries
            .into_iter()
            .zip(marks)
            .map(|(s, marks)| RevsLine {
                oid: s.oid,
                subject: s.subject,
                marks,
            })
            .collect();
    }
    Ok(outcome)
}
