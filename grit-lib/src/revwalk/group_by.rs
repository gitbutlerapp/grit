//! Group walked commits for summary views (author shortlog-style reports).

use std::collections::HashMap;

use crate::commit_encoding;
use crate::error::Result;
use crate::mailmap::{load_mailmap_table, map_contact_table, parse_contact, MailmapTable};
use crate::objects::ObjectId;
use crate::repo::Repository;

/// One author's commits after grouping a walk result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorGroup {
    /// Display name after mailmap (may be empty when only email is known).
    pub name: String,
    /// Email after mailmap (may be empty).
    pub email: String,
    /// Number of commits in this group.
    pub count: usize,
    /// Commit ids in walk order within the group (empty when [`GroupByAuthorOptions::count_only`]).
    pub commits: Vec<ObjectId>,
}

/// Options for [`group_commits_by_author`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GroupByAuthorOptions {
    /// When true, omit per-commit oid lists (counts only; faster for `--count`).
    pub count_only: bool,
}

/// Group `oids` by mapped author identity, sorted like `git shortlog -sne`.
///
/// When `mailmap` is `None`, authors are taken from each commit header unchanged.
/// Sort order is descending commit count, then ascending `Name <email>` string
/// (matching Git's `compareAuthors`).
///
/// # Errors
///
/// Returns ODB and commit parse errors when an oid is missing or corrupt.
pub fn group_commits_by_author(
    repo: &Repository,
    oids: &[ObjectId],
    mailmap: Option<&MailmapTable>,
    options: GroupByAuthorOptions,
) -> Result<Vec<AuthorGroup>> {
    let odb = repo.odb();
    if options.count_only {
        let mut counts: HashMap<(String, String), usize> = HashMap::new();
        for oid in oids {
            let (name, email) = author_for_commit(odb, *oid, mailmap)?;
            *counts.entry((name, email)).or_default() += 1;
        }
        let mut groups: Vec<AuthorGroup> = counts
            .into_iter()
            .map(|((name, email), count)| AuthorGroup {
                name,
                email,
                count,
                commits: Vec::new(),
            })
            .collect();
        groups.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| author_sort_key(a).cmp(&author_sort_key(b)))
        });
        return Ok(groups);
    }

    let mut buckets: HashMap<(String, String), Vec<ObjectId>> = HashMap::new();
    for oid in oids {
        let (name, email) = author_for_commit(odb, *oid, mailmap)?;
        buckets.entry((name, email)).or_default().push(*oid);
    }

    let mut groups: Vec<AuthorGroup> = buckets
        .into_iter()
        .map(|((name, email), commits)| AuthorGroup {
            count: commits.len(),
            name,
            email,
            commits,
        })
        .collect();

    groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| author_sort_key(a).cmp(&author_sort_key(b)))
    });
    Ok(groups)
}

/// Load `.mailmap` when `use_mailmap` is true (same sources as `log.mailmap`).
///
/// # Errors
///
/// Propagates mailmap load errors from the repository.
pub fn mailmap_for_grouping(repo: &Repository, use_mailmap: bool) -> Result<Option<MailmapTable>> {
    if !use_mailmap {
        return Ok(None);
    }
    let table = load_mailmap_table(repo)?;
    Ok((!table.is_empty()).then_some(table))
}

fn author_for_commit(
    odb: &crate::odb::Odb,
    oid: ObjectId,
    mailmap: Option<&MailmapTable>,
) -> Result<(String, String)> {
    let obj = odb.read(&oid)?;
    let (author_raw, encoding) = scan_author_raw_and_encoding(&obj.data).ok_or_else(|| {
        crate::error::Error::CorruptObject(format!("commit {oid} missing author header"))
    })?;
    let author = commit_encoding::decode_bytes(encoding.as_deref(), &author_raw);
    Ok(author_contact(&author, mailmap))
}

/// Author header bytes and optional `encoding` header (Git continuation lines included).
fn scan_author_raw_and_encoding(raw: &[u8]) -> Option<(Vec<u8>, Option<String>)> {
    #[derive(Clone, Copy)]
    enum Continuation {
        Author,
        Ignore,
    }

    let mut pos = 0usize;
    let mut author_raw: Option<Vec<u8>> = None;
    let mut encoding: Option<String> = None;
    let mut cont = Continuation::Ignore;

    while pos < raw.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < raw.len() && raw[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &raw[line_start..line_end];
        let after_nl = line_end.saturating_add(1);
        if line.is_empty() {
            let author = author_raw?;
            return Some((author, encoding));
        }

        if line.first() == Some(&b' ') {
            let rest = line.get(1..).unwrap_or_default();
            if matches!(cont, Continuation::Author) {
                author_raw.as_mut()?.extend_from_slice(rest);
            }
            pos = after_nl;
            continue;
        }

        let key_end = line.iter().position(|&b| b == b' ')?;
        let key = &line[..key_end];
        let rest = line.get(key_end + 1..).unwrap_or_default();

        match key {
            b"author" => {
                author_raw = Some(rest.to_vec());
                cont = Continuation::Author;
            }
            b"encoding" => {
                encoding = std::str::from_utf8(rest)
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                cont = Continuation::Ignore;
            }
            _ => cont = Continuation::Ignore,
        }
        pos = after_nl;
    }
    None
}

fn author_contact(author_line: &str, mailmap: Option<&MailmapTable>) -> (String, String) {
    let line = strip_author_timestamp(author_line);
    let (name, email) = parse_contact(line);
    let Some(table) = mailmap else {
        return (name.unwrap_or_default(), email.unwrap_or_default());
    };
    map_contact_table(name.as_deref(), email.as_deref(), table)
}

fn strip_author_timestamp(ident: &str) -> &str {
    let ident = ident.trim();
    let Some(gt) = ident.rfind('>') else {
        return ident;
    };
    let rest = ident[gt + 1..].trim();
    if rest.is_empty() || rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        ident[..=gt].trim()
    } else {
        ident
    }
}

fn author_sort_key(group: &AuthorGroup) -> String {
    if group.email.is_empty() {
        group.name.clone()
    } else if group.name.is_empty() {
        format!("<{}>", group.email)
    } else {
        format!("{} <{}>", group.name, group.email)
    }
}
