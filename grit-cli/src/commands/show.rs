//! `grit show` — show information about a commit, tag, or branch.
//!
//! Resolves the given name (default `HEAD`) and prints: for a branch, a header
//! plus the commit it points at; for a lightweight tag, the same; for an
//! annotated tag, the tag's own metadata and message followed by the target
//! commit; and for any commit, its metadata, message, and the change it
//! introduced (rendered with the same delta-style as `grit diff`).

use std::io::IsTerminal;

use anyhow::{bail, Context, Result};
use grit_lib::diffstat::{write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::objects::{parse_tag, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::commands::diff::{diff_of_commit, DiffOutcome, LineKind};
use crate::context::{self, subject_line};
use crate::output::HumanRender;

/// Result of `grit show`.
#[derive(Serialize)]
pub struct ShowOutcome {
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
    let repo = context::discover()?;
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

    let files: Vec<FileStatInput> = stat
        .files
        .iter()
        .map(|f| FileStatInput {
            path_display: stat_path(f),
            insertions: f.insertions,
            deletions: f.deletions,
            is_binary: f.binary,
            is_unmerged: f.status.eq_ignore_ascii_case("unmerged"),
        })
        .collect();

    let color_add = if color { "\x1b[32m" } else { "" };
    let color_del = if color { "\x1b[31m" } else { "" };
    let color_reset = if color { "\x1b[m" } else { "" };
    let opts = DiffstatOptions {
        total_width: 80,
        line_prefix: "",
        width_prefix: "",
        subtract_prefix_from_terminal: true,
        terminal_width: crate::ui::terminal_width_columns(),
        stat_name_width: None,
        stat_graph_width: None,
        stat_count: None,
        color_add,
        color_del,
        color_reset,
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };

    let mut buf = Vec::new();
    if write_diffstat_block(&mut buf, &files, &opts).is_ok() {
        print!("{}", String::from_utf8_lossy(&buf));
    }
}

/// `old => new` for a rename, otherwise just the path.
fn stat_path(file: &FileStat) -> String {
    match &file.old_path {
        Some(old) => format!("{old} => {}", file.path),
        None => file.path.clone(),
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
