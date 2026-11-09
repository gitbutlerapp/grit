//! `grit show` outcomes and rendering.

use std::io::Write;

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::AbbrevLength;

use crate::commands::show::{
    message_body, ShowKind, ShowObjectResult, ShowOutcome, ShowPathOutcome, ShowRunResult,
};

/// Print a message indented four spaces, like `git show`.
pub(crate) fn print_message(w: &mut dyn Write, message: &str) -> Result<()> {
    for line in message.lines() {
        if line.is_empty() {
            outln!(w);
        } else {
            outln!(w, "    {line}");
        }
    }
    Ok(())
}
use crate::context;
use crate::diff_render;
use crate::output::outln;
use crate::render::list::{format_inline_code, print_fenced_text};
use crate::render::style::paint;
use crate::render::table::escape_table_cell;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

impl HumanRender for ShowRunResult {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowRunResult::Single(obj) => obj.render_human(w, ctx),
            ShowRunResult::Multiple(objects) => {
                for (i, obj) in objects.iter().enumerate() {
                    if i > 0 {
                        outln!(w);
                    }
                    obj.render_human(w, ctx)?;
                }
                Ok(())
            }
        }
    }
}
impl MarkdownRender for ShowRunResult {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowRunResult::Single(obj) => obj.render_markdown(w, ctx),
            ShowRunResult::Multiple(objects) => {
                for (i, obj) in objects.iter().enumerate() {
                    if i > 0 {
                        outln!(w);
                    }
                    obj.render_markdown(w, ctx)?;
                }
                Ok(())
            }
        }
    }
}
impl HumanRender for ShowObjectResult {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowObjectResult::Commit(o) => o.render_human(w, ctx),
            ShowObjectResult::Path(p) => p.render_human(w, ctx),
        }
    }
}
impl MarkdownRender for ShowObjectResult {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowObjectResult::Commit(o) => o.render_markdown(w, ctx),
            ShowObjectResult::Path(p) => p.render_markdown(w, ctx),
        }
    }
}
impl HumanRender for ShowPathOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ShowPathOutcome::Blob {
                binary,
                text,
                data_base64,
                spec,
                ..
            } => {
                if let Some(t) = text {
                    w.write_all(t.as_bytes())
                        .map_err(|e| anyhow::anyhow!("writing blob: {e}"))?;
                    return Ok(());
                }
                if *binary {
                    let bytes = if let Some(b64) = data_base64 {
                        STANDARD
                            .decode(b64)
                            .map_err(|e| anyhow::anyhow!("decoding blob base64: {e}"))?
                    } else {
                        let (repo, _) = context::discover_with_warnings()?;
                        let resolved = repo.resolve(spec)?;
                        repo.read_object_raw_for_display(&resolved.oid)?.data
                    };
                    w.write_all(&bytes)
                        .map_err(|e| anyhow::anyhow!("writing blob: {e}"))?;
                    return Ok(());
                }
                let (repo, _) = context::discover_with_warnings()?;
                let resolved = repo.resolve(spec)?;
                let obj = repo.read_object_raw_for_display(&resolved.oid)?;
                w.write_all(&obj.data)
                    .map_err(|e| anyhow::anyhow!("writing blob: {e}"))?;
                Ok(())
            }
            ShowPathOutcome::Tree { cat_file_p, .. } => {
                w.write_all(cat_file_p)
                    .map_err(|e| anyhow::anyhow!("writing tree: {e}"))?;
                Ok(())
            }
        }
    }
}
impl MarkdownRender for ShowPathOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ShowPathOutcome::Blob {
                spec,
                oid,
                size,
                binary,
                text,
                ..
            } => {
                outln!(w, "## {}\n", format_inline_code(spec));
                outln!(w, "**Oid:** `{oid}`");
                outln!(w, "**Size:** {size} bytes\n");
                if *binary {
                    outln!(w, "_binary blob (see `data_base64` in JSON)_");
                } else if let Some(t) = text {
                    print_fenced_text(w, t)?;
                }
            }
            ShowPathOutcome::Tree { spec, entries, .. } => {
                outln!(w, "## {}\n", format_inline_code(spec));
                outln!(w, "| Mode | Type | OID | Path |");
                outln!(w, "| --- | --- | --- | --- |");
                for e in entries {
                    let path_cell = format_inline_code(&escape_table_cell(&e.path));
                    outln!(
                        w,
                        "| {} | {} | `{}` | {path_cell} |",
                        e.mode,
                        e.object_type,
                        e.oid,
                    );
                }
            }
        }
        Ok(())
    }
}
impl MarkdownRender for ShowOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(tag) = &self.tag {
            outln!(w, "# Tag `{}`", tag.name);
            if let Some(tagger) = &tag.tagger {
                outln!(w);
                outln!(w, "**Tagger:** {} <{}>", tagger.name, tagger.email);
                outln!(w, "**Date:** {}", tagger.date);
            }
            if !tag.message.trim().is_empty() {
                outln!(w);
                print_fenced_text(w, &tag.message)?;
            }
            outln!(w);
        } else if self.kind == ShowKind::Branch {
            if let Some(name) = &self.ref_name {
                outln!(w, "# Branch `{name}`");
                outln!(w);
            }
        } else if self.kind == ShowKind::Tag {
            if let Some(name) = &self.ref_name {
                outln!(w, "# Tag `{name}`");
                outln!(w);
            }
        }

        let repo = context::discover()?;
        let commit_oid: ObjectId = self
            .commit
            .oid
            .parse()
            .context("invalid commit oid in show outcome")?;
        let short = repo
            .revwalk()
            .abbreviate(commit_oid, AbbrevLength::Configured)?;
        outln!(w, "# Commit `{short}`");
        outln!(w);
        outln!(w, "**Oid:** `{}`", self.commit.oid);
        if self.commit.parents.len() > 1 {
            let parents: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| {
                    let oid: ObjectId = p.parse().context("invalid parent oid in show outcome")?;
                    Ok(format!(
                        "`{}`",
                        repo.revwalk().abbreviate(oid, AbbrevLength::Configured)?
                    ))
                })
                .collect::<Result<_>>()?;
            outln!(w, "**Merge parents:** {}", parents.join(", "));
        }
        outln!(
            w,
            "**Author:** {} <{}>",
            self.commit.author.name,
            self.commit.author.email
        );
        outln!(w, "**Date:** {}", self.commit.author.date);
        outln!(w);
        if !self.commit.subject.is_empty() {
            outln!(w, "## {}", self.commit.subject);
            outln!(w);
        }
        let body = message_body(&self.commit.message, &self.commit.subject);
        if !body.is_empty() {
            print_fenced_text(w, &body)?;
            outln!(w);
        }
        crate::diff_render::render_markdown_stat(w, &self.stat, None)?;
        self.diff.render_markdown(w, _ctx)?;
        Ok(())
    }
}
impl HumanRender for ShowOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let color = ctx.color;

        if let Some(tag) = &self.tag {
            outln!(w, "{}", paint(color, "33;1", &format!("tag {}", tag.name)));
            if let Some(tagger) = &tag.tagger {
                outln!(w, "Tagger: {} <{}>", tagger.name, tagger.email);
                outln!(w, "Date:   {}", tagger.date);
            }
            outln!(w);
            print_message(w, &tag.message)?;
            outln!(w);
        } else if self.kind == ShowKind::Branch {
            if let Some(name) = &self.ref_name {
                outln!(w, "{}", paint(color, "36", &format!("branch {name}")));
            }
        } else if self.kind == ShowKind::Tag {
            if let Some(name) = &self.ref_name {
                outln!(w, "{}", paint(color, "36", &format!("tag {name}")));
            }
        }

        if let Some(params) = &self.patch_text {
            let repo = context::discover()?;
            crate::commands::diff::emit_show_log_patch_text(&repo, params, w)?;
            return Ok(());
        }
        outln!(
            w,
            "{}",
            paint(color, "33", &format!("commit {}", self.commit.oid))
        );
        if self.commit.parents.len() > 1 {
            let repo = context::discover()?;
            let abbrev: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| -> Result<String> {
                    let oid: ObjectId = p.parse().context("invalid parent oid in show outcome")?;
                    repo.revwalk()
                        .abbreviate(oid, AbbrevLength::Configured)
                        .map_err(Into::into)
                })
                .collect::<Result<_>>()?;
            outln!(w, "Merge:  {}", abbrev.join(" "));
        }
        outln!(
            w,
            "Author: {} <{}>",
            self.commit.author.name,
            self.commit.author.email
        );
        outln!(w, "Date:   {}", self.commit.author.date);
        outln!(w);
        print_message(w, &self.commit.message)?;

        let fmt = self.diff.format_flags();
        if fmt.human_stat_block() {
            diff_render::render_stat_with_leading_blank(
                w,
                &self.stat,
                color,
                self.diff.path_display_opts(),
            )?;
        } else {
            if !self.diff.files.is_empty() && fmt.detail() == grit_lib::diff::DiffDetail::Patch {
                outln!(w);
            }
            self.diff.render_human_diff(w, None, color)?;
        }
        Ok(())
    }
}
