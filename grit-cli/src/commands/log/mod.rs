//! `grit log` — the recent history reachable from HEAD.
//!
//! For scripts and agents (`--json`, `--markdown`, or output that is not a terminal) it
//! shows one page of commits, newest first, and when there's more, a `--cursor` token for
//! the next page. On a terminal it shows the whole history through the pager.

use grit_lib::progress::{CancelToken, NullProgress};

use anyhow::{bail, Context, Result};
use grit_lib::diff::{CommitFilesManyOptions, DiffOptions};
use grit_lib::gitbutler::GitButlerWorkspace;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::log::CommitReuse;
use grit_lib::revwalk::group_by::GroupByAuthorOptions;
use grit_lib::revwalk::log_walk::{LogWalkCursor, LogWalkQuery};
use grit_lib::revwalk::object_inspect::OidAbbrevBatch;
use grit_lib::revwalk::CommitExtent;
use grit_lib::revwalk::{
    GraphCharset, GraphCommitInput, GraphLayout, GraphLineKind, GraphRenderedLine,
};
use grit_lib::state::HeadState;
use schemars::JsonSchema;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::commands::diff::{cli_outcome_from_lib, resolve_commit_diff_pathspecs, DiffOutcome};
use crate::commands::show::diffstat;
use crate::commits_json::{fetch_limit_plus_one, truncate_oid_list, CommitJsonLoader};
use crate::context::{self, CommitSummary};
use crate::diff_format::DiffFormatArgs;
use crate::log_display::{
    decorate_commit_row, format_decorations_folded, render_oneline_line, render_oneline_oid_subject,
};
use crate::output::{CommitJson, RenderContext};
use crate::pathspec_args::{promote_long_magic_pathspecs_from_revs, TrailingPathspecArgs};
use crate::render::HumanRender;
use crate::rev_select::{self, RevSelectArgs};
use crate::ui;

/// How many commits to show per page when the caller does not set `-n` / `--max-count`.
const PAGE: usize = 20;

/// How many commits one `grit log` run lists when `-n` / `--max-count` is not given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogPage {
    /// One page of [`PAGE`] commits and a `--cursor` token for the next (scripts, agents).
    ///
    /// Walks that cannot resume from a cursor (path limits, commit filters) list every
    /// match, except `--graph`, which stops at a page.
    Paged,
    /// Batches of this many commits for [`LogStream`], resumed from the walk itself instead of
    /// a token. Walks that cannot resume list every match at once.
    Batches(usize),
}

impl LogPage {
    /// Page size for walks that can hand out a continuation cursor.
    fn cursor_page(self) -> usize {
        match self {
            Self::Paged => PAGE,
            Self::Batches(n) => n,
        }
    }

    /// Commit limit for `--graph` walks.
    fn graph_limit(self) -> Option<usize> {
        match self {
            Self::Paged => Some(PAGE),
            Self::Batches(_) => None,
        }
    }
}

/// Group commits for summary output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogGroupBy {
    Author,
}

/// Display flags for human / Markdown rendering (not part of `--json`).
#[derive(Clone, Copy, Debug, Default)]
pub struct LogDisplayOptions {
    pub oneline: bool,
    pub decorate: bool,
    pub group_by: Option<LogGroupBy>,
    pub group_count_only: bool,
    pub graph: bool,
    /// When set with `--graph`, draw the graph with ASCII `*|` instead of Unicode box-drawing.
    pub graph_ascii: bool,
    /// End each default human row with a `+added -removed` line summary (on a terminal).
    pub line_counts: bool,
    /// Author and date column widths to keep, so streamed batches line up.
    pub columns: Option<ui::CommitColumns>,
    /// The repository has no commits (`HEAD` unset); an empty page is not "no history".
    pub no_commits_in_repo: bool,
}

/// Result of `grit log`: one page of commits, plus the next page's start (if any).
#[derive(Serialize, JsonSchema)]
pub struct LogOutcome {
    #[serde(default)]
    pub commits: Vec<LogCommit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<LogAuthorGroupJson>>,
    /// Opaque continuation cursor for `--cursor`, or `null` when there is no further history.
    pub next: Option<String>,
    /// True when more commits matched the query than returned in this page (`-n` or default page size).
    pub truncated: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) author_group_rows: Vec<(usize, String, String)>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) display: LogDisplayOptions,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) head_branch: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) oneline_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) graph_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) graph_oneline_stream: Option<RefCell<GraphOnelineStream>>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) graph_note: Option<&'static str>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) continuation_args: Option<String>,
    /// Per-commit line summaries for human rows (see [`LogDisplayOptions::line_counts`]).
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) line_counts: Vec<Option<ui::LineCounts>>,
    /// Where a [`LogPage::Batches`] walk continues.
    #[serde(skip)]
    #[schemars(skip)]
    resume: Option<(LogWalkQuery, LogWalkCursor)>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) format: DiffFormatArgs,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) commit_diffs: Vec<Option<DiffOutcome>>,
    /// Resolved inputs for patch output deferred until human rendering.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) patch_text: Option<LogPatchTextParams>,
}

pub(crate) struct LogPatchTextParams {
    pub(crate) oids: Vec<ObjectId>,
    pub(crate) pathspec: grit_lib::worktree::pathspec::Pathspec,
}

