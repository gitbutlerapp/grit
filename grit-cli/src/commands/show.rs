//! `grit show` — show information about a commit, tag, or branch.
//!
//! Resolves the given name (default `HEAD`) and prints: for a branch, a header
//! plus the commit it points at; for a lightweight tag, the same; for an
//! annotated tag, the tag's own metadata and message followed by the target
//! commit; and for any commit, its metadata, message, and the change it
//! introduced (rendered with the same delta-style as `grit diff`).

use std::io::IsTerminal;
use std::io::Write;

use anyhow::{bail, Context, Result};
use grit_lib::objects::{parse_tag, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::{diff_of_commit, DiffOutcome, LineKind};
use crate::commits_json::CommitJsonLoader;
use crate::context;
use crate::markdown;
use crate::output::{outln, ChangeStatus, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// Width budget for the `+`/`-` change bars in the diffstat.
const BAR_WIDTH: usize = 40;

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
    pub commit: CommitJson,
    /// A summary (diffstat) of the change the commit introduced — not the full patch.
    pub stat: DiffStat,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

/// A diffstat: per-file insertion/deletion counts plus totals.
#[derive(Serialize, JsonSchema)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct FileStat {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub insertions: usize,
    pub deletions: usize,
    pub binary: bool,
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

pub fn run(object: Option<String>) -> Result<ShowOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let target = object.unwrap_or_else(|| "HEAD".to_owned());

    let (kind, ref_name, tag) = classify(&repo, &target)?;

    // Resolve the name, then peel through any (annotated) tag objects to the
    // underlying commit. `resolve_revision` resolves branches/HEAD/sha and
    // lightweight tags to a commit, but yields the tag *object* for annotated tags.
    let resolved =
        grit_lib::revwalk::rev_parse::resolve_revision_without_index_dwim(&repo, &target)
            .with_context(|| format!("could not resolve '{target}'"))?;
    let commit_oid = peel_to_commit(&repo, resolved)?;
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let loader = CommitJsonLoader::new(&repo, std::slice::from_ref(&commit_oid))?;
    let commit = loader.load_one(&repo, &commit_oid, now)?;

    let stat = diffstat(&diff_of_commit(&repo, &commit_oid)?);

    Ok(ShowOutcome {
        warnings: sink.warnings(),
        kind,
        ref_name,
        tag,
        commit,
        stat,
        path_display: PathDisplayOptions::from_repo(&repo).ok(),
    })
}

/// Reduce a full diff to a per-file insertion/deletion summary.
pub(crate) fn diffstat(diff: &DiffOutcome) -> DiffStat {
    let mut files = Vec::new();
    let mut insertions = 0;
    let mut deletions = 0;
    for file in &diff.files {
        let mut ins = 0;
        let mut del = 0;
        for hunk in &file.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Add => ins += 1,
                    LineKind::Del => del += 1,
                    LineKind::Context => {}
                }
            }
        }
        insertions += ins;
        deletions += del;
        files.push(FileStat {
            path: file.path.clone(),
            old_path: file.old_path.clone(),
            status: file.status,
            insertions: ins,
            deletions: del,
            binary: file.binary,
        });
    }
    DiffStat {
        files_changed: files.len(),
        files,
        insertions,
        deletions,
    }
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
        render_markdown_stat(w, &self.stat, self.path_display.as_ref())?;
        Ok(())
    }
}

impl HumanRender for ShowOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = use_color();

        // Annotated tags lead with their own block.
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

        // Commit block.
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

        render_stat(w, &self.stat, color, self.path_display.as_ref())?;
        Ok(())
    }
}

