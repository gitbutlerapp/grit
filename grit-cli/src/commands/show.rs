//! `grit show` — show information about a commit, tag, or branch.
//!
//! Resolves the given name (default `HEAD`) and prints: for a branch, a header
//! plus the commit it points at; for a lightweight tag, the same; for an
//! annotated tag, the tag's own metadata and message followed by the target
//! commit; and for any commit, its metadata, message, and the change it
//! introduced (rendered with the same delta-style as `grit diff`).

use std::io::IsTerminal;

use anyhow::{bail, Context, Result};
use grit_lib::objects::{parse_tag, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::commands::diff::{diff_of_commit, DiffOutcome, LineKind};
use crate::context::{self, subject_line};
use crate::markdown;
use crate::output::{HumanRender, MarkdownRender};

/// Width budget for the `+`/`-` change bars in the diffstat.
const BAR_WIDTH: usize = 40;

/// Result of `grit show`.
#[derive(Serialize)]
pub struct ShowOutcome {
    /// Non-fatal warnings emitted while resolving the object.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,
    /// `commit` | `branch` | `tag` | `annotated_tag`.
    pub kind: String,
    /// Branch or tag name, when shown via a ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    /// Annotated-tag metadata, when `kind == "annotated_tag"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<TagInfo>,
    pub commit: CommitInfo,
    /// A summary (diffstat) of the change the commit introduced — not the full patch.
    pub stat: DiffStat,
}

/// A diffstat: per-file insertion/deletion counts plus totals.
#[derive(Serialize)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Serialize)]
pub struct FileStat {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub insertions: usize,
    pub deletions: usize,
    pub binary: bool,
}

#[derive(Serialize)]
pub struct CommitInfo {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: Person,
    pub committer: Person,
    pub subject: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct TagInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagger: Option<Person>,
    pub message: String,
}

/// A parsed identity line (`Name <email> <epoch> <tz>`).
#[derive(Serialize)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub date: String,
}

pub fn run(object: Option<String>) -> Result<ShowOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let target = object.unwrap_or_else(|| "HEAD".to_owned());

    let (kind, ref_name, tag) = classify(&repo, &target)?;

    // Resolve the name, then peel through any (annotated) tag objects to the
    // underlying commit. `resolve_revision` resolves branches/HEAD/sha and
    // lightweight tags to a commit, but yields the tag *object* for annotated tags.
    let resolved = grit_lib::rev_parse::resolve_revision(&repo, &target)
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
        author: Person::parse(&data.author),
        committer: Person::parse(&data.committer),
        subject: subject_line(&data.message),
        message: data.message.trim_end().to_owned(),
    };

    let stat = diffstat(&diff_of_commit(&repo, &commit_oid)?);

    Ok(ShowOutcome {
        warnings: sink.warnings(),
        kind,
        ref_name,
        tag,
        commit,
        stat,
    })
}

/// Reduce a full diff to a per-file insertion/deletion summary.
fn diffstat(diff: &DiffOutcome) -> DiffStat {
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
            status: file.status.clone(),
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
fn classify(repo: &Repository, target: &str) -> Result<(String, Option<String>, Option<TagInfo>)> {
    let git_dir = &repo.git_dir;

    if target == "HEAD" {
        if let Ok(Some(sym)) = refs::read_symbolic_ref(git_dir, "HEAD") {
            if let Some(branch) = sym.strip_prefix("refs/heads/") {
                return Ok(("branch".to_owned(), Some(branch.to_owned()), None));
            }
        }
        return Ok(("commit".to_owned(), None, None));
    }

    if refs::resolve_ref(git_dir, &format!("refs/heads/{target}")).is_ok() {
        return Ok(("branch".to_owned(), Some(target.to_owned()), None));
    }

    if let Ok(tag_oid) = refs::resolve_ref(git_dir, &format!("refs/tags/{target}")) {
        let object = repo.odb.read(&tag_oid)?;
        if object.kind == ObjectKind::Tag {
            let td = parse_tag(&object.data)?;
            let info = TagInfo {
                name: target.to_owned(),
                tagger: td.tagger.as_deref().map(Person::parse),
                message: td.message.trim_end().to_owned(),
            };
            return Ok((
                "annotated_tag".to_owned(),
                Some(target.to_owned()),
                Some(info),
            ));
        }
        return Ok(("tag".to_owned(), Some(target.to_owned()), None));
    }

    Ok(("commit".to_owned(), None, None))
}

/// Follow tag objects until reaching a commit.
fn peel_to_commit(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let object = repo.odb.read(&oid)?;
        match object.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => oid = parse_tag(&object.data)?.object,
            _ => bail!("'{}' does not resolve to a commit", oid.to_hex()),
        }
    }
}

impl Person {
    /// Parse a raw `Name <email> <epoch> <tz>` identity line.
    fn parse(ident: &str) -> Self {
        let (name, rest) = ident.split_once(" <").map_or((ident, ""), |(n, r)| (n, r));
        let (email, when) = rest.split_once("> ").map_or((rest, ""), |(e, w)| (e, w));
        Self {
            name: name.trim().to_owned(),
            email: email.trim_end_matches('>').to_owned(),
            date: format_date(when),
        }
    }
}