/// The `--cursor` token for the next page, only when the listing is shown as a page (a
/// streamed listing hands its walk on to the next batch instead).
///
/// The token is the short form stored under `.git`; when the repository is not writable it
/// falls back to the long self-contained form, so a read-only repository still pages.
fn encode_next_cursor(
    repo: &grit_lib::repo::Repository,
    cursor: Option<LogWalkCursor>,
    page: LogPage,
) -> Result<Option<String>> {
    let Some(cursor) = cursor.filter(|_| page == LogPage::Paged) else {
        return Ok(None);
    };
    match cursor.encode_short(repo) {
        Ok(token) => Ok(Some(token)),
        Err(grit_lib::error::Error::Io(_)) => cursor
            .encode()
            .map(Some)
            .context("could not encode log continuation cursor"),
        Err(e) => Err(e).context("could not store log continuation cursor"),
    }
}

fn format_log_cursor_command(rev_args: Option<&str>, cursor: &str) -> String {
    match rev_args.filter(|s| !s.is_empty()) {
        Some(revs) => format!("grit log {revs} --cursor={cursor}"),
        None => format!("grit log --cursor={cursor}"),
    }
}

/// What to tell the reader about the history past this page, if anything.
pub(crate) fn continuation_hint(outcome: &LogOutcome) -> Option<String> {
    if !outcome.truncated {
        return None;
    }
    Some(match &outcome.next {
        Some(next) => format_log_cursor_command(outcome.continuation_args.as_deref(), next),
        None => "rerun with a larger `-n`".to_owned(),
    })
}

/// Author aggregate in `--json` output when `--group-by author` is set.
#[derive(Serialize, JsonSchema)]
pub struct LogAuthorGroupJson {
    pub name: String,
    pub email: String,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<CommitJson>>,
}

/// How listing builds rows (mapped from [`crate::output::OutputMode`] in dispatch only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogEmitKind {
    Human,
    Json,
    Markdown,
}

pub use crate::log_commit::LogCommit;

/// The repository and walk a streamed listing runs on, set up once by
/// [`listing_stream_plan`] and consumed by [`crate::render::log_stream::stream_listing`].
pub struct LogStreamPlan {
    pub(crate) repo: grit_lib::repo::Repository,
    pub(crate) walk_pathspec: grit_lib::worktree::pathspec::Pathspec,
    pub(crate) default_tips: Vec<String>,
    pub(crate) walk_opts: grit_lib::revwalk::RevWalkListOptions,
}

/// The plan for [`crate::render::log_stream::stream_listing`] when it can replace a full [`run`] for this query, or
/// `None` when it cannot.
pub fn listing_stream_plan(
    rev: &RevSelectArgs,
    pathspecs: &TrailingPathspecArgs,
    display: LogDisplayOptions,
    cursor: &Option<String>,
    format: &DiffFormatArgs,
) -> Result<Option<LogStreamPlan>> {
    if cursor.as_ref().is_some_and(|token| !token.is_empty()) {
        return Ok(None);
    }
    if display.group_by.is_some() || display.graph {
        return Ok(None);
    }
    if format.log_patch_text() {
        return Ok(None);
    }
    let repo = context::discover()?;
    let mut rev = rev.clone();
    let mut pathspecs = pathspecs.clone();
    promote_long_magic_pathspecs_from_revs(&mut rev, &mut pathspecs);
    let walk_pathspec = pathspecs.rev_walk_pathspec(&repo)?;
    let default_tips = default_tips(&repo, &rev)?;
    let tip_refs: Vec<&str> = default_tips.iter().map(String::as_str).collect();
    let walk_opts =
        rev.to_rev_walk_options(&repo, repo.environment(), &tip_refs, walk_pathspec.clone())?;
    if !crate::stream_commit_list::plain_walk_streamable(&walk_opts) {
        return Ok(None);
    }
    let token_refs = rev.rev_tokens(&tip_refs);
    if LogWalkQuery::from_rev_walk(&repo, &token_refs, &walk_opts)?.is_none() {
        return Ok(None);
    }
    Ok(Some(LogStreamPlan {
        repo,
        walk_pathspec,
        default_tips,
        walk_opts,
    }))
}

/// Whether the caller asked for enough commits that incremental listing helps.
#[must_use]
pub fn listing_should_stream_incrementally(rev: &RevSelectArgs, non_terminal: bool) -> bool {
    match rev.max_count() {
        Some(0) => false,
        Some(n) => n > PAGE,
        None => !non_terminal,
    }
}

