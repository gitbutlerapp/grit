//! `grit revs` — list or count commits matching revision ranges.

use grit_lib::progress::NullProgress;

use crate::commits_json::{fetch_limit_plus_one, truncate_oid_list, CommitJsonLoader};
use crate::context;
use crate::pathspec_args::{promote_long_magic_pathspecs_from_revs, TrailingPathspecArgs};
use crate::rev_select::RevSelectArgs;
use anyhow::{bail, Result};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::rev_list::RevListResult;

pub use crate::render::revs::*;

/// Counting and side-marking switches for [`run`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RevsMode {
    /// `--count`: print counts instead of commits.
    pub count: bool,
    /// `--left-right`: mark sides when listing, or count each side with `--count`.
    pub left_right: bool,
    /// `--cherry-mark`: pair the sides by patch-id when listing.
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

pub fn run(
    mut rev: RevSelectArgs,
    mut pathspecs: TrailingPathspecArgs,
    mode: RevsMode,
    emit: RevsEmitKind,
) -> Result<RevsOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let cancel = crate::interrupt::process_cancel_token();
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
            let lr = walk.left_right_counts(&token_refs, &opts, &mut NullProgress, &cancel)?;
            return Ok(RevsOutcome {
                commits: Vec::new(),
                lines: Vec::new(),
                count: None,
                left_right: Some(lr.into()),
                truncated: false,
                next: None,
            });
        }
        let n = walk.count(&token_refs, &opts, &mut NullProgress, &cancel)?;
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
    let mut list_result =
        walk.rev_list_result(&token_refs, &fetch_opts, &mut NullProgress, &cancel)?;
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
    if emit == RevsEmitKind::Json {
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

mod dispatch;
pub use dispatch::dispatch;
