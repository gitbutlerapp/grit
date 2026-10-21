//! `grit log` — the recent history reachable from HEAD.
//!
//! Deliberately minimal: it shows one page of commits (newest first) and, when
//! there's more, prints the command to fetch the next page.

use anyhow::{bail, Context, Result};
use grit_lib::diff::{CommitFilesManyInput, CommitFilesManyOptions, DiffOptions};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::group_by::{
    group_commits_by_author, mailmap_for_grouping, GroupByAuthorOptions,
};
use grit_lib::revwalk::log_walk::{LogWalkCursor, LogWalkQuery};
use grit_lib::revwalk::rev_list_commits_for_author_group;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::diff::{
    cli_outcome_from_lib, diff_of_commit, render_log_commit_diff, resolve_commit_diff_pathspecs,
    DiffOutcome, DiffTotalsJson, FileDiff,
};
use crate::commands::show::{diffstat, DiffStat};
use crate::commits_json::{fetch_limit_plus_one, truncate_oid_list, CommitJsonLoader};
use crate::context::{self, CommitSummary};
use crate::diff_format::DiffFormatArgs;
use crate::diff_render::{render_stat, use_color};
use crate::log_display::{
    decorate_commit_row, render_author_groups_human, render_author_groups_markdown,
    render_oneline_line,
};
use crate::markdown;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::rev_select::{self, RevSelectArgs};
use crate::ui;

/// How many commits to show per page when the caller does not set `-n` / `--max-count`.
const PAGE: usize = 10;

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
    commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    author_group_rows: Vec<(usize, String, String)>,
    #[serde(skip)]
    #[schemars(skip)]
    display: LogDisplayOptions,
    #[serde(skip)]
    #[schemars(skip)]
    head_branch: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    oneline_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    continuation_args: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    format: DiffFormatArgs,
    #[serde(skip)]
    #[schemars(skip)]
    commit_diffs: Vec<Option<DiffOutcome>>,
}