pub fn run(
    mut rev: RevSelectArgs,
    mut pathspecs: TrailingPathspecArgs,
    cursor: Option<String>,
    format: DiffFormatArgs,
    display: LogDisplayOptions,
    output_mode: LogEmitKind,
    page: LogPage,
) -> Result<LogOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let cancel = crate::interrupt::process_cancel_token();
    if display.group_by.is_some() {
        if format.any_format_flag() {
            bail!("`grit log` cannot combine diff format flags with `--group-by`");
        }
        if display.oneline {
            bail!("`grit log` cannot combine `--oneline` with `--group-by`");
        }
        if display.graph {
            bail!("`grit log` cannot combine `--graph` with `--group-by`");
        }
    }
    if display.graph {
        if format.any_format_flag() {
            bail!("`grit log` cannot combine diff format flags with `--graph`");
        }
        if output_mode == LogEmitKind::Json {
            return Err(crate::json_error::incompatible_output_flags(
                "`grit log --graph` is for human output only; omit `--graph` when using `--json`",
                crate::error_kind::ErrorKind::IncompatibleFlags,
            ));
        }
    }
    let repo = context::discover()?;
    let display = LogDisplayOptions {
        no_commits_in_repo: repo.references().head().is_err(),
        ..display
    };
    let env = repo.environment();
    promote_long_magic_pathspecs_from_revs(&mut rev, &mut pathspecs);
    let walk_pathspec = pathspecs.rev_walk_pathspec(&repo)?;
    let path_args = walk_pathspec.specs().to_vec();
    let default_tips = default_tips(&repo, &rev)?;
    let default_tips: Vec<&str> = default_tips.iter().map(String::as_str).collect();
    let opts = rev.to_rev_walk_options(&repo, env, &default_tips, walk_pathspec)?;
    let continuation = cursor.filter(|token| !token.is_empty());
    if display.group_by.is_some() && continuation.is_some() {
        bail!("`grit log` cannot combine `--cursor` with `--group-by`");
    }
    if display.graph && continuation.is_some() {
        bail!("`grit log` cannot combine `--graph` with `--cursor`");
    }
    if let Some(token) = continuation.as_deref() {
        if pathspecs.has_any() {
            bail!(
                "`grit log` cannot combine `--cursor` with path filters; drop the cursor or the paths"
            );
        }
        if rev.has_log_walk_selectors() {
            bail!(
                "`grit log` cannot combine `--cursor` with revision, date, or ref-set selectors; \
                 pass only `--cursor` (and optionally `-n` / `--max-count` for page size)"
            );
        }
        if rev.reverse
            || rev.first_parent
            || rev.merges
            || rev.no_merges
            || rev.skip > 0
            || rev.has_commit_filter_flags()
        {
            bail!(
                "`grit log` cannot combine `--cursor` with `--reverse`, `--first-parent`, \
                 `--merges`, `--no-merges`, `--skip`, or commit filters (`--author`, `--grep`, …)"
            );
        }
        let resume = LogWalkCursor::decode_with_git_dir(token, Some(repo.git_dir()))
            .context("invalid log continuation cursor")?;
        let query = resume.query().clone();
        let page_size = rev.max_count().unwrap_or(page.cursor_page());
        let walked = LogWalkCursor::step(&repo, &query, Some(&resume), page_size)
            .context("could not list commits")?;
        let truncated = walked.next.is_some();
        let next = encode_next_cursor(&repo, walked.next, page)?;
        return build_commit_outcome(
            &repo,
            &walked.commits,
            walked.commit_data,
            format,
            &path_args,
            next,
            truncated,
            display,
            output_mode,
            None,
            LogGraphBuild::default(),
        );
    }

    let token_refs = rev.rev_tokens(&default_tips);
    let page_size = rev.max_count().unwrap_or(page.cursor_page());

    let mut walk_opts = opts.clone();
    walk_opts.graph = display.graph;

    if display.group_by == Some(LogGroupBy::Author) {
        return run_log_group_by_author(
            &repo,
            &token_refs,
            &walk_opts,
            &rev,
            display,
            output_mode,
            &cancel,
        );
    }

    let needs_rev_walk = display.graph
        || pathspecs.has_any()
        || walk_opts
            .commit_filter
            .as_ref()
            .is_some_and(|f| f.is_active())
        || walk_opts.pickaxe.is_some();
    let log_query = if needs_rev_walk {
        None
    } else {
        LogWalkQuery::from_rev_walk(&repo, &token_refs, &walk_opts)?
    };

    let Some(query) = log_query else {
        if rev.max_count() == Some(0) {
            return empty_outcome(display);
        }
        let walk = repo.revwalk();
        let mut fetch_opts = walk_opts.clone();
        let page_limit = rev.max_count().or(if display.graph {
            page.graph_limit()
        } else {
            None
        });
        fetch_opts.limit = fetch_limit_plus_one(page_limit);
        fetch_opts.retain_commit_data = !display.graph;
        fetch_opts.retained_commit_extent = if fetch_opts.retain_commit_data
            && log_through_subject_retained_extent(display, &format, output_mode)
        {
            CommitExtent::ThroughSubject
        } else {
            CommitExtent::Whole
        };
        let mut list_result = walk
            .rev_list_result(&token_refs, &fetch_opts, &mut NullProgress, &cancel)
            .context("could not list commits")?;
        let parsed = std::mem::take(&mut list_result.commit_data);
        let graph_linkage = list_result.log_graph_linkage.take();
        let (oids, truncated) = truncate_oid_list(list_result.commits, page_limit, rev.reverse);
        return build_commit_outcome(
            &repo,
            &oids,
            parsed,
            format,
            &path_args,
            None,
            truncated,
            display,
            output_mode,
            None,
            LogGraphBuild {
                parent_rewrite: graph_needs_parent_rewrite(pathspecs.has_any(), &walk_opts),
                linkage: graph_linkage,
            },
        );
    };

    let walked =
        LogWalkCursor::step(&repo, &query, None, page_size).context("could not list commits")?;
    let truncated = walked.next.is_some();
    // A one-shot `-n` asks for that many commits, not for the first page of a walk.
    let resumable = walked.next.filter(|_| rev.max_count().is_none());
    // A batch hands its walk on to the next batch instead of to a `--cursor` token.
    let (next, resume) = match page {
        LogPage::Batches(_) => (None, resumable.map(|cursor| (query, cursor))),
        LogPage::Paged => (encode_next_cursor(&repo, resumable, page)?, None),
    };
    let mut outcome = build_commit_outcome(
        &repo,
        &walked.commits,
        walked.commit_data,
        format,
        &path_args,
        next,
        truncated && resume.is_none(),
        display,
        output_mode,
        None,
        LogGraphBuild::default(),
    )?;
    outcome.resume = resume;
    Ok(outcome)
}

