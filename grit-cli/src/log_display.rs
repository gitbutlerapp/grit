//! Human and Markdown rendering helpers for `grit log` display modes.

use std::io::Write;

use anyhow::Result;
use grit_lib::object_inspect::abbreviate_for_display;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

use crate::output::{outln, CommitJson, CommitRefJson, CommitRefKind};

/// Format ref decorations for `--oneline` / `--decorate` (Git `%d` / `%h %d %s`).
///
/// When `head_branch` is set (symbolic `HEAD` on that branch), fold `HEAD -> <branch>`
/// using that branch name instead of an arbitrary same-tip branch.
pub fn format_decorations_folded(refs: &[CommitRefJson], head_branch: Option<&str>) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let head = refs.iter().find(|r| r.kind == CommitRefKind::Head);
    let branch = head_branch.and_then(|hb| {
        refs.iter()
            .find(|r| r.kind == CommitRefKind::Branch && r.name == hb)
    });
    let mut parts: Vec<String> = Vec::new();
    if head.is_some() {
        if let Some(b) = branch {
            parts.push(format!("HEAD -> {}", b.name));
        } else {
            parts.push("HEAD".to_owned());
        }
    }
    for r in refs {
        match r.kind {
            CommitRefKind::Head => {}
            CommitRefKind::Branch if branch.is_some_and(|b| b.name == r.name) && head.is_some() => {
            }
            CommitRefKind::Tag => parts.push(format!("tag: {}", r.name)),
            _ => parts.push(r.name.clone()),
        }
    }
    format!("({})", parts.join(", "))
}

/// Insert decorations into a default log row after the abbreviated sha column.
pub fn decorate_commit_row(row: &str, refs: &[CommitRefJson], head_branch: Option<&str>) -> String {
    let deco = format_decorations_folded(refs, head_branch);
    if deco.is_empty() {
        return row.to_owned();
    }
    let bytes = row.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    while i < bytes.len() && bytes[i] != b' ' {
        i += 1;
    }
    let mut out = String::with_capacity(row.len() + deco.len() + 2);
    out.push_str(&row[..i]);
    out.push(' ');
    out.push_str(&deco);
    out.push_str(&row[i..]);
    out
}

/// One `--oneline` row: `<abbrev> [<decorations>] <subject>`.
pub fn render_oneline_line(
    repo: &Repository,
    commit: &CommitJson,
    decorate: bool,
    head_branch: Option<&str>,
) -> Result<String> {
    let oid: ObjectId = commit
        .oid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid commit oid in log row: {e}"))?;
    let abbrev = abbreviate_for_display(repo, oid, true)?;
    let deco = if decorate {
        let d = format_decorations_folded(&commit.refs, head_branch);
        if d.is_empty() {
            String::new()
        } else {
            format!("{d} ")
        }
    } else {
        String::new()
    };
    Ok(format!("{abbrev} {deco}{}", commit.subject))
}

/// Shortlog-style author group lines (`git shortlog -sne`).
pub fn render_author_groups_human(
    w: &mut dyn Write,
    groups: &[(usize, String, String)],
) -> Result<()> {
    let width = groups
        .iter()
        .map(|(count, _, _)| count.to_string().len())
        .max()
        .unwrap_or(1);
    for (count, name, email) in groups {
        let label = author_label(name, email);
        outln!(w, "{count:>width$}\t{label}");
    }
    Ok(())
}

pub fn render_author_groups_markdown(
    w: &mut dyn Write,
    groups: &[(usize, String, String)],
) -> Result<()> {
    for (count, name, email) in groups {
        let label = if email.is_empty() {
            name.clone()
        } else {
            format!("{name} `<{email}>`")
        };
        outln!(w, "- **{count}** {label}");
    }
    Ok(())
}

fn author_label(name: &str, email: &str) -> String {
    if email.is_empty() {
        name.to_owned()
    } else {
        format!("{name} <{email}>")
    }
}