/// Format `"<epoch> <tz>"` as `YYYY-MM-DD HH:MM:SS ±HHMM` in the recorded zone.
fn format_date(when: &str) -> String {
    let mut parts = when.split_whitespace();
    let Some(epoch) = parts.next().and_then(|s| s.parse::<i64>().ok()) else {
        return when.trim().to_owned();
    };
    let tz = parts.next().unwrap_or("+0000");
    let offset = tz_offset_seconds(tz);
    let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(epoch + offset) else {
        return when.trim().to_owned();
    };
    let Ok(fmt) = time::format_description::parse_borrowed::<1>(
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    ) else {
        return when.trim().to_owned();
    };
    match dt.format(&fmt) {
        Ok(s) => format!("{s} {tz}"),
        Err(_) => when.trim().to_owned(),
    }
}

/// Parse a `±HHMM` timezone offset into seconds.
fn tz_offset_seconds(tz: &str) -> i64 {
    if tz.len() < 5 {
        return 0;
    }
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let hours: i64 = tz[1..3].parse().unwrap_or(0);
    let minutes: i64 = tz[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

// --- Human rendering --------------------------------------------------------

impl MarkdownRender for ShowOutcome {
    fn render_markdown(&self) {
        if let Some(tag) = &self.tag {
            println!("# Tag `{}`", tag.name);
            if let Some(tagger) = &tag.tagger {
                println!();
                println!("**Tagger:** {} <{}>", tagger.name, tagger.email);
                println!("**Date:** {}", tagger.date);
            }
            if !tag.message.trim().is_empty() {
                println!();
                print_markdown_message(&tag.message);
            }
            println!();
        } else if self.kind == "branch" {
            if let Some(name) = &self.ref_name {
                println!("# Branch `{name}`");
                println!();
            }
        } else if self.kind == "tag" {
            if let Some(name) = &self.ref_name {
                println!("# Tag `{name}`");
                println!();
            }
        }

        let short = self.commit.oid.get(..7).unwrap_or(self.commit.oid.as_str());
        println!("# Commit `{short}`");
        println!();
        println!("**Oid:** `{}`", self.commit.oid);
        if self.commit.parents.len() > 1 {
            let parents: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| format!("`{}`", p.get(..7).unwrap_or(p.as_str())))
                .collect();
            println!("**Merge parents:** {}", parents.join(", "));
        }
        println!(
            "**Author:** {} <{}>",
            self.commit.author.name, self.commit.author.email
        );
        println!("**Date:** {}", self.commit.author.date);
        println!();
        if !self.commit.subject.is_empty() {
            println!("## {}", self.commit.subject);
            println!();
        }
        let body = message_body(&self.commit.message, &self.commit.subject);
        if !body.is_empty() {
            print_markdown_message(&body);
            println!();
        }
        render_markdown_stat(&self.stat);
    }
}

impl HumanRender for ShowOutcome {
    fn render_human(&self) {
        let color = use_color();

        // Annotated tags lead with their own block.
        if let Some(tag) = &self.tag {
            println!("{}", paint(color, "33;1", &format!("tag {}", tag.name)));
            if let Some(tagger) = &tag.tagger {
                println!("Tagger: {} <{}>", tagger.name, tagger.email);
                println!("Date:   {}", tagger.date);
            }
            println!();
            print_message(&tag.message);
            println!();
        } else if self.kind == "branch" {
            if let Some(name) = &self.ref_name {
                println!("{}", paint(color, "36", &format!("branch {name}")));
            }
        } else if self.kind == "tag" {
            if let Some(name) = &self.ref_name {
                println!("{}", paint(color, "36", &format!("tag {name}")));
            }
        }

        // Commit block.
        println!(
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
            println!("Merge:  {}", abbrev.join(" "));
        }
        println!(
            "Author: {} <{}>",
            self.commit.author.name, self.commit.author.email
        );
        println!("Date:   {}", self.commit.author.date);
        println!();
        print_message(&self.commit.message);

        render_stat(&self.stat, color);
    }
}

/// Render the diffstat, mirroring `git show --stat`.
fn render_stat(stat: &DiffStat, color: bool) {
    if stat.files.is_empty() {
        return;
    }
    println!();

    let name_width = stat
        .files
        .iter()
        .map(|f| stat_path(f).chars().count())
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
        let path = stat_path(file);
        if file.binary {
            println!(" {path:<name_width$} | Bin");
            continue;
        }
        let total = file.insertions + file.deletions;
        let bar = change_bar(file.insertions, file.deletions, max_changes, color);
        println!(" {path:<name_width$} | {total:>3} {bar}");
    }

    println!(" {}", summary_line(stat));
}

/// `old => new` for a rename, otherwise just the path.
fn stat_path(file: &FileStat) -> String {
    match &file.old_path {
        Some(old) => format!("{old} => {}", file.path),
        None => file.path.clone(),
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
fn print_message(message: &str) {
    for line in message.lines() {
        if line.is_empty() {
            println!();
        } else {
            println!("    {line}");
        }
    }
}

fn print_markdown_message(message: &str) {
    for line in message.lines() {
        println!("{line}");
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

fn render_markdown_stat(stat: &DiffStat) {
    if stat.files.is_empty() {
        return;
    }
    println!("## Changes");
    println!();
    println!("| File | + | − |");
    println!("| --- | ---: | ---: |");
    for file in &stat.files {
        let path = markdown::escape_table_cell(&stat_path(file));
        if file.binary {
            println!("| `{path}` | binary | |");
        } else {
            println!("| `{path}` | {} | {} |", file.insertions, file.deletions);
        }
    }
    println!();
    println!("{}", summary_line(stat));
}

fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}