/// Where the walk starts when no revision is given: `HEAD`, or on a GitButler workspace the
/// branches merged into it, so GitButler's own workspace commit is not listed.
fn default_tips(repo: &grit_lib::repo::Repository, rev: &RevSelectArgs) -> Result<Vec<String>> {
    if !rev.revs.is_empty() {
        return Ok(Vec::new());
    }
    let workspace = GitButlerWorkspace::detect(repo).context("could not read HEAD")?;
    Ok(match workspace.filter(|w| !w.heads.is_empty()) {
        Some(workspace) => workspace
            .head_commits()
            .iter()
            .map(ObjectId::to_hex)
            .collect(),
        None => vec!["HEAD".to_owned()],
    })
}

#[derive(Clone, Default)]
pub(crate) struct LogGraphBuild {
    parent_rewrite: bool,
    linkage: Option<grit_lib::revwalk::rev_list::LogGraphLinkage>,
}

pub(crate) struct GraphOnelineStream {
    oids: Vec<ObjectId>,
    parent_map: HashMap<ObjectId, Vec<ObjectId>>,
    subjects: HashMap<ObjectId, String>,
    layout: GraphLayout,
    next_commit: usize,
    pending_block: Option<(Vec<GraphRenderedLine>, usize, ObjectId)>,
}

impl GraphOnelineStream {
    fn fill_pending(&mut self, oid: ObjectId) {
        let parents = self.parent_map.get(&oid).cloned().unwrap_or_default();
        let block = self
            .layout
            .render_commit(&GraphCommitInput { id: oid, parents });
        self.pending_block = Some((block, 0, oid));
    }

    /// Returns the next complete line to print, or `None` when finished.
    pub(crate) fn next_line(
        &mut self,
        repo: &grit_lib::repo::Repository,
        abbrev: &mut OidAbbrevBatch,
    ) -> Result<Option<String>> {
        let term_width = ui::terminal_width();
        loop {
            if let Some((block, line_idx, oid)) = self.pending_block.as_mut() {
                if *line_idx < block.len() {
                    let line = &block[*line_idx];
                    *line_idx += 1;
                    if line.kind == GraphLineKind::Padding {
                        return Ok(Some(line.prefix.trim_end().to_owned()));
                    }
                    let lead = ui::display_width(&line.prefix);
                    let subject = self
                        .subjects
                        .get(oid)
                        .map(String::as_str)
                        .unwrap_or("(no subject)");
                    let text = render_oneline_oid_subject(repo, *oid, subject, abbrev)?;
                    let text = ui::fit_width(&text, term_width.map(|w| w.saturating_sub(lead)));
                    return Ok(Some(format!("{}{text}", line.prefix)));
                }
                self.pending_block = None;
            }
            if self.next_commit >= self.oids.len() {
                return Ok(None);
            }
            let oid = self.oids[self.next_commit];
            self.next_commit += 1;
            self.fill_pending(oid);
        }
    }
}

fn graph_needs_parent_rewrite(
    has_pathspecs: bool,
    walk: &grit_lib::revwalk::RevWalkListOptions,
) -> bool {
    has_pathspecs || walk.commit_filter.as_ref().is_some_and(|f| f.is_active())
}

fn resolve_current_branch(repo: &grit_lib::repo::Repository) -> Result<Option<String>> {
    Ok(
        match repo.references().head().context("could not resolve HEAD")? {
            HeadState::Branch { short_name, .. } => Some(short_name),
            _ => None,
        },
    )
}

pub(crate) fn empty_outcome(display: LogDisplayOptions) -> Result<LogOutcome> {
    Ok(LogOutcome {
        commits: Vec::new(),
        groups: if display.group_by.is_some() {
            Some(Vec::new())
        } else {
            None
        },
        next: None,
        truncated: false,
        commit_rows: Vec::new(),
        author_group_rows: Vec::new(),
        display,
        head_branch: None,
        oneline_lines: Vec::new(),
        graph_lines: Vec::new(),
        graph_oneline_stream: None,
        graph_note: None,
        continuation_args: None,
        line_counts: Vec::new(),
        resume: None,
        format: DiffFormatArgs::default(),
        commit_diffs: Vec::new(),
        patch_text: None,
    })
}

