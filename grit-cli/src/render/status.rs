//! `grit status` human and Markdown rendering helpers and [`StatusOutcome`] presenters.

use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::operation::{ConflictKind, ConflictKindPhrase};
use grit_lib::porcelain::status::{StatusAction, StatusGuidance};
use grit_lib::refs::branch_tracking::StatusHeader;
use grit_lib::refs::branch_tracking::{
    Standing, StandingHeader, StatusIntegrationAction, StatusIntegrationHint,
};
use grit_lib::revwalk::AbbrevLength;
use grit_lib::state::NextStep;

use crate::commands::status::{
    GitButlerWorkspaceJson, StatusOutcome, StatusSharedJson, WorkspaceHeadJson, SHORTLOG_LIMIT,
};
use crate::context::{self};
use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::{self, PathDisplayOptions};

/// Human-readable heading for a grouped conflict section.
#[must_use]
pub fn conflict_kind_heading(phrase: ConflictKindPhrase) -> &'static str {
    match phrase {
        ConflictKindPhrase::BothModified => "both modified",
        ConflictKindPhrase::BothAdded => "both added",
        ConflictKindPhrase::BothDeleted => "both deleted",
        ConflictKindPhrase::AddedByUs => "added by us",
        ConflictKindPhrase::AddedByThem => "added by them",
        ConflictKindPhrase::DeletedByUs => "deleted by us",
        ConflictKindPhrase::DeletedByThem => "deleted by them",
        ConflictKindPhrase::RenameRename => "renamed differently",
        ConflictKindPhrase::DirectoryRenameMoved => "moved with its directory",
        ConflictKindPhrase::DirectoryRenameSplit => "directory split up",
        ConflictKindPhrase::DirectoryRenameBlocked => "directory move blocked",
        _ => "unmerged",
    }
}

impl HumanRender for StatusOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.short_format {
            self.render_short_human(w)?;
            return Ok(());
        }
        if let Some(outcome) = self.interrupted_rewrite {
            outln!(
                w,
                "{}",
                outcome.sentence(
                    ".git/index.lock",
                    self.interrupted_rewrite_lock_holder.as_ref()
                )
            );
            outln!(w);
        }
        self.render_header(w, ctx)?;
        if let Some(shared) = &self.shared {
            shared.render_human(w)?;
        }
        self.render_changes(w)?;
        self.render_hints(w)?;
        self.render_stash_footer(w)?;
        Ok(())
    }
}

impl MarkdownRender for StatusOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.short_format {
            self.render_short_human(w)?;
            return Ok(());
        }
        if let Some(outcome) = self.interrupted_rewrite {
            outln!(
                w,
                "> {}",
                outcome.sentence(
                    "`.git/index.lock`",
                    self.interrupted_rewrite_lock_holder.as_ref()
                )
            );
            outln!(w);
        }
        self.render_markdown_header(w, ctx)?;
        if let Some(shared) = &self.shared {
            shared.render_markdown(w)?;
        }
        self.render_markdown_changes(w)?;
        self.render_markdown_hints(w)?;
        self.render_stash_footer(w)?;
        Ok(())
    }
}

impl StatusSharedJson {
    pub(crate) fn render_human(&self, w: &mut dyn Write) -> Result<()> {
        if self.heads.is_empty() {
            outln!(w, "  shared; nobody else has pushed a head yet");
            return Ok(());
        }
        let names: Vec<&str> = self.heads.iter().map(|h| h.user.as_str()).collect();
        outln!(w, "  shared with {}", names.join(", "));
        let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
        for h in &self.heads {
            let hint = match h.relation {
                crate::render::shared_branch::HeadRelation::Ahead => "   grit sync",
                _ => "",
            };
            outln!(
                w,
                "    {:<width$}  {}  {}{hint}",
                h.user,
                grit_lib::objects::short_hex(&h.tip),
                h.relation.words()
            );
            if let Some(d) = &h.divergence {
                d.render_human(w, "      ")?;
            }
        }
        Ok(())
    }