fn format_log_cursor_command(rev_args: Option<&str>, cursor: &str) -> String {
    match rev_args.filter(|s| !s.is_empty()) {
        Some(revs) => format!("grit log {revs} --cursor={cursor}"),
        None => format!("grit log --cursor={cursor}"),
    }
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

impl HumanRender for LogOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.display.group_by.is_some() {
            if self.author_group_rows.is_empty() {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            return render_author_groups_human(w, &self.author_group_rows);
        }
        if self.commits.is_empty() {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        let now = ctx.now.unix_timestamp();
        let color = use_color();
        let head_branch = self.head_branch.as_deref();
        if self.display.oneline {
            for line in &self.oneline_lines {
                outln!(w, "{line}");
            }
        } else {
            for (row, (commit, diff)) in ui::commit_rows(&self.commit_rows, now)
                .iter()
                .zip(self.commits.iter().zip(&self.commit_diffs))
            {
                let row = if self.display.decorate {
                    decorate_commit_row(row, &commit.commit.refs, head_branch)
                } else {
                    row.clone()
                };
                outln!(w, "{row}");
                if let Some(outcome) = diff {
                    render_log_commit_diff(w, outcome)?;
                } else if let Some(stat) = &commit.stat {
                    render_stat(w, stat, color, None)?;
                }
                if diff.is_some() || commit.stat.is_some() {
                    outln!(w);
                }
            }
        }
        if let Some(next) = &self.next {
            outln!(w);
            outln!(
                w,
                "→ more: {}",
                format_log_cursor_command(self.continuation_args.as_deref(), next)
            );
        }
        Ok(())
    }
}

/// One commit in `grit log` JSON output.
#[derive(Serialize, JsonSchema)]
pub struct LogCommit {
    #[serde(flatten)]
    pub commit: CommitJson,
    /// Per-file diff detail when `-p`, `--stat`, `--name-only`, or `--name-status` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileDiff>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    /// The files the commit changed, only with `--stat` (same shape as [`grit show`](../show/)).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
}

impl MarkdownRender for LogOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.display.group_by.is_some() {
            if self.author_group_rows.is_empty() {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            return render_author_groups_markdown(w, &self.author_group_rows);
        }
        if self.commit_rows.is_empty() {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        if self.display.oneline {
            for line in &self.oneline_lines {
                outln!(w, "- `{line}`");
            }
        } else if self.display.decorate {
            let now = ctx.now.unix_timestamp();
            let head_branch = self.head_branch.as_deref();
            for (summary, commit) in self.commit_rows.iter().zip(&self.commits) {
                outln!(
                    w,
                    "{}",
                    markdown::commit_bullet_with_decorations(
                        summary,
                        &commit.commit.refs,
                        head_branch,
                        now,
                    )
                );
            }
        } else {
            markdown::print_commit_list(w, &self.commit_rows, ctx)?;
        }
        if self.format.any_format_flag() {
            for (row, diff) in self.commit_rows.iter().zip(&self.commit_diffs) {
                if let Some(outcome) = diff {
                    if outcome.files.is_empty() {
                        continue;
                    }
                    outln!(w);
                    let oid = row.oid.to_hex();
                    outln!(w, "### `{oid}`");
                    outcome.render_markdown(w, ctx)?;
                }
            }
        }
        if let Some(next) = &self.next {
            outln!(w);
            let cmd = format_log_cursor_command(self.continuation_args.as_deref(), next);
            outln!(w, "More history: run `{cmd}`");
        }
        Ok(())
    }
}

pub fn run(
    rev: RevSelectArgs,
    cursor: Option<String>,
    format: DiffFormatArgs,
    display: LogDisplayOptions,
    output_mode: OutputMode,
) -> Result<LogOutcome> {
    if display.group_by.is_some() {
        if format.any_format_flag() {
            bail!("`grit log` cannot combine diff format flags with `--group-by`");
        }
        if display.oneline {
            bail!("`grit log` cannot combine `--oneline` with `--group-by`");
        }
    }
    let repo = context::discover()?;
    let env = repo.environment();
    let continuation_args = rev.log_continuation_args();
    let mut deprecated_cursor = None;
    let mut deprecated_warning = false;
    let opts = rev.to_rev_walk_options(
        &repo,
        env,
        &["HEAD"],
        &mut deprecated_cursor,
        &mut deprecated_warning,
    )?;
    if deprecated_warning {
        eprintln!(
            "warning: passing a log cursor to `--before` is deprecated; use `--cursor` instead"
        );
    }
    let continuation = cursor.or(deprecated_cursor);
    if display.group_by.is_some() && continuation.is_some() {
        bail!("`grit log` cannot combine `--cursor` with `--group-by`");
    }
    if let Some(token) = continuation.as_deref() {
        if !rev.paths.is_empty() {
            bail!(
                "`grit log` cannot combine `--cursor` with path filters; drop the cursor or the paths"
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
        let resume = LogWalkCursor::decode(token).context("invalid log continuation cursor")?;
        let query = if rev.has_explicit_revision_selection() {
            let token_refs = rev.rev_tokens(&["HEAD"]);
            let expected =
                LogWalkQuery::from_rev_walk(&repo, &token_refs, &opts)?.ok_or_else(|| {
                    anyhow::anyhow!("cursor was created for a different revision selection")
                })?;
            if resume.query() != &expected {
                bail!("cursor was created for a different revision selection");
            }
            expected
        } else {
            resume.query().clone()
        };
        let page_size = rev.max_count.unwrap_or(PAGE);
        let page = LogWalkCursor::step(&repo, &query, Some(&resume), page_size)
            .context("could not list commits")?;
        let truncated = page.next.is_some();
        let next = page.next.map(|c| c.encode()).transpose()?;
        return build_commit_outcome(
            &repo,
            &page.commits,
            format,
            &rev.paths,
            next,
            truncated,
            display,
            output_mode,
            Some(continuation_args),
        );
    }

    let token_refs = rev.rev_tokens(&["HEAD"]);
    let page_size = rev.max_count.unwrap_or(PAGE);

    let use_rev_walk = display.group_by.is_some()
        || !rev.paths.is_empty()
        || rev.max_count.is_some()
        || opts.commit_filter.as_ref().is_some_and(|f| f.is_active())
        || LogWalkQuery::from_rev_walk(&repo, &token_refs, &opts)?.is_none();

    if use_rev_walk {
        if rev.max_count == Some(0) {
            return empty_outcome(display);
        }
        let mut fetch_opts = opts.clone();
        fetch_opts.limit = fetch_limit_plus_one(rev.max_count);
        if display.group_by == Some(LogGroupBy::Author) {
            let use_mailmap = rev_select::log_mailmap_enabled(&repo, rev.no_mailmap);
            let mailmap = mailmap_for_grouping(&repo, use_mailmap)?;
            if display.group_count_only {
                let oids = if rev.max_count.is_none() {
                    rev_list_commits_for_author_group(&repo, &token_refs, &fetch_opts)
                        .context("could not list commits")?
                } else {
                    repo.rev_walk()
                        .commits(&token_refs, &fetch_opts)
                        .context("could not list commits")?
                };
                let (oids, truncated) = truncate_oid_list(oids, rev.max_count);
                let grouped = group_commits_by_author(
                    &repo,
                    &oids,
                    mailmap.as_ref(),
                    GroupByAuthorOptions { count_only: true },
                )
                .context("could not group commits by author")?;
                return build_author_group_outcome_from_groups(&repo, grouped, truncated, display);
            }
            let oids = repo
                .rev_walk()
                .commits(&token_refs, &fetch_opts)
                .context("could not list commits")?;
            let (oids, truncated) = truncate_oid_list(oids, rev.max_count);
            return build_author_group_outcome(&repo, &oids, truncated, display, use_mailmap);
        }
        let oids = repo
            .rev_walk()
            .commits(&token_refs, &fetch_opts)
            .context("could not list commits")?;
        let (oids, truncated) = truncate_oid_list(oids, rev.max_count);
        return build_commit_outcome(
            &repo,
            &oids,
            format,
            &rev.paths,
            None,
            truncated,
            display,
            output_mode,
            None,
        );
    }

    let query = LogWalkQuery::from_rev_walk(&repo, &token_refs, &opts)?
        .context("internal error: log walk query missing")?;

    let page =
        LogWalkCursor::step(&repo, &query, None, page_size).context("could not list commits")?;
    let truncated = page.next.is_some();
    let next = page.next.map(|c| c.encode()).transpose()?;
    let continuation = next.is_some().then_some(continuation_args);
    build_commit_outcome(
        &repo,
        &page.commits,
        format,
        &rev.paths,
        next,
        truncated,
        display,
        output_mode,
        continuation,
    )
}

fn resolve_current_branch(repo: &grit_lib::repo::Repository) -> Result<Option<String>> {
    Ok(
        match resolve_head(repo.git_dir()).context("could not resolve HEAD")? {
            HeadState::Branch { short_name, .. } => Some(short_name),
            _ => None,
        },
    )
}

fn empty_outcome(display: LogDisplayOptions) -> Result<LogOutcome> {
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
        continuation_args: None,
        format: DiffFormatArgs::default(),
        commit_diffs: Vec::new(),
    })
}

fn build_author_group_outcome(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    truncated: bool,
    display: LogDisplayOptions,
    use_mailmap: bool,
) -> Result<LogOutcome> {
    let mailmap = mailmap_for_grouping(repo, use_mailmap)?;
    let grouped = group_commits_by_author(
        repo,
        oids,
        mailmap.as_ref(),
        GroupByAuthorOptions {
            count_only: display.group_count_only,
        },
    )?;
    build_author_group_outcome_from_groups(repo, grouped, truncated, display)
}

fn build_author_group_outcome_from_groups(
    repo: &grit_lib::repo::Repository,
    grouped: Vec<grit_lib::revwalk::AuthorGroup>,
    truncated: bool,
    display: LogDisplayOptions,
) -> Result<LogOutcome> {
    let include_commits = !display.group_count_only;
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
        continuation_args: None,
        format: DiffFormatArgs::default(),
        commit_diffs: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_commit_outcome(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    format: DiffFormatArgs,
    path_args: &[String],
    next: Option<String>,
    truncated: bool,
    display: LogDisplayOptions,
    output_mode: OutputMode,
    continuation_args: Option<String>,
) -> Result<LogOutcome> {
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let json_output = output_mode == OutputMode::Json;
    let need_refs = display.decorate || json_output;
    let head_branch = if display.oneline || display.decorate {
        resolve_current_branch(repo)?
    } else {
        None
    };
    let detail = format.detail_for_log();

    let (commit_json_rows, commit_rows) = if need_refs {
        let loader = CommitJsonLoader::new(repo)?;
        loader.load_with_summaries(repo, oids, now)?
    } else {
        let summaries = CommitJsonLoader::load_summaries_only(repo, oids)?;
        let json_rows = summaries
            .iter()
            .map(|s| CommitJson::from_summary(s, now))
            .collect();
        (json_rows, summaries)
    };

    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let pathspecs = if path_args.is_empty() {
        Vec::new()
    } else {
        resolve_commit_diff_pathspecs(repo, path_args)?
    };

    let mut commit_inputs = Vec::with_capacity(oids.len());
    if detail.is_some() {
        for oid in oids {
            let commit = context::read_commit(repo, oid)?;
            commit_inputs.push(CommitFilesManyInput {
                oid: *oid,
                parents: commit.parents.clone(),
                tree: commit.tree,
            });
        }
    }

    let many_results = if let Some(detail) = detail {
        let mut diff_options = DiffOptions::from_config(config.as_ref());
        format.apply_to_options(&mut diff_options, config.as_ref())?;
        diff_options.word_level_inline = false;
        let opts = CommitFilesManyOptions {
            diff_options,
            detail,
            skip_merge_patches: true,
        };
        Some((
            detail,
            repo.diff()
                .commit_files_many_work(
                    &commit_inputs,
                    &pathspecs,
                    opts,
                    config.as_ref(),
                    &thread_policy,
                )
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
            let stat = if format.stat && commit_inputs[i].parents.len() < 2 {
                Some(diffstat(&diff_of_commit(repo, &commit_inputs[i].oid)?))
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

    let head_branch_ref = head_branch.as_deref();
    let oneline_lines = if display.oneline {
        commits
            .iter()
            .map(|c| render_oneline_line(repo, &c.commit, display.decorate, head_branch_ref))
            .collect::<Result<Vec<_>>>()?
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
        continuation_args,
        format,
        commit_diffs,
    })
}