fn run_log_group_by_author(
    repo: &grit_lib::repo::Repository,
    token_refs: &[&str],
    walk_opts: &grit_lib::revwalk::RevWalkListOptions,
    rev: &RevSelectArgs,
    display: LogDisplayOptions,
    output_mode: LogEmitKind,
    cancel: &CancelToken,
) -> Result<LogOutcome> {
    let use_mailmap = rev_select::log_mailmap_enabled(repo, rev.no_mailmap);
    let max_count = rev.max_count();
    let reverse = rev.reverse;
    let mailmap = repo.revwalk().mailmap_for_grouping(use_mailmap)?;
    let need_commit_json = output_mode == LogEmitKind::Json && !display.group_count_only;
    let group_options = GroupByAuthorOptions {
        count_only: !need_commit_json,
    };

    if need_commit_json {
        if let Some(limit) = max_count {
            let mut fetch_opts = walk_opts.clone();
            fetch_opts.limit = fetch_limit_plus_one(Some(limit));
            let oids = repo
                .revwalk()
                .commits(token_refs, &fetch_opts, &mut NullProgress, &cancel)
                .context("could not list commits")?;
            let (oids, truncated) = truncate_oid_list(oids, Some(limit), reverse);
            let grouped = repo
                .revwalk()
                .group_by_author(
                    &oids,
                    mailmap.as_ref(),
                    GroupByAuthorOptions { count_only: false },
                )
                .context("could not group commits by author")?;
            return build_author_group_outcome_from_groups(
                repo,
                grouped,
                truncated,
                display,
                output_mode,
            );
        }
        let grouped = repo
            .revwalk()
            .group_by_author_in_walk(
                token_refs,
                walk_opts,
                mailmap.as_ref(),
                GroupByAuthorOptions { count_only: false },
            )
            .context("could not group commits by author")?;
        return build_author_group_outcome_from_groups(repo, grouped, false, display, output_mode);
    }

    if max_count.is_none() {
        let grouped = repo
            .revwalk()
            .group_by_author_in_walk(token_refs, walk_opts, mailmap.as_ref(), group_options)
            .context("could not group commits by author")?;
        return build_author_group_outcome_from_groups(repo, grouped, false, display, output_mode);
    }

    let mut fetch_opts = walk_opts.clone();
    fetch_opts.limit = fetch_limit_plus_one(max_count);
    let oids = repo
        .revwalk()
        .commits(token_refs, &fetch_opts, &mut NullProgress, &cancel)
        .context("could not list commits")?;
    let (oids, truncated) = truncate_oid_list(oids, max_count, reverse);
    let grouped = repo
        .revwalk()
        .group_by_author(&oids, mailmap.as_ref(), group_options)
        .context("could not group commits by author")?;
    build_author_group_outcome_from_groups(repo, grouped, truncated, display, output_mode)
}

fn build_author_group_outcome_from_groups(
    repo: &grit_lib::repo::Repository,
    grouped: Vec<grit_lib::revwalk::AuthorGroup>,
    truncated: bool,
    display: LogDisplayOptions,
    output_mode: LogEmitKind,
) -> Result<LogOutcome> {
    let include_commits = output_mode == LogEmitKind::Json && !display.group_count_only;
    let loader = if include_commits {
        Some(CommitJsonLoader::new(repo)?)
    } else {
        None
    };
    let now = if include_commits {
        Some(context::wall_clock_now(repo.environment()).unix_timestamp())
    } else {
        None
    };
    let mut json_groups = Vec::with_capacity(grouped.len());
    let mut author_group_rows = Vec::with_capacity(grouped.len());
    for g in &grouped {
        author_group_rows.push((g.count, g.name.clone(), g.email.clone()));
        let commits = if let (Some(loader), Some(now)) = (loader.as_ref(), now) {
            Some(loader.load(repo, &g.commits, now)?)
        } else {
            None
        };
        json_groups.push(LogAuthorGroupJson {
            name: g.name.clone(),
            email: g.email.clone(),
            count: g.count,
            commits,
        });
    }
    Ok(LogOutcome {
        commits: Vec::new(),
        groups: Some(json_groups),
        next: None,
        truncated,
        commit_rows: Vec::new(),
        author_group_rows,
        display,
        head_branch: None,
        oneline_lines: Vec::new(),
        graph_lines: Vec::new(),
        graph_oneline_stream: None,
        graph_note: None,
        continuation_args: None,
        line_counts: Vec::new(),
        resume: None,
        format: DiffFormatArgs::default(),
        commit_diffs: Vec::new(),
        patch_text: None,
    })
}

pub(crate) type LoadedCommitList = (Vec<LogCommit>, Vec<CommitSummary>, Vec<Option<DiffOutcome>>);

/// Load commit rows for a fixed OID list, optionally attaching per-commit diff detail.
///
/// The human [`CommitSummary`] rows are left empty unless `summaries` is set.
pub(crate) fn load_commits_with_format_for_oids(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    format: DiffFormatArgs,
    path_args: &[String],
    parsed: impl CommitReuse,
    summaries: bool,
) -> Result<LoadedCommitList> {
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let loader = CommitJsonLoader::for_listed_commits();
    let (commit_json_rows, commit_rows) = if summaries {
        loader.load_with_summaries_reusing(repo, oids, now, parsed)?
    } else {
        (loader.load_reusing(repo, oids, now, parsed)?, Vec::new())
    };

    let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
    let many_results = if let Some(detail) = format.detail_for_log() {
        let config = repo.config().snapshot().context("could not load config")?;
        let thread_policy = repo.thread_policy();
        let mut diff_options = DiffOptions::from_config(config.as_ref());
        format.apply_context(&mut diff_options);
        diff_options.word_level_inline = false;
        let opts = CommitFilesManyOptions {
            diff_options,
            detail,
            skip_merge_patches: true,
        };
        Some((
            detail,
            repo.diff()
                .commit_files_many(oids, &pathspecs, opts, config.as_ref(), &thread_policy)
                .context("could not diff commits")?,
        ))
    } else {
        None
    };

    let mut commits = Vec::with_capacity(oids.len());
    let mut commit_diffs = Vec::with_capacity(oids.len());
    for (i, commit) in commit_json_rows.into_iter().enumerate() {
        if let Some((detail, ref results)) = many_results {
            let outcome = cli_outcome_from_lib(repo, results[i].output.clone(), detail, format)?;
            let stat = if format.stat && results[i].parent_count < 2 {
                Some(diffstat(&outcome))
            } else {
                None
            };
            commits.push(LogCommit {
                commit,
                files: Some(outcome.files.clone()),
                totals: outcome.totals,
                stat,
            });
            commit_diffs.push(Some(outcome));
        } else {
            commits.push(LogCommit {
                commit,
                files: None,
                totals: None,
                stat: None,
            });
            commit_diffs.push(None);
        }
    }
    Ok((commits, commit_rows, commit_diffs))
}