    pub(crate) fn render_markdown(&self, w: &mut dyn Write) -> Result<()> {
        outln!(w);
        if self.heads.is_empty() {
            outln!(w, "Shared; nobody else has pushed a head yet.");
            return Ok(());
        }
        outln!(
            w,
            "Shared. The other members' heads, from `{}`:",
            self.remote
        );
        outln!(w);
        outln!(w, "| Member | Head | Compared with yours |");
        outln!(w, "| --- | --- | --- |");
        for h in &self.heads {
            outln!(
                w,
                "| `{}` | `{}` | {} |",
                h.user,
                grit_lib::objects::short_hex(&h.tip),
                h.relation.words()
            );
        }
        for h in &self.heads {
            if let Some(d) = &h.divergence {
                outln!(w);
                outln!(w, "`{}`'s head diverged from yours:", h.user);
                outln!(w);
                d.render_markdown(w, "")?;
            }
        }
        Ok(())
    }
}

impl StatusOutcome {
    pub(crate) fn render_short_human(&self, w: &mut dyn Write) -> Result<()> {
        for line in &self.short_lines {
            outln!(w, "{line}");
        }
        self.render_stash_footer(w)
    }

    pub(crate) fn render_stash_footer(&self, w: &mut dyn Write) -> Result<()> {
        if self.stash_count > 0 {
            let noun = if self.stash_count == 1 {
                "entry"
            } else {
                "entries"
            };
            outln!(w, "{} stash {noun}", self.stash_count);
        }
        Ok(())
    }

    fn ignored_paths(&self) -> &[String] {
        self.ignored.as_deref().unwrap_or(&[])
    }

    /// Trunk or upstream ref name used in the status header line.
    fn header_comparison_name(&self) -> &str {
        StatusHeader::primary_comparison_name(&self.standing).unwrap_or("")
    }

    pub(crate) fn render_upstream_line_human(&self, w: &mut dyn Write) -> Result<()> {
        if !self.show_counts {
            return Ok(());
        }
        let Some(up) = self.upstream.as_ref() else {
            return Ok(());
        };
        if self.target.is_none() {
            return Ok(());
        }
        if up.ahead.is_some() || up.behind.is_some() {
            outln!(
                w,
                "  ↑{} ↓{} {}",
                up.ahead.unwrap_or(0),
                up.behind.unwrap_or(0),
                up.r#ref
            );
        }
        Ok(())
    }

    pub(crate) fn render_upstream_line_markdown(&self, w: &mut dyn Write) -> Result<()> {
        if !self.show_counts {
            return Ok(());
        }
        let Some(up) = &self.upstream else {
            return Ok(());
        };
        if self.target.is_none() {
            return Ok(());
        }
        if up.ahead.is_some() || up.behind.is_some() {
            outln!(
                w,
                "↑**{}** ↓**{}** `{}`",
                up.ahead.unwrap_or(0),
                up.behind.unwrap_or(0),
                up.r#ref
            );
        }
        Ok(())
    }

    fn integration_update_hint(&self) -> Option<&StatusIntegrationHint> {
        self.integration_hint.as_ref()
    }

