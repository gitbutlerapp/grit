//! `grit log` human and Markdown rendering for [`LogOutcome`].

use std::io::Write;

use anyhow::Result;

use crate::commands::diff::render_log_commit_diff;
use crate::commands::log::{continuation_hint, LogOutcome};
use crate::context;
use crate::diff_render::{render_stat, use_color};
use crate::log_display::{
    decorate_commit_row, format_decorations_folded, render_author_groups_human,
    render_author_groups_markdown,
};
use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui;

fn render_continuation(outcome: &LogOutcome, w: &mut dyn Write) -> Result<()> {
    let Some(hint) = continuation_hint(outcome) else {
        return Ok(());
    };
    outln!(w);
    outln!(w, "→ more: {hint}");
    Ok(())
}

impl HumanRender for LogOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(params) = &self.patch_text {
            let repo = context::discover()?;
            crate::commands::diff::emit_commits_log_patch_text(
                &repo,
                &params.oids,
                self.format,
                &params.pathspec,
                w,
            )?;
            return render_continuation(self, w);
        }
        if self.display.group_by.is_some() {
            if self.author_group_rows.is_empty() && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            if self.author_group_rows.is_empty() {
                return Ok(());
            }
            return render_author_groups_human(w, &self.author_group_rows);
        }
        if !self.display.graph {
            let empty = if self.display.oneline {
                self.oneline_lines.is_empty()
            } else {
                self.commit_rows.is_empty()
            };
            if empty && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            if empty {
                return Ok(());
            }
        }
        let now = ctx.now.unix_timestamp();
        let color = use_color();
        let head_branch = self.head_branch.as_deref();
        if self.display.graph {
            if self.graph_lines.is_empty() && self.graph_oneline_stream.is_none() {
                if self.display.no_commits_in_repo {
                    outln!(w, "No commits yet.");
                }
                return Ok(());
            }
            render_graph_human(self, w)?;
        } else if self.display.oneline {
            for line in &self.oneline_lines {
                outln!(w, "{line}");
            }
        } else {
            let reserved: Vec<usize> = if self.display.decorate {
                self.commits
                    .iter()
                    .map(|c| {
                        match ui::display_width(&format_decorations_folded(
                            &c.commit.refs,
                            head_branch,
                        )) {
                            0 => 0,
                            n => n + 1,
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let repo = crate::context::discover()?;
            let sha_abbrevs =
                crate::log_display::abbrev_commit_summaries(&repo, &self.commit_rows)?;
            let opts = ui::CommitRowOptions {
                reserved: &reserved,
                line_counts: &self.line_counts,
                columns: self.display.columns,
                sha_abbrevs: Some(&sha_abbrevs),
            };
            for (i, row) in ui::commit_rows_with(&self.commit_rows, now, &opts)
                .iter()
                .enumerate()
            {
                let commit = self.commits.get(i);
                let diff = self.commit_diffs.get(i).and_then(Option::as_ref);
                let stat = commit.and_then(|c| c.stat.as_ref());
                match commit {
                    Some(commit) if self.display.decorate => outln!(
                        w,
                        "{}",
                        decorate_commit_row(row, &commit.commit.refs, head_branch)
                    ),
                    _ => outln!(w, "{row}"),
                }
                if let Some(outcome) = diff {
                    render_log_commit_diff(w, outcome)?;
                } else if let Some(stat) = stat {
                    render_stat(w, stat, color, None)?;
                }
                if diff.is_some() || stat.is_some() {
                    outln!(w);
                }
            }
        }
        render_continuation(self, w)
    }
}

impl MarkdownRender for LogOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.display.group_by.is_some() {
            if self.author_group_rows.is_empty() && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            if self.author_group_rows.is_empty() {
                return Ok(());
            }
            return render_author_groups_markdown(w, &self.author_group_rows);
        }
        if !self.display.graph {
            let empty = if self.display.oneline {
                self.oneline_lines.is_empty()
            } else {
                self.commit_rows.is_empty()
            };
            if empty && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            if empty {
                return Ok(());
            }
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
            let repo = crate::context::discover()?;
            let sha_abbrevs =
                crate::log_display::abbrev_commit_summaries(&repo, &self.commit_rows)?;
            let now = ctx.now.unix_timestamp();
            let head_branch = self.head_branch.as_deref();
            for ((summary, short), commit) in
                self.commit_rows.iter().zip(&sha_abbrevs).zip(&self.commits)
            {
                outln!(
                    w,
                    "{}",
                    markdown::commit_bullet_with_decorations(
                        summary,
                        short,
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
            render_commit_diffs_markdown(w, &self.commit_rows, &self.commit_diffs, ctx)?;
        }
        match (continuation_hint(self), &self.next) {
            (Some(cmd), Some(_)) => {
                outln!(w);
                outln!(w, "More history: run `{cmd}`");
            }
            (Some(hint), None) => {
                outln!(w);
                outln!(w, "More history: {hint}");
            }
            (None, _) => {}
        }
        Ok(())
    }
}

/// Per-commit diff sections after a commit list in Markdown (log, shortlog, …).
pub(crate) fn render_commit_diffs_markdown(
    w: &mut dyn Write,
    rows: &[crate::context::CommitSummary],
    diffs: &[Option<crate::commands::diff::DiffOutcome>],
    ctx: &RenderContext,
) -> Result<()> {
    use crate::render::MarkdownRender;
    for (row, diff) in rows.iter().zip(diffs) {
        let Some(outcome) = diff else {
            continue;
        };
        if outcome.files.is_empty() {
            continue;
        }
        outln!(w);
        outln!(w, "### `{}`", row.oid.to_hex());
        outcome.render_markdown(w, ctx)?;
    }
    Ok(())
}

pub(crate) fn render_graph_human(outcome: &LogOutcome, w: &mut dyn Write) -> Result<()> {
    if let Some(stream_cell) = &outcome.graph_oneline_stream {
        let repo = context::discover()?;
        let mut abbrev = crate::log_display::display_abbrev_batch(&repo)?;
        let mut stream = stream_cell.borrow_mut();
        while let Some(line) = stream.next_line(&repo, &mut abbrev)? {
            outln!(w, "{line}");
        }
        return Ok(());
    }
    let mut buf = String::new();
    for line in &outcome.graph_lines {
        buf.push_str(line);
        buf.push('\n');
    }
    crate::stdio::io_result(w.write_all(buf.as_bytes()))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))
}
