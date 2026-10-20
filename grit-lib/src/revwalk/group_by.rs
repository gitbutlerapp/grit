//! Group walked commits for summary views (author shortlog-style reports).

use std::collections::HashMap;

use crate::error::Result;
use crate::mailmap::{load_mailmap_table, map_contact_table, parse_contact, MailmapTable};
use crate::objects::{parse_commit, ObjectId};
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
    /// Commit ids in walk order within the group.
    pub commits: Vec<ObjectId>,
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
) -> Result<Vec<AuthorGroup>> {
    let odb = repo.odb();
    let mut buckets: HashMap<(String, String), Vec<ObjectId>> = HashMap::new();

    for oid in oids {
        let obj = odb.read(oid)?;
        let parsed = parse_commit(&obj.data)?;
        let (name, email) = author_contact(&parsed.author, mailmap);
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