    pub(crate) fn render_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let base = self.header_comparison_name();
        let now = ctx.now.unix_timestamp();
        if let (Some(workspace), StatusHeader::Branch(_)) =
            (&self.gitbutler_workspace, &self.header)
        {
            return self.render_workspace_human(w, workspace, now);
        }
        match &self.header {
            StatusHeader::Branch(StandingHeader::Compared(Standing::Ahead)) => {
                outln!(
                    w,
                    "On {branch}  ·  {} ahead of {base}",
                    self.ahead.unwrap_or(0)
                );
                self.render_upstream_line_human(w)?;
                outln!(w);
                self.render_ahead_shortlog(w, now)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Behind)) => {
                outln!(
                    w,
                    "On {branch}  ·  {} behind {base}",
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_human(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Diverged)) => {
                outln!(
                    w,
                    "On {branch}  ·  {} ahead, {} behind {base}",
                    self.ahead.unwrap_or(0),
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_human(w)?;
                outln!(w);
                self.render_ahead_shortlog(w, now)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Even)) => {
                outln!(w, "On {branch}  ·  even with {base}");
                self.render_upstream_line_human(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) if base.is_empty() => {
                outln!(w, "On {branch}");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) => {
                outln!(w, "On {branch}  ·  {base}");
                self.render_upstream_line_human(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::NoTarget) => {
                outln!(w, "On {branch}");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Unborn) => {
                outln!(w, "On {branch} — no commits yet");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Detached) => {
                let short = self.head_short.as_deref().unwrap_or_default();
                outln!(w, "Detached at {short}");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Invalid) => {
                outln!(w, "HEAD is in an unknown state");
                outln!(w);
            }
            StatusHeader::MergeInProgress => {
                outln!(w, "On {branch}  ·  merging — resolve conflicts");
                outln!(w);
            }
            StatusHeader::RebaseInProgress { branch, onto } => {
                let branch = branch.as_deref().unwrap_or("detached HEAD");
                match onto {
                    Some(onto) => outln!(w, "Rebasing {branch} onto {onto}"),
                    None => outln!(w, "Rebasing {branch}"),
                }
                outln!(w);
            }
            StatusHeader::OtherInProgress => {
                outln!(w, "On {branch}  ·  operation in progress");
                outln!(w);
            }
        }
        Ok(())
    }

    pub(crate) fn render_changes(&self, w: &mut dyn Write) -> Result<()> {
        let display = self.path_display.as_ref();
        let ignored = self.ignored_paths();
        if self.clean {
            outln!(w, "Nothing to commit — working tree clean.");
            ui::print_ignored(w, ignored, display)?;
            return Ok(());
        }
        ui::print_change_group(w, "Staged", &self.staged_entries, display)?;
        ui::print_change_group(w, "Changed (not staged)", &self.unstaged_entries, display)?;
        if !self.conflict_groups.is_empty() {
            self.render_conflicts_by_kind(w, display)?;
        }
        ui::print_untracked(w, &self.untracked, display)?;
        crate::embedded_gitlink::emit_embedded_repository_status_notes_human(
            &self.embedded_repositories,
        );
        ui::print_ignored(w, ignored, display)?;
        self.render_worktree_case_collision_notice(w, crate::output::OutputMode::Human)?;
        Ok(())
    }

    fn render_worktree_case_collision_notice(
        &self,
        w: &mut dyn Write,
        mode: crate::output::OutputMode,
    ) -> Result<()> {
        if self.worktree_case_collision_paths.is_empty() {
            return Ok(());
        }
        outln!(w);
        crate::render::clone::render_worktree_case_collision_guidance(
            w,
            mode,
            &self.worktree_case_collision_paths,
        )
    }

    pub(crate) fn render_conflicts_by_kind(
        &self,
        w: &mut dyn Write,
        display: Option<&PathDisplayOptions>,
    ) -> Result<()> {
        outln!(w, "Conflicts");
        for group in &self.conflict_groups {
            outln!(w, "  {} ({})", kind_label(&group.kind), group.paths.len());
            for path in &group.paths {
                let path = display
                    .map(|d| d.format_path_for_human(path))
                    .unwrap_or_else(|| path.clone());
                outln!(w, "    {path}");
            }
        }
        outln!(w);
        Ok(())
    }

    pub(crate) fn render_hints(&self, w: &mut dyn Write) -> Result<()> {
        match &self.guidance {
            StatusGuidance::Operation { steps, alternative } => {
                let steps: Vec<_> = steps.iter().filter_map(NextStep::command).collect();
                outln!(w, "→ {}", steps.join("  ·  "));
                if let Some(alt) = alternative.as_ref().and_then(NextStep::command) {
                    outln!(w, "or {alt}");
                }
            }
            StatusGuidance::ResolveConflictsThenContinueMerge => {
                outln!(w, "→ resolve conflicts, then grit merge --continue");
            }
            StatusGuidance::ResolveConflictsThenStage => {
                outln!(
                    w,
                    "→ resolve conflicts, then grit add <file> to mark them resolved"
                );
            }
            StatusGuidance::ContinueMerge => {
                outln!(w, "→ grit merge --continue to finish the merge");
            }
            StatusGuidance::Actions(actions) if !actions.is_empty() => {
                let hints: Vec<String> = actions
                    .iter()
                    .filter_map(|action| match action {
                        StatusAction::DiscardWorktreeChanges => {
                            Some("grit restore <file> to discard worktree changes".to_owned())
                        }
                        StatusAction::Stage => Some("grit add <file> to stage".to_owned()),
                        StatusAction::Commit if self.gitbutler_workspace.is_some() => {
                            Some("but commit to commit (GitButler workspace)".to_owned())
                        }
                        StatusAction::Commit => {
                            Some("grit commit \"message\" to commit".to_owned())
                        }
                        StatusAction::Pull { .. } => self
                            .integration_update_hint()
                            .map(|hint| integration_hint_command(hint, false)),
                    })
                    .collect();
                if !hints.is_empty() {
                    outln!(w, "→ {}", hints.join("  ·  "));
                }
            }
            StatusGuidance::Actions(_) => {}
        }
        Ok(())
    }

    /// The first line of a workspace header: how many branches, and how far behind the target.
    fn workspace_summary(
        &self,
        workspace: &GitButlerWorkspaceJson,
        code: fn(&str) -> String,
    ) -> String {
        let count = workspace.heads.len();
        let mut line = format!(
            "{count} branch{} applied",
            if count == 1 { "" } else { "es" }
        );
        if let (Some(behind), Some(target)) = (self.behind.filter(|&n| n > 0), &self.target) {
            line.push_str(&format!("  ·  {behind} behind {}", code(target)));
        }
        line
    }

    pub(crate) fn render_workspace_human(
        &self,
        w: &mut dyn Write,
        workspace: &GitButlerWorkspaceJson,
        now: i64,
    ) -> Result<()> {
        outln!(
            w,
            "On GitButler workspace  ·  {}",
            self.workspace_summary(workspace, str::to_owned)
        );
        outln!(w);
        let names: Vec<String> = workspace.heads.iter().map(workspace_head_label).collect();
        let name_w = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        let reserved = vec![name_w + 2; names.len()];
        let repo = context::discover()?;
        let sha_abbrevs = crate::log_display::abbrev_commit_summaries(&repo, &self.workspace_rows)?;
        let opts = ui::CommitRowOptions {
            reserved: &reserved,
            sha_abbrevs: Some(&sha_abbrevs),
            ..ui::CommitRowOptions::default()
        };
        for (name, row) in names
            .iter()
            .zip(ui::commit_rows_with(&self.workspace_rows, now, &opts))
        {
            outln!(w, "  {name:<name_w$}{row}");
        }
        outln!(w);
        Ok(())
    }

    pub(crate) fn render_workspace_markdown(
        &self,
        w: &mut dyn Write,
        workspace: &GitButlerWorkspaceJson,
    ) -> Result<()> {
        let repo = context::discover()?;
        let code = |s: &str| format!("`{s}`");
        outln!(
            w,
            "On a **GitButler workspace** · {}",
            self.workspace_summary(workspace, |s| format!("`{s}`"))
                .replace("  ·  ", " · ")
        );
        outln!(w);
        for head in &workspace.heads {
            let ahead = head
                .ahead
                .map(|n| format!(" · {n} ahead"))
                .unwrap_or_default();
            let commit_oid: ObjectId = head
                .commit
                .parse()
                .context("invalid workspace head commit oid")?;
            let short = repo
                .revwalk()
                .abbreviate(commit_oid, AbbrevLength::Configured)?;
            outln!(
                w,
                "- **{}** `{}` {}{ahead}",
                code(&workspace_head_label(head)),
                short,
                head.subject
            );
        }
        outln!(w);
        Ok(())
    }

    pub(crate) fn render_ahead_shortlog(&self, w: &mut dyn Write, now: i64) -> Result<()> {
        let repo = context::discover()?;
        let sha_abbrevs = crate::log_display::abbrev_commit_summaries(&repo, &self.commit_rows)?;
        let opts = ui::CommitRowOptions {
            sha_abbrevs: Some(&sha_abbrevs),
            ..ui::CommitRowOptions::default()
        };
        for row in ui::commit_rows_with(&self.commit_rows, now, &opts)
            .iter()
            .take(SHORTLOG_LIMIT)
        {
            outln!(w, "{row}");
        }
        if self.ahead.unwrap_or(0) > SHORTLOG_LIMIT {
            outln!(
                w,
                "  … and {} more",
                self.ahead.unwrap_or(0) - SHORTLOG_LIMIT
            );
        }
        outln!(w);
        Ok(())
    }

    pub(crate) fn render_markdown_hints(&self, w: &mut dyn Write) -> Result<()> {
        match &self.guidance {
            StatusGuidance::Operation { steps, alternative } => {
                let steps: Vec<_> = steps.iter().filter_map(NextStep::command).collect();
                outln!(w, "→ `{}`", steps.join("`  ·  `"));
                if let Some(alt) = alternative.as_ref().and_then(NextStep::command) {
                    outln!(w, "or `{alt}`");
                }
            }
            StatusGuidance::ResolveConflictsThenContinueMerge => {
                outln!(w, "→ resolve conflicts, then grit merge --continue");
            }
            StatusGuidance::ResolveConflictsThenStage => {
                outln!(
                    w,
                    "→ resolve conflicts, then grit add `PATH` to mark them resolved"
                );
            }
            StatusGuidance::ContinueMerge => {
                outln!(w, "→ grit merge --continue to finish the merge");
            }
            StatusGuidance::Actions(actions) => {
                let hints: Vec<String> = actions
                    .iter()
                    .filter_map(|action| match action {
                        StatusAction::DiscardWorktreeChanges => None,
                        StatusAction::Stage => Some("grit add `PATH` to stage".to_owned()),
                        StatusAction::Commit if self.gitbutler_workspace.is_some() => {
                            Some("`but commit` to commit (GitButler workspace)".to_owned())
                        }
                        StatusAction::Commit => {
                            Some("grit commit `\"message\"` to commit".to_owned())
                        }
                        StatusAction::Pull { .. } => self
                            .integration_update_hint()
                            .map(|hint| integration_hint_command(hint, true)),
                    })
                    .collect();
                if !hints.is_empty() {
                    outln!(w, "→ {}", hints.join("  ·  "));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn render_markdown_header(
        &self,
        w: &mut dyn Write,
        ctx: &RenderContext,
    ) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let base = self.header_comparison_name();
        if let (Some(workspace), StatusHeader::Branch(_)) =
            (&self.gitbutler_workspace, &self.header)
        {
            return self.render_workspace_markdown(w, workspace);
        }
        match &self.header {
            StatusHeader::Branch(StandingHeader::Compared(Standing::Ahead)) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** ahead of `{base}`",
                    self.ahead.unwrap_or(0)
                );
                self.render_upstream_line_markdown(w)?;
                self.render_markdown_ahead_shortlog(w, ctx)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Behind)) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** behind `{base}`",
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_markdown(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Diverged)) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** ahead, **{}** behind `{base}`",
                    self.ahead.unwrap_or(0),
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_markdown(w)?;
                self.render_markdown_ahead_shortlog(w, ctx)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Even)) => {
                outln!(w, "On **`{branch}`** · even with `{base}`");
                self.render_upstream_line_markdown(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) if base.is_empty() => {
                outln!(w, "On **`{branch}`**");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) => {
                outln!(w, "On **`{branch}`** · `{base}`");
                self.render_upstream_line_markdown(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::NoTarget) => {
                outln!(w, "On **`{branch}`**");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Unborn) => {
                outln!(w, "On **`{branch}`** — no commits yet");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Detached) => {
                let short = self.head_short.as_deref().unwrap_or_default();
                outln!(w, "Detached at `{short}`");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Invalid) => {
                outln!(w, "HEAD is in an unknown state");
                outln!(w);
            }
            StatusHeader::MergeInProgress => {
                outln!(w, "On **`{branch}`** · merging — resolve conflicts");
                outln!(w);
            }
            StatusHeader::RebaseInProgress { branch, onto } => {
                let branch = branch
                    .as_deref()
                    .map_or_else(|| "detached HEAD".to_owned(), |b| format!("**`{b}`**"));
                match onto {
                    Some(onto) => outln!(w, "Rebasing {branch} onto `{onto}`"),
                    None => outln!(w, "Rebasing {branch}"),
                }
                outln!(w);
            }
            StatusHeader::OtherInProgress => {
                outln!(w, "On **`{branch}`** · operation in progress");
                outln!(w);
            }
        }
        Ok(())
    }