/// Render the diffstat, mirroring `git show --stat`.
pub(crate) fn render_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if stat.files.is_empty() {
        return Ok(());
    }
    outln!(w);

    let name_width = stat
        .files
        .iter()
        .map(|f| stat_path(f, path_opts).chars().count())
        .max()
        .unwrap_or(0)
        .min(60);
    let max_changes = stat
        .files
        .iter()
        .map(|f| f.insertions + f.deletions)
        .max()
        .unwrap_or(0);

    for file in &stat.files {
        let path = stat_path(file, path_opts);
        if file.binary {
            outln!(w, " {path:<name_width$} | Bin");
            continue;
        }
        let total = file.insertions + file.deletions;
        let bar = change_bar(file.insertions, file.deletions, max_changes, color);
        outln!(w, " {path:<name_width$} | {total:>3} {bar}");
    }

    outln!(w, " {}", summary_line(stat));
    Ok(())
}

/// `old => new` for a rename, otherwise just the path.
fn stat_path(file: &FileStat, path_opts: Option<&PathDisplayOptions>) -> String {
    match &file.old_path {
        Some(old) => {
            let old_shown = path_opts
                .map(|o| o.format_path_for_human(old))
                .unwrap_or_else(|| old.clone());
            let new_shown = path_opts
                .map(|o| o.format_path_for_human(&file.path))
                .unwrap_or_else(|| file.path.clone());
            format!("{old_shown} => {new_shown}")
        }
        None => path_opts
            .map(|o| o.format_path_for_human(&file.path))
            .unwrap_or_else(|| file.path.clone()),
    }
}

/// A scaled `+`/`-` bar (green/red on a TTY), like `git --stat`.
fn change_bar(insertions: usize, deletions: usize, max_changes: usize, color: bool) -> String {
    let total = insertions + deletions;
    if total == 0 {
        return String::new();
    }
    // Scale the longest file's bar to BAR_WIDTH; shorter ones scale proportionally.
    let scaled = if max_changes > BAR_WIDTH {
        ((total * BAR_WIDTH).div_ceil(max_changes)).max(1)
    } else {
        total
    };
    let mut plus = ((insertions * scaled) as f64 / total as f64).round() as usize;
    let mut minus = scaled.saturating_sub(plus);
    // Keep at least one cell for a side that actually changed.
    if insertions > 0 && plus == 0 {
        plus = 1;
        minus = minus.saturating_sub(1);
    }
    if deletions > 0 && minus == 0 {
        minus = 1;
        plus = plus.saturating_sub(1);
    }
    let plus_bar = "+".repeat(plus);
    let minus_bar = "-".repeat(minus);
    if color {
        format!(
            "{}{}",
            paint(true, "32", &plus_bar),
            paint(true, "31", &minus_bar)
        )
    } else {
        format!("{plus_bar}{minus_bar}")
    }
}

/// `N files changed, N insertions(+), N deletions(-)` (omitting zero parts).
fn summary_line(stat: &DiffStat) -> String {
    let mut parts = vec![format!(
        "{} file{} changed",
        stat.files_changed,
        plural(stat.files_changed)
    )];
    if stat.insertions > 0 {
        parts.push(format!(
            "{} insertion{}(+)",
            stat.insertions,
            plural(stat.insertions)
        ));
    }
    if stat.deletions > 0 {
        parts.push(format!(
            "{} deletion{}(-)",
            stat.deletions,
            plural(stat.deletions)
        ));
    }
    parts.join(", ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
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

fn message_body(full: &str, subject: &str) -> String {
    let mut lines = full.lines();
    let first = lines.next().unwrap_or("");
    if first == subject {
        lines.collect::<Vec<_>>().join("\n").trim().to_owned()
    } else {
        full.trim().to_owned()
    }
}

pub(crate) fn render_markdown_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if stat.files.is_empty() {
        return Ok(());
    }
    outln!(w, "## Changes");
    outln!(w);
    outln!(w, "| File | + | − |");
    outln!(w, "| --- | ---: | ---: |");
    for file in &stat.files {
        let path = markdown::escape_table_cell(&stat_path(file, path_opts));
        let path_code = markdown::format_inline_code(&path);
        if file.binary {
            outln!(w, "| {path_code} | binary | |");
        } else {
            outln!(
                w,
                "| {path_code} | {} | {} |",
                file.insertions,
                file.deletions
            );
        }
    }
    outln!(w);
    outln!(w, "{}", summary_line(stat));
    Ok(())
}

pub(crate) fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}
