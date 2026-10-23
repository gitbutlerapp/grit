//! `grit log` — the recent history reachable from HEAD.
//!
//! Deliberately minimal: it shows one page of commits (newest first) and, when
//! there's more, prints the command to fetch the next page.

use anyhow::{bail, Context, Result};
use grit_lib::diff::{CommitFilesManyInput, CommitFilesManyOptions, DiffOptions};
use grit_lib::objects::{CommitData, ObjectId, OidHashMap};
use grit_lib::revwalk::graph::{
    display_parents_for_graph, log_graph_materialize, GraphCharset, GraphCommitInput, GraphLayout,
    GraphLineKind, GraphRenderedLine,
};
use grit_lib::revwalk::group_by::{
    group_commits_by_author, group_commits_by_author_from_rev, mailmap_for_grouping,
    GroupByAuthorOptions,
};
use grit_lib::revwalk::log_walk::{LogWalkCursor, LogWalkQuery};
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
    render_oneline_from_summary, render_oneline_line, render_oneline_oid_subject,
};
use crate::markdown;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;
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
    pub graph: bool,
    pub graph_unicode: bool,
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
    graph_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    graph_note: Option<&'static str>,
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
        if self.commits.is_empty() && !self.display.graph {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        let now = ctx.now.unix_timestamp();
        let color = use_color();
        let head_branch = self.head_branch.as_deref();
        if self.display.graph {
            if self.graph_lines.is_empty() {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            self.render_graph_human(w)?;
        } else if self.display.oneline {
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
        if self.commit_rows.is_empty() && !self.display.graph {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        if let Some(note) = self.graph_note {
            outln!(w, "_{note}_");
            outln!(w);
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
    pathspecs: TrailingPathspecArgs,
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
        if display.graph {
            bail!("`grit log` cannot combine `--graph` with `--group-by`");
        }
    }
    if display.graph {
        if format.any_format_flag() {
            bail!("`grit log` cannot combine diff format flags with `--graph`");
        }
        if output_mode == OutputMode::Json {
            return Err(crate::json_error::incompatible_output_flags(
                "`grit log --graph` is for human output only; omit `--graph` when using `--json`",
                "incompatible_flags",
            ));
        }
    }
    let repo = context::discover()?;
    let env = repo.environment();
    let walk_paths = pathspecs.rev_walk_tokens(&repo)?;
    let path_args = walk_paths.as_slice();
    let opts = rev.to_rev_walk_options(&repo, env, &["HEAD"], path_args)?;
    let continuation = cursor;
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
        let resume = LogWalkCursor::decode(token).context("invalid log continuation cursor")?;
        let query = resume.query().clone();
        let page_size = rev.max_count.unwrap_or(PAGE);
        let page = LogWalkCursor::step(&repo, &query, Some(&resume), page_size)
            .context("could not list commits")?;
        let truncated = page.next.is_some();
        let next = page.next.map(|c| c.encode()).transpose()?;
        return build_commit_outcome(
            &repo,
            &page.commits,
            &mut OidHashMap::default(),
            format,
            path_args,
            next,
            truncated,
            display,
            output_mode,
            None,
            LogGraphBuild::default(),
        );
    }

    let token_refs = rev.rev_tokens(&["HEAD"]);
    let page_size = rev.max_count.unwrap_or(PAGE);

    let mut walk_opts = opts.clone();
    walk_opts.graph = display.graph;

    let use_rev_walk = display.graph
        || display.group_by.is_some()
        || pathspecs.has_any()
        || rev.max_count.is_some()
        || walk_opts
            .commit_filter
            .as_ref()
            .is_some_and(|f| f.is_active())
        || walk_opts.pickaxe.is_some()
        || LogWalkQuery::from_rev_walk(&repo, &token_refs, &walk_opts)?.is_none();

    if use_rev_walk {
        if rev.max_count == Some(0) {
            return empty_outcome(display);
        }
        let walk = repo.rev_walk();
        let mut fetch_opts = walk_opts.clone();
        let page_limit = if display.graph && rev.max_count.is_none() {
            Some(PAGE)
        } else {
            rev.max_count
        };
        fetch_opts.limit = fetch_limit_plus_one(page_limit);
        fetch_opts.retain_commit_data = !display.graph && display.group_by.is_none();
        let mut list_result = walk
            .rev_list_result(&token_refs, &fetch_opts)
            .context("could not list commits")?;
        let mut parsed = std::mem::take(&mut list_result.commit_data);
        let graph_linkage = list_result.log_graph_linkage.clone();
        let (oids, truncated) = truncate_oid_list(list_result.commits, page_limit);
        if display.group_by == Some(LogGroupBy::Author) {
            let use_mailmap = rev_select::log_mailmap_enabled(&repo, rev.no_mailmap);
            let mailmap = mailmap_for_grouping(&repo, use_mailmap)?;
            if display.group_count_only {
                if rev.max_count.is_none() {
                    let grouped = group_commits_by_author_from_rev(
                        &repo,
                        &token_refs,
                        &fetch_opts,
                        mailmap.as_ref(),
                        GroupByAuthorOptions { count_only: true },
                    )
                    .context("could not group commits by author")?;
                    return build_author_group_outcome_from_groups(&repo, grouped, false, display);
                }
                let oids = repo
                    .rev_walk()
                    .commits(&token_refs, &fetch_opts)
                    .context("could not list commits")?;
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
        return build_commit_outcome(
            &repo,
            &oids,
            &mut parsed,
            format,
            path_args,
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
    }

    let query = LogWalkQuery::from_rev_walk(&repo, &token_refs, &opts)?
        .context("internal error: log walk query missing")?;

    let page =
        LogWalkCursor::step(&repo, &query, None, page_size).context("could not list commits")?;
    let truncated = page.next.is_some();
    let next = page.next.map(|c| c.encode()).transpose()?;
    build_commit_outcome(
        &repo,
        &page.commits,
        &mut OidHashMap::default(),
        format,
        path_args,
        next,
        truncated,
        display,
        output_mode,
        None,
        LogGraphBuild::default(),
    )
}

#[derive(Clone, Default)]
struct LogGraphBuild {
    parent_rewrite: bool,
    linkage: Option<grit_lib::revwalk::rev_list::LogGraphLinkage>,
}

fn graph_needs_parent_rewrite(
    has_pathspecs: bool,
    walk: &grit_lib::revwalk::RevWalkListOptions,
) -> bool {
    has_pathspecs || walk.commit_filter.as_ref().is_some_and(|f| f.is_active())
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
        graph_lines: Vec::new(),
        graph_note: None,
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
        graph_lines: Vec::new(),
        graph_note: None,
        continuation_args: None,
        format: DiffFormatArgs::default(),
        commit_diffs: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_commit_outcome(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    parsed: &mut OidHashMap<CommitData>,
    format: DiffFormatArgs,
    path_args: &[String],
    next: Option<String>,
    truncated: bool,
    display: LogDisplayOptions,
    output_mode: OutputMode,
    continuation_args: Option<String>,
    graph: LogGraphBuild,
) -> Result<LogOutcome> {
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let json_output = output_mode == OutputMode::Json;
    let graph_human_oneline = display.graph
        && output_mode == OutputMode::Human
        && display.oneline
        && !format.any_format_flag()
        && !display.decorate;
    let need_refs = display.decorate || json_output;
    let head_branch = if display.oneline || display.decorate {
        resolve_current_branch(repo)?
    } else {
        None
    };

    if graph_human_oneline {
        let graph_lines = build_graph_oneline_lines(repo, oids, display, &graph)?;
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
            graph_lines,
            graph_note: None,
            continuation_args,
            format,
            commit_diffs: Vec::new(),
        });
    }

    let detail = format.detail_for_log();

    let (commit_json_rows, commit_rows) = if need_refs {
        CommitJsonLoader::for_listed_commits()
            .load_with_summaries_reusing(repo, oids, now, parsed)?
    } else {
        let summaries = CommitJsonLoader::load_summaries_only(repo, oids, parsed)?;
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
    let oneline_lines = if display.oneline && !display.graph {
        commits
            .iter()
            .map(|c| render_oneline_line(repo, &c.commit, display.decorate, head_branch_ref))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    let graph_note = if display.graph && output_mode == OutputMode::Markdown {
        Some("`--graph` affects human output only; this listing omits graph columns.")
    } else {
        None
    };

    let graph_lines = if display.graph && output_mode == OutputMode::Human {
        build_graph_text_lines(
            repo,
            oids,
            Some(&commits),
            &commit_rows,
            display,
            head_branch_ref,
            now,
            &graph,
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
        graph_note,
        continuation_args,
        format,
        commit_diffs,
    })
}

impl LogOutcome {
    fn render_graph_human(&self, w: &mut dyn Write) -> Result<()> {
        let mut buf = String::new();
        for (i, line) in self.graph_lines.iter().enumerate() {
            if i > 0 {
                buf.push('\n');
            }
            buf.push_str(line);
        }
        if !buf.is_empty() {
            buf.push('\n');
            w.write_all(buf.as_bytes())?;
        }
        Ok(())
    }
}

fn build_graph_oneline_lines(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    display: LogDisplayOptions,
    graph: &LogGraphBuild,
) -> Result<Vec<String>> {
    let charset = if display.graph_unicode {
        GraphCharset::UNICODE
    } else {
        GraphCharset::ASCII
    };
    let (parent_map, subjects) = if let Some(linkage) = graph.linkage.clone() {
        (linkage.parents, linkage.subjects)
    } else {
        log_graph_materialize(repo, oids, graph.parent_rewrite)?
    };
    let inputs: Vec<GraphCommitInput> = oids
        .iter()
        .map(|&id| GraphCommitInput {
            id,
            parents: parent_map.get(&id).cloned().unwrap_or_default(),
        })
        .collect();
    let mut layout = GraphLayout::new(charset);
    let blocks = layout.render(&inputs);
    let mut out = Vec::with_capacity(oids.len() + blocks.len());
    let mut prefix_cache = HashMap::new();
    for (block, &oid) in blocks.into_iter().zip(oids.iter()) {
        let subject = subjects
            .get(&oid)
            .map(String::as_str)
            .unwrap_or("(no subject)");
        for graph_line in block {
            if graph_line.kind == GraphLineKind::Padding {
                out.push(graph_line.prefix.trim_end().to_owned());
                continue;
            }
            let text = render_oneline_oid_subject(repo, oid, subject, &mut prefix_cache)?;
            out.push(format!("{}{text}", graph_line.prefix));
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn build_graph_text_lines(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    commits: Option<&[LogCommit]>,
    summaries: &[CommitSummary],
    display: LogDisplayOptions,
    head_branch: Option<&str>,
    now: i64,
    graph: &LogGraphBuild,
) -> Result<Vec<String>> {
    let charset = if display.graph_unicode {
        GraphCharset::UNICODE
    } else {
        GraphCharset::ASCII
    };
    let visible: HashSet<ObjectId> = oids.iter().copied().collect();
    let parent_map = if let Some(linkage) = graph.linkage.as_ref() {
        linkage.parents.clone()
    } else {
        display_parents_for_graph(repo, &visible, graph.parent_rewrite)?
    };
    let inputs: Vec<GraphCommitInput> = oids
        .iter()
        .map(|&id| GraphCommitInput {
            id,
            parents: parent_map.get(&id).cloned().unwrap_or_default(),
        })
        .collect();
    let mut layout = GraphLayout::new(charset);
    let blocks = layout.render(&inputs);
    let mut out = Vec::with_capacity(oids.len() + blocks.len());
    if let Some(commits) = commits {
        for (block, (commit, summary)) in blocks.into_iter().zip(commits.iter().zip(summaries)) {
            append_graph_block_lines(
                repo,
                &mut out,
                block,
                summary,
                Some(commit),
                display,
                head_branch,
                now,
            )?;
        }
    } else {
        for (block, summary) in blocks.into_iter().zip(summaries) {
            append_graph_block_lines(
                repo,
                &mut out,
                block,
                summary,
                None,
                display,
                head_branch,
                now,
            )?;
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn append_graph_block_lines(
    repo: &grit_lib::repo::Repository,
    out: &mut Vec<String>,
    block: Vec<GraphRenderedLine>,
    summary: &CommitSummary,
    commit: Option<&LogCommit>,
    display: LogDisplayOptions,
    head_branch: Option<&str>,
    now: i64,
) -> Result<()> {
    for graph_line in block {
        if graph_line.kind == GraphLineKind::Padding {
            out.push(graph_line.prefix.trim_end().to_owned());
            continue;
        }
        let text = if display.oneline {
            if display.decorate {
                let commit = commit
                    .ok_or_else(|| anyhow::anyhow!("internal error: missing log commit row"))?;
                render_oneline_line(repo, &commit.commit, true, head_branch)?
            } else {
                render_oneline_from_summary(repo, summary)?
            }
        } else {
            let commit =
                commit.ok_or_else(|| anyhow::anyhow!("internal error: missing log commit row"))?;
            let row = ui::commit_rows(std::slice::from_ref(summary), now)[0].clone();
            if display.decorate {
                decorate_commit_row(&row, &commit.commit.refs, head_branch)
            } else {
                row
            }
        };
        out.push(format!("{}{text}", graph_line.prefix));
    }
    Ok(())
}