/// Whether listing rows need whole commits (refs, diffs or JSON commit rows) rather than the
/// author, date, change id and subject that [`CommitExtent::ThroughSubject`] keeps.
pub(crate) fn rows_need_commit_details(
    display: LogDisplayOptions,
    format: &DiffFormatArgs,
    output_mode: LogEmitKind,
) -> bool {
    display.decorate
        || output_mode == LogEmitKind::Json
        || (format.any_format_flag() && !format.log_patch_text())
}

/// Use a through-subject read for listing when one-line human/Markdown output needs no extra fields.
fn log_through_subject_retained_extent(
    display: LogDisplayOptions,
    format: &DiffFormatArgs,
    output_mode: LogEmitKind,
) -> bool {
    display.oneline
        && !rows_need_commit_details(display, format, output_mode)
        && !display.line_counts
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_commit_outcome(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    mut parsed: impl CommitReuse,
    format: DiffFormatArgs,
    path_args: &[String],
    next: Option<String>,
    truncated: bool,
    display: LogDisplayOptions,
    output_mode: LogEmitKind,
    continuation_args: Option<String>,
    graph: LogGraphBuild,
) -> Result<LogOutcome> {
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let json_output = output_mode == LogEmitKind::Json;
    let graph_human_oneline = display.graph
        && output_mode == LogEmitKind::Human
        && display.oneline
        && !format.any_format_flag()
        && !display.decorate;
    let need_commit_details = rows_need_commit_details(display, &format, output_mode);
    let plain_oneline = display.oneline
        && !need_commit_details
        && !display.decorate
        && (!display.graph || output_mode == LogEmitKind::Markdown);
    let head_branch = if display.oneline || display.decorate {
        resolve_current_branch(repo)?
    } else {
        None
    };

    if format.log_patch_text() {
        let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
        return Ok(LogOutcome {
            commits: Vec::new(),
            groups: None,
            next,
            truncated,
            commit_rows: Vec::new(),
            author_group_rows: Vec::new(),
            display,
            head_branch: None,
            oneline_lines: Vec::new(),
            graph_lines: Vec::new(),
            graph_oneline_stream: None,
            graph_note: None,
            continuation_args,
            line_counts: Vec::new(),
            resume: None,
            format,
            commit_diffs: Vec::new(),
            patch_text: Some(LogPatchTextParams {
                oids: oids.to_vec(),
                pathspec: pathspecs,
            }),
        });
    }

    let graph_note = if display.graph && output_mode == LogEmitKind::Markdown {
        Some("`--graph` affects human output only; this listing omits graph columns.")
    } else {
        None
    };

    let mut abbrev_batch = crate::log_display::display_abbrev_batch(repo)?;

    if plain_oneline {
        use grit_lib::objects::read_commit_subject_line;

        let oneline_lines = oids
            .iter()
            .map(|oid| {
                let abbrev_hex = abbrev_batch.abbrev_known(repo, *oid)?;
                let subject = match parsed.cached_subject_line(oid) {
                    Some(Ok(s)) => s,
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        read_commit_subject_line(&repo.odb().read_commit_through_subject(oid)?)?
                    }
                };
                Ok(format!("{abbrev_hex} {subject}"))
            })
            .collect::<Result<Vec<_>>>()?;
        drop(parsed);
        return Ok(LogOutcome {
            commits: Vec::new(),
            groups: None,
            next,
            truncated,
            commit_rows: Vec::new(),
            author_group_rows: Vec::new(),
            display,
            head_branch,
            oneline_lines,
            graph_lines: Vec::new(),
            graph_oneline_stream: None,
            graph_note,
            continuation_args,
            line_counts: Vec::new(),
            resume: None,
            format,
            commit_diffs: Vec::new(),
            patch_text: None,
        });
    }

    if graph_human_oneline {
        let graph_oneline_stream = Some(RefCell::new(build_graph_oneline_stream(
            repo, oids, display, graph,
        )?));
        return Ok(LogOutcome {
            commits: Vec::new(),
            groups: None,
            next,
            truncated,
            commit_rows: Vec::new(),
            author_group_rows: Vec::new(),
            display,
            head_branch: None,
            oneline_lines: Vec::new(),
            graph_lines: Vec::new(),
            graph_oneline_stream,
            graph_note: None,
            continuation_args,
            line_counts: Vec::new(),
            resume: None,
            format,
            commit_diffs: Vec::new(),
            patch_text: None,
        });
    }

    // Without refs, diffs or JSON output, human and Markdown rows render from the summaries
    // alone, so no JSON commit rows are built.
    let (commits, commit_rows, commit_diffs) = if need_commit_details {
        load_commits_with_format_for_oids(repo, oids, format, path_args, parsed, !json_output)?
    } else {
        let summaries = CommitJsonLoader::load_summaries_only(repo, oids, &mut parsed)?;
        drop(parsed);
        (Vec::new(), summaries, Vec::new())
    };

    let head_branch_ref = head_branch.as_deref();
    // Markdown lists `--graph --oneline` walks as plain oneline bullets (see `graph_note`).
    let oneline_lines =
        if display.oneline && (!display.graph || output_mode == LogEmitKind::Markdown) {
            commits
                .iter()
                .map(|c| {
                    render_oneline_line(
                        repo,
                        &mut abbrev_batch,
                        &c.commit,
                        display.decorate,
                        head_branch_ref,
                    )
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };

    let line_counts = if display.line_counts
        && output_mode == LogEmitKind::Human
        && !display.oneline
        && !format.any_format_flag()
    {
        commit_line_counts(repo, oids, path_args)?
    } else {
        Vec::new()
    };

    let mut graph_for_text = graph;
    let graph_lines = if display.graph && output_mode == LogEmitKind::Human {
        build_graph_text_lines(
            repo,
            oids,
            (!commits.is_empty()).then_some(commits.as_slice()),
            &commit_rows,
            &line_counts,
            display,
            head_branch_ref,
            now,
            &mut abbrev_batch,
            &mut graph_for_text,
        )?
    } else {
        Vec::new()
    };

    Ok(LogOutcome {
        commits,
        groups: None,
        next,
        truncated,
        commit_rows,
        author_group_rows: Vec::new(),
        display,
        head_branch,
        oneline_lines,
        graph_lines,
        graph_oneline_stream: None,
        graph_note,
        continuation_args,
        line_counts,
        resume: None,
        format,
        commit_diffs,
        patch_text: None,
    })
}

/// Lines each commit added and removed against its first parent, limited to `path_args`.
///
/// Merges get `None`, as in `git log --shortstat`, and so do commits that change no lines.
fn commit_line_counts(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    path_args: &[String],
) -> Result<Vec<Option<ui::LineCounts>>> {
    let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
    let config = repo.config().snapshot().context("could not load config")?;
    let totals = repo
        .diff()
        .commit_line_totals(
            oids,
            &pathspecs,
            DiffOptions::from_config(config.as_ref()),
            config.as_ref(),
            &repo.thread_policy(),
        )
        .context("could not diff commits")?;
    Ok(totals
        .into_iter()
        .map(|totals| {
            totals
                .filter(|t| t.insertions + t.deletions > 0)
                .map(|t| ui::LineCounts {
                    added: t.insertions,
                    removed: t.deletions,
                })
        })
        .collect())
}

/// What `grit log` draws on a terminal.
pub enum TerminalLog {
    /// One listing (`-n` or `--cursor` was given).
    Listing(Box<LogOutcome>),
    /// The whole history, a batch at a time.
    Stream(Box<LogStream>),
}

impl TerminalLog {
    /// Draw it as human output to `w`.
    ///
    /// # Errors
    ///
    /// Returns render, walk and write errors (a reader that hangs up mid-stream is not one).
    pub fn render(self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Listing(outcome) => outcome.render_human(w, ctx),
            Self::Stream(stream) => stream.render(w, ctx),
        }
    }
}

/// How many commits [`LogStream`] draws before walking further, so the first screen is quick.
const FIRST_BATCH: usize = 100;

/// How many commits each later [`LogStream`] batch walks.
const BATCH: usize = 1000;

/// The whole history for a terminal, walked and drawn a batch at a time.
///
/// The first batch is walked by [`Self::start`], so errors show before a pager opens.
/// [`Self::render`] draws it and keeps walking until history ends or the reader hangs up.
/// Walks that cannot resume from a cursor (path limits, commit filters, `--graph`) are
/// walked in full as one batch.
pub struct LogStream {
    repo: grit_lib::repo::Repository,
    batch: LogOutcome,
    format: DiffFormatArgs,
    display: LogDisplayOptions,
}

impl LogStream {
    /// Walk the first batch of the commits `rev` and `pathspecs` select.
    ///
    /// # Errors
    ///
    /// Returns the selection and walk errors [`run`] does.
    pub fn start(
        rev: RevSelectArgs,
        pathspecs: TrailingPathspecArgs,
        format: DiffFormatArgs,
        display: LogDisplayOptions,
    ) -> Result<Self> {
        let batch = run(
            rev,
            pathspecs,
            None,
            format,
            display,
            LogEmitKind::Human,
            LogPage::Batches(FIRST_BATCH),
        )?;
        Ok(Self {
            repo: context::discover()?,
            batch,
            format,
            display,
        })
    }

    /// Draw every batch to `w`, stopping early without error when `w`'s reader hangs up.
    ///
    /// # Errors
    ///
    /// Returns walk, diff and write errors other than a closed pipe.
    pub fn render(mut self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        // Every batch keeps the first batch's column widths, so the rows line up.
        let columns =
            ui::CommitColumns::for_stream(&self.batch.commit_rows, ctx.now.unix_timestamp());
        loop {
            self.batch.display.columns = Some(columns);
            let resume = self.batch.resume.take();
            let mut buf = Vec::new();
            self.batch.render_human(&mut buf, ctx)?;
            match w.write_all(&buf).and_then(|()| w.flush()) {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                result => result.context("writing output")?,
            }
            let Some((query, cursor)) = resume else {
                return Ok(());
            };
            let walked = LogWalkCursor::step(&self.repo, &query, Some(&cursor), BATCH)
                .context("could not list commits")?;
            if walked.commits.is_empty() {
                return Ok(());
            }
            let next = walked.next.map(|cursor| (query, cursor));
            self.batch = build_commit_outcome(
                &self.repo,
                &walked.commits,
                walked.commit_data,
                self.format,
                &[],
                None,
                false,
                self.display,
                LogEmitKind::Human,
                None,
                LogGraphBuild::default(),
            )?;
            self.batch.resume = next;
        }
    }
}

/// Lay out the graph for `oids` and return each commit's block of rendered lines.
fn graph_blocks(
    oids: &[ObjectId],
    parent_map: &HashMap<ObjectId, Vec<ObjectId>>,
    display: LogDisplayOptions,
) -> Vec<Vec<GraphRenderedLine>> {
    let charset = if display.graph_ascii {
        GraphCharset::ASCII
    } else {
        GraphCharset::UNICODE
    };
    let inputs: Vec<GraphCommitInput> = oids
        .iter()
        .map(|&id| GraphCommitInput {
            id,
            parents: parent_map.get(&id).cloned().unwrap_or_default(),
        })
        .collect();
    GraphLayout::new_without_topology_recording(charset).render(&inputs)
}

/// Columns the commit line's graph prefix takes in each block.
fn commit_prefix_widths(blocks: &[Vec<GraphRenderedLine>]) -> Vec<usize> {
    blocks
        .iter()
        .map(|block| {
            block
                .iter()
                .find(|line| line.kind != GraphLineKind::Padding)
                .map_or(0, |line| ui::display_width(&line.prefix))
        })
        .collect()
}

/// Join each block's graph prefixes to its commit text.
fn join_graph_blocks(blocks: Vec<Vec<GraphRenderedLine>>, texts: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(blocks.len() * 2);
    for (block, text) in blocks.into_iter().zip(texts) {
        for graph_line in block {
            if graph_line.kind == GraphLineKind::Padding {
                out.push(graph_line.prefix.trim_end().to_owned());
            } else {
                out.push(format!("{}{text}", graph_line.prefix));
            }
        }
    }
    out
}

fn build_graph_oneline_stream(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    display: LogDisplayOptions,
    mut graph: LogGraphBuild,
) -> Result<GraphOnelineStream> {
    let (parent_map, subjects) = if let Some(linkage) = graph.linkage.take() {
        (linkage.parents, linkage.subjects)
    } else {
        repo.revwalk()
            .graph_materialize(oids, graph.parent_rewrite)?
    };
    let charset = if display.graph_ascii {
        GraphCharset::ASCII
    } else {
        GraphCharset::UNICODE
    };
    Ok(GraphOnelineStream {
        oids: oids.to_vec(),
        parent_map,
        subjects,
        layout: GraphLayout::new_without_topology_recording(charset),
        next_commit: 0,
        pending_block: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_graph_text_lines(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    commits: Option<&[LogCommit]>,
    summaries: &[CommitSummary],
    line_counts: &[Option<ui::LineCounts>],
    display: LogDisplayOptions,
    head_branch: Option<&str>,
    now: i64,
    abbrev_batch: &mut OidAbbrevBatch,
    graph: &mut LogGraphBuild,
) -> Result<Vec<String>> {
    let visible: HashSet<ObjectId> = oids.iter().copied().collect();
    let parent_map = if let Some(linkage) = graph.linkage.take() {
        linkage.parents
    } else {
        repo.revwalk()
            .graph_materialize(
                &visible.iter().copied().collect::<Vec<_>>(),
                graph.parent_rewrite,
            )
            .map(|(parents, _)| parents)?
    };
    let blocks = graph_blocks(oids, &parent_map, display);
    let leads = commit_prefix_widths(&blocks);
    let texts = if display.oneline {
        let term_width = ui::terminal_width();
        summaries
            .iter()
            .zip(&leads)
            .enumerate()
            .map(|(i, (summary, lead))| {
                let text = if display.decorate {
                    let commit = commits
                        .and_then(|c| c.get(i))
                        .ok_or_else(|| anyhow::anyhow!("internal error: missing log commit row"))?;
                    render_oneline_line(repo, abbrev_batch, &commit.commit, true, head_branch)?
                } else {
                    render_oneline_oid_subject(repo, summary.oid, &summary.subject, abbrev_batch)?
                };
                Ok(ui::fit_width(
                    &text,
                    term_width.map(|w| w.saturating_sub(*lead)),
                ))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        let refs = (0..summaries.len())
            .map(|i| {
                if !display.decorate {
                    return Ok(None);
                }
                commits
                    .and_then(|c| c.get(i))
                    .map(|commit| Some(commit.commit.refs.as_slice()))
                    .ok_or_else(|| anyhow::anyhow!("internal error: missing log commit row"))
            })
            .collect::<Result<Vec<_>>>()?;
        // Decorations go in after the row is cut to width, so leave room for them too.
        let reserved: Vec<usize> = leads
            .iter()
            .zip(&refs)
            .map(|(lead, refs)| {
                let deco = refs.map_or(0, |refs| {
                    ui::display_width(&format_decorations_folded(refs, head_branch))
                });
                if deco == 0 {
                    *lead
                } else {
                    lead + deco + 1
                }
            })
            .collect();
        let sha_abbrevs = crate::log_display::abbrev_commit_summaries(repo, summaries)?;
        let opts = ui::CommitRowOptions {
            reserved: &reserved,
            line_counts,
            columns: display.columns,
            sha_abbrevs: Some(&sha_abbrevs),
        };
        ui::commit_rows_with(summaries, now, &opts)
            .into_iter()
            .zip(&refs)
            .map(|(row, refs)| match refs {
                Some(refs) => decorate_commit_row(&row, refs, head_branch),
                None => row,
            })
            .collect()
    };
    Ok(join_graph_blocks(blocks, texts))
}

mod dispatch;
pub use dispatch::dispatch;
