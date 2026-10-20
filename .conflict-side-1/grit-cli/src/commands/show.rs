//! `grit show` — show information about a commit, tag, or branch.
//!
//! Resolves the given name (default `HEAD`) and prints: for a branch, a header
//! plus the commit it points at; for a lightweight tag, the same; for an
//! annotated tag, the tag's own metadata and message followed by the target
//! commit; and for any commit, its metadata, message, and the change it
//! introduced (delta-style patch by default, or `--stat` / other diff formats).
//!
//! Merge commits use first-parent diff semantics (same as `git show` without
//! `--diff-merges`); combined parent diffs are not shown here.

use std::io::Write;

use anyhow::{bail, Context, Result};
use grit_lib::objects::{parse_tag, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::{CommitDiffOptions, DiffOutcome, DiffTotalsJson, FileDiff};
use crate::context::{self, subject_line};
use crate::diff_format::DiffFormatArgs;
pub use crate::diff_render::DiffStat;
use crate::diff_render::{self, diffstat_from_files};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Inputs for `grit show`.
pub struct ShowRunOptions {
    pub object: Option<String>,
    pub paths: Vec<String>,
    pub format: DiffFormatArgs,
    pub word_level_inline: bool,
}

/// What `grit show` resolved (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShowKind {
    Commit,
    Branch,
    Tag,
    AnnotatedTag,
}

/// Result of `grit show`.
#[derive(Serialize, JsonSchema)]
pub struct ShowOutcome {
    /// Non-fatal warnings emitted while resolving the object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,
    pub kind: ShowKind,
    /// Branch or tag name, when shown via a ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    /// Annotated-tag metadata, when `kind == "annotated_tag"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tag: Option<TagInfo>,
    pub commit: CommitInfo,
    /// Per-file diff rows (same shape as `grit diff --json`).
    pub files: Vec<FileDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    /// Summary diffstat (always present for scripting convenience).
    pub stat: DiffStat,
    #[serde(skip)]
    #[schemars(skip)]
    diff: DiffOutcome,
}

#[derive(Serialize, JsonSchema)]
pub struct CommitInfo {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: Person,
    pub committer: Person,
    pub subject: String,
    pub message: String,
}

#[derive(Serialize, JsonSchema)]
pub struct TagInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tagger: Option<Person>,
    pub message: String,
}

pub use crate::output::Person;

pub fn run(opts: ShowRunOptions) -> Result<ShowOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let target = opts.object.unwrap_or_else(|| "HEAD".to_owned());

    let (kind, ref_name, tag) = classify(&repo, &target)?;

    let resolved =
        grit_lib::revwalk::rev_parse::resolve_revision_without_index_dwim(&repo, &target)
            .with_context(|| format!("could not resolve '{target}'"))?;
    let commit_oid = peel_to_commit(&repo, resolved)?;
    let data = context::read_commit(&repo, &commit_oid)?;

    let commit = CommitInfo {
        oid: commit_oid.to_hex(),
        parents: data
            .parents
            .iter()
            .map(grit_lib::objects::ObjectId::to_hex)
            .collect(),
        author: Person::from_identity_line(&data.author),
        committer: Person::from_identity_line(&data.committer),
        subject: subject_line(&data.message),
        message: data.message.trim_end().to_owned(),
    };

    let diff = crate::commands::diff::diff_for_commit(
        &repo,
        &commit_oid,
        CommitDiffOptions {
            paths: opts.paths,
            format: opts.format,
            word_level_inline: opts.word_level_inline,
        },
    )?;
    let stat = diffstat_from_files(&diff.files);
    let files = diff.files.clone();
    let totals = diff.totals;

    Ok(ShowOutcome {
        warnings: sink.warnings(),
        kind,
        ref_name,
        tag,
        commit,
        files,
        totals,
        stat,
        diff,
    })
}

/// Back-compat wrapper for stash and log.
pub(crate) fn diffstat(diff: &crate::commands::diff::DiffOutcome) -> DiffStat {
    diffstat_from_files(&diff.files)
}

/// Determine what `target` names: a branch, a tag (light or annotated), or a
/// bare commit. Returns `(kind, ref_name, annotated_tag_info)`.
fn classify(
    repo: &Repository,
    target: &str,
) -> Result<(ShowKind, Option<String>, Option<TagInfo>)> {
    let git_dir = repo.git_dir();

    if target == "HEAD" {
        if let Ok(Some(sym)) = refs::read_symbolic_ref(git_dir, "HEAD") {
            if let Some(branch) = sym.strip_prefix("refs/heads/") {
                return Ok((ShowKind::Branch, Some(branch.to_owned()), None));
            }
        }
        return Ok((ShowKind::Commit, None, None));
    }

    if refs::resolve_ref(git_dir, &format!("refs/heads/{target}")).is_ok() {
        return Ok((ShowKind::Branch, Some(target.to_owned()), None));
    }

    if let Ok(tag_oid) = refs::resolve_ref(git_dir, &format!("refs/tags/{target}")) {
        let object = repo.odb().read(&tag_oid)?;
        if object.kind == ObjectKind::Tag {
            let td = parse_tag(&object.data)?;
            let info = TagInfo {
                name: target.to_owned(),
                tagger: td.tagger.as_deref().map(Person::from_identity_line),
                message: td.message.trim_end().to_owned(),
            };
            return Ok((ShowKind::AnnotatedTag, Some(target.to_owned()), Some(info)));
        }
        return Ok((ShowKind::Tag, Some(target.to_owned()), None));
    }

    Ok((ShowKind::Commit, None, None))
}

/// Follow tag objects until reaching a commit.
fn peel_to_commit(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let object = repo.odb().read(&oid)?;
        match object.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => oid = parse_tag(&object.data)?.object,
            _ => bail!("'{}' does not resolve to a commit", oid.to_hex()),
        }
    }
}

// --- Human rendering --------------------------------------------------------

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
                print_markdown_message(w, &tag.message)?;
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

        let short = self.commit.oid.get(..7).unwrap_or(self.commit.oid.as_str());
        outln!(w, "# Commit `{short}`");
        outln!(w);
        outln!(w, "**Oid:** `{}`", self.commit.oid);
        if self.commit.parents.len() > 1 {
            let parents: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| format!("`{}`", p.get(..7).unwrap_or(p.as_str())))
                .collect();
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
            print_markdown_message(w, &body)?;
            outln!(w);
        }
        self.diff.render_markdown_diff(w)?;
        Ok(())
    }
}

impl HumanRender for ShowOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = diff_render::use_color();

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

        outln!(
            w,
            "{}",
            paint(color, "33", &format!("commit {}", self.commit.oid))
        );
        if self.commit.parents.len() > 1 {
            let abbrev: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| p.get(..7).unwrap_or(p).to_owned())
                .collect();
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
            self.diff.render_human_diff(w, None)?;
        }
        Ok(())
    }
}

/// Print a message indented four spaces, like `git show`.
fn print_message(w: &mut dyn Write, message: &str) -> Result<()> {
    for line in message.lines() {
        if line.is_empty() {
            outln!(w);
        } else {
            outln!(w, "    {line}");
        }
    }
    Ok(())
}

fn print_markdown_message(w: &mut dyn Write, message: &str) -> Result<()> {
    for line in message.lines() {
        outln!(w, "{line}");
    }
    Ok(())
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn message_body(full: &str, subject: &str) -> String {
    let mut lines = full.lines();
    let first = lines.next().unwrap_or("");
    if first == subject {
        lines.collect::<Vec<_>>().join("\n").trim().to_owned()
    } else {
        full.trim().to_owned()
    }
}