    pub(crate) fn render_markdown_ahead_shortlog(
        &self,
        w: &mut dyn Write,
        ctx: &RenderContext,
    ) -> Result<()> {
        if !self.commit_rows.is_empty() {
            outln!(w);
            markdown::print_commit_list(
                w,
                &self.commit_rows[..self.commit_rows.len().min(SHORTLOG_LIMIT)],
                ctx,
            )?;
            if self.ahead.unwrap_or(0) > SHORTLOG_LIMIT {
                outln!(w);
                outln!(
                    w,
                    "… and {} more commit(s).",
                    self.ahead.unwrap_or(0) - SHORTLOG_LIMIT
                );
            }
        }
        outln!(w);
        Ok(())
    }

    pub(crate) fn render_markdown_changes(&self, w: &mut dyn Write) -> Result<()> {
        let display = self.path_display.as_ref();
        let ignored = self.ignored_paths();
        if self.clean {
            outln!(w, "Nothing to commit — working tree clean.");
            markdown::print_ignored(w, ignored, display)?;
            return Ok(());
        }
        markdown::print_change_section(w, "Staged", &self.staged_entries, display)?;
        markdown::print_change_section(w, "Changed (not staged)", &self.unstaged_entries, display)?;
        if !self.conflict_groups.is_empty() {
            self.render_markdown_conflicts_by_kind(w, display)?;
        }
        markdown::print_untracked(w, &self.untracked, display)?;
        crate::embedded_gitlink::emit_embedded_repository_status_notes_markdown(
            &self.embedded_repositories,
        );
        markdown::print_ignored(w, ignored, display)?;
        self.render_worktree_case_collision_notice(w, crate::output::OutputMode::Markdown)?;
        Ok(())
    }

    pub(crate) fn render_markdown_conflicts_by_kind(
        &self,
        w: &mut dyn Write,
        display: Option<&PathDisplayOptions>,
    ) -> Result<()> {
        outln!(w, "## Conflicts");
        for group in &self.conflict_groups {
            outln!(
                w,
                "- **{}** ({} paths)",
                kind_label(&group.kind),
                group.paths.len()
            );
            for path in &group.paths {
                let path = display
                    .map(|d| d.format_path_for_human(path))
                    .unwrap_or_else(|| path.clone());
                let shown = markdown::format_inline_code(&path);
                outln!(w, "  - {shown}");
            }
        }
        outln!(w);
        Ok(())
    }
}

fn kind_label(kind: &ConflictKind) -> &'static str {
    crate::render::status::conflict_kind_heading(kind.phrase())
}

fn workspace_head_label(head: &WorkspaceHeadJson) -> String {
    if head.branches.is_empty() {
        "(no branch)".to_owned()
    } else {
        head.branches.join(", ")
    }
}

fn integration_hint_command(hint: &StatusIntegrationHint, markdown: bool) -> String {
    let ref_name = if markdown {
        format!("`{}`", hint.ref_name)
    } else {
        hint.ref_name.clone()
    };
    match hint.action {
        StatusIntegrationAction::Pull => format!("grit pull to update from {ref_name}"),
        StatusIntegrationAction::Merge => format!("grit merge {ref_name}"),
    }
}
