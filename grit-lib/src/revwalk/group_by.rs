//! Group walked commits for summary views (author shortlog-style reports).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::commit_encoding;
use crate::error::Result;
use crate::mailmap::{load_mailmap_table, MailmapTable};
use crate::objects::ObjectId;
use crate::pack::SingleLocalPackReader;
use crate::repo::Repository;
use crate::rev_list::try_for_each_bitmap_reachable_commit_from_specs;
use crate::revwalk::queries::{
    positive_specs_for_walk, rev_list_commits_for_author_group, rev_list_options_from_walk,
    MergeFilter, RevWalkParsedSpecs,
};
use crate::revwalk::RevWalkListOptions;

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
    if options.count_only {
        return count_authors_from_commit_oids(repo, oids, mailmap);
    }

    let odb = repo.odb();
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

    sort_author_groups(&mut groups);
    Ok(groups)
}

/// Resolve `tokens` and group by author (`--count` uses bitmap rev-list + pack-order reads).
///
/// # Errors
///
/// Propagates rev-parse, rev-list, ODB, and commit header errors.
pub fn group_commits_by_author_from_rev(
    repo: &Repository,
    tokens: &[&str],
    walk_options: &RevWalkListOptions,
    mailmap: Option<&MailmapTable>,
    options: GroupByAuthorOptions,
) -> Result<Vec<AuthorGroup>> {
    if options.count_only {
        return count_authors_from_rev_walk(repo, tokens, walk_options, mailmap);
    }
    let oids = repo.rev_walk().commits(tokens, walk_options)?;
    group_commits_by_author(repo, &oids, mailmap, options)
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

struct AuthorCounter {
    entries: Vec<(String, String, usize)>,
    index: HashMap<u128, usize>,
}

impl AuthorCounter {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn add_owned(&mut self, name: String, email: String) {
        self.add_stored(&name, &email);
    }

    fn add_with_mailmap(&mut self, name: &str, email: &str, mailmap: Option<&MailmapTable>) {
        let (stored_name, stored_email) = match mailmap {
            Some(table) => {
                let (n, e) = table.map_user_cow(name, email);
                (n.into_owned(), e.into_owned())
            }
            None => (name.to_owned(), email.to_owned()),
        };
        self.add_stored(&stored_name, &stored_email);
    }

    fn add_stored(&mut self, name: &str, email: &str) {
        let key = author_identity_key(name, email);
        if let Some(&idx) = self.index.get(&key) {
            let entry = &self.entries[idx];
            if entry.0 == name && entry.1 == email {
                self.entries[idx].2 += 1;
                return;
            }
        }
        let idx = self.entries.len();
        self.entries.push((name.to_owned(), email.to_owned(), 1));
        self.index.insert(key, idx);
    }

    fn into_sorted_groups(self) -> Vec<AuthorGroup> {
        let mut groups: Vec<AuthorGroup> = self
            .entries
            .into_iter()
            .map(|(name, email, count)| AuthorGroup {
                name,
                email,
                count,
                commits: Vec::new(),
            })
            .collect();
        sort_author_groups(&mut groups);
        groups
    }
}

fn count_authors_from_rev_walk(
    repo: &Repository,
    tokens: &[&str],
    walk_options: &RevWalkListOptions,
    mailmap: Option<&MailmapTable>,
) -> Result<Vec<AuthorGroup>> {
    let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, walk_options)?;
    if shortlog_stack_walk_compatible(walk_options, &parsed) {
        let mut tips = crate::rev_list::resolve_revision_specs_to_commits(repo, &positive)?;
        if all_refs {
            tips.extend(crate::rev_list::all_ref_commit_tips_for_walk(repo)?);
        }
        tips.sort();
        tips.dedup();
        let exclude = crate::rev_list::resolve_revision_specs_to_commits(repo, &parsed.negative)?;
        if commit_graph_available(repo) {
            if let Some(groups) = repo.odb().with_pack_read_context(|| {
                count_authors_stack_walk(repo, &tips, &exclude, walk_options.first_parent, mailmap)
            })? {
                return Ok(groups);
            }
        }
    }
    let mut opts = rev_list_options_from_walk(repo, walk_options, &parsed, all_refs, false)?;
    opts.use_bitmap_index = true;
    let objects_dir = repo.git_dir().join("objects");
    let mut counter = AuthorCounter::new();
    let mut pack_reader: Option<SingleLocalPackReader> = None;
    let ran = repo.odb().with_pack_read_context(|| {
        try_for_each_bitmap_reachable_commit_from_specs(
            repo,
            &positive,
            &parsed.negative,
            &opts,
            |oid| {
                accumulate_author_for_oid(
                    repo,
                    &objects_dir,
                    &mut pack_reader,
                    oid,
                    mailmap,
                    &mut counter,
                )
            },
        )
    })?;
    if ran {
        return Ok(counter.into_sorted_groups());
    }
    let oids = rev_list_commits_for_author_group(repo, tokens, walk_options)?;
    count_authors_from_commit_oids(repo, &oids, mailmap)
}

fn commit_graph_available(repo: &Repository) -> bool {
    let objects = repo.git_dir().join("objects");
    objects.join("info/commit-graph").is_file() || objects.join("info/commit-graphs").is_dir()
}

fn shortlog_stack_walk_compatible(walk: &RevWalkListOptions, parsed: &RevWalkParsedSpecs) -> bool {
    walk.paths.is_empty()
        && walk.since.is_none()
        && walk.until.is_none()
        && walk.merges == MergeFilter::Any
        && !walk.reverse
        && walk.skip == 0
        && walk.limit.is_none()
        && walk.commit_filter.as_ref().is_none_or(|f| !f.is_active())
        && parsed.symmetric_left.is_none()
        && parsed.symmetric_right.is_none()
}

fn count_authors_stack_walk(
    repo: &Repository,
    tips: &[ObjectId],
    exclude: &[ObjectId],
    first_parent: bool,
    mailmap: Option<&MailmapTable>,
) -> Result<Option<Vec<AuthorGroup>>> {
    if tips.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let objects_dir = repo.git_dir().join("objects");
    let mut pack_reader =
        SingleLocalPackReader::open_allow_loose_for_commits(&objects_dir, &tips[0])?;
    let Some(reader) = pack_reader.as_mut() else {
        return Ok(None);
    };
    let excluded: HashSet<ObjectId> = if exclude.is_empty() {
        HashSet::new()
    } else {
        let mut graph = crate::rev_list::CommitGraph::with_commit_graph(repo, false, true);
        crate::rev_list::walk_closure(&mut graph, exclude)?
    };
    let mut graph = crate::rev_list::CommitGraph::with_commit_graph(repo, first_parent, true);
    let mut counter = AuthorCounter::new();
    let mut stack: Vec<ObjectId> = tips.to_vec();
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if excluded.contains(&oid) || !seen.insert(oid) {
            continue;
        }
        if let Some(prefix) = reader.read_commit_author_prefix_unordered(&oid)? {
            if accumulate_author_count(prefix, mailmap, &mut counter).is_err() {
                let obj = reader.read_object(&oid)?;
                accumulate_author_count(&obj.data, mailmap, &mut counter)?;
            }
        } else {
            let obj = reader.read_object(&oid)?;
            accumulate_author_count(&obj.data, mailmap, &mut counter)?;
        }
        stack.extend(graph.parents_of(oid)?);
    }
    Ok(Some(counter.into_sorted_groups()))
}

fn accumulate_author_for_oid(
    repo: &Repository,
    objects_dir: &Path,
    pack_reader: &mut Option<SingleLocalPackReader>,
    oid: ObjectId,
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
) -> Result<()> {
    if pack_reader.is_none() {
        *pack_reader = SingleLocalPackReader::open_allow_loose_for_commits(objects_dir, &oid)?;
    }
    if let Some(reader) = pack_reader.as_mut() {
        if let Some(prefix) = reader.read_commit_author_prefix(&oid)? {
            if accumulate_author_count(prefix, mailmap, counter).is_ok() {
                return Ok(());
            }
        }
        let obj = reader.read_object(&oid)?;
        accumulate_author_count(&obj.data, mailmap, counter)?;
        return Ok(());
    }
    let odb = repo.odb();
    let obj = odb.read(&oid)?;
    accumulate_author_count(&obj.data, mailmap, counter)
}

fn count_authors_from_commit_oids(
    repo: &Repository,
    oids: &[ObjectId],
    mailmap: Option<&MailmapTable>,
) -> Result<Vec<AuthorGroup>> {
    let objects_dir = repo.git_dir().join("objects");
    let mut counter = AuthorCounter::new();
    let used_single_pack = repo.odb().with_pack_read_context(|| {
        count_authors_single_pack(&objects_dir, oids, mailmap, &mut counter)
    })?;
    if used_single_pack {
        return Ok(counter.into_sorted_groups());
    }
    let odb = repo.odb();
    for oid in oids {
        let obj = odb.read(oid)?;
        accumulate_author_count(&obj.data, mailmap, &mut counter)?;
    }
    Ok(counter.into_sorted_groups())
}

fn count_authors_single_pack(
    objects_dir: &Path,
    oids: &[ObjectId],
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
) -> Result<bool> {
    let Some(first) = oids.first() else {
        return Ok(true);
    };
    let Some(mut reader) = SingleLocalPackReader::open_allow_loose_for_commits(objects_dir, first)?
    else {
        return Ok(false);
    };
    for oid in oids {
        if let Some(prefix) = reader.read_commit_author_prefix(oid)? {
            if accumulate_author_count(prefix, mailmap, counter).is_ok() {
                continue;
            }
        }
        let obj = reader.read_object(oid)?;
        accumulate_author_count(&obj.data, mailmap, counter)?;
    }
    Ok(true)
}

fn author_identity_key(name: &str, email: &str) -> u128 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    email.hash(&mut hasher);
    let lo = hasher.finish();
    email.to_ascii_lowercase().hash(&mut hasher);
    name.to_ascii_lowercase().hash(&mut hasher);
    let hi = hasher.finish();
    ((hi as u128) << 64) | lo as u128
}

fn sort_author_groups(groups: &mut [AuthorGroup]) {
    groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| author_sort_key(a).cmp(&author_sort_key(b)))
    });
}

fn author_for_commit(
    odb: &crate::odb::Odb,
    oid: ObjectId,
    mailmap: Option<&MailmapTable>,
) -> Result<(String, String)> {
    let obj = odb.read(&oid)?;
    author_from_commit_bytes(&obj.data, mailmap)
}

fn accumulate_author_count(
    data: &[u8],
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
) -> Result<()> {
    if let Ok((name, email)) = author_slices_from_commit_bytes(data) {
        counter.add_with_mailmap(name, email, mailmap);
        return Ok(());
    }
    let (name, email) = author_from_commit_bytes(data, mailmap)?;
    counter.add_owned(name, email);
    Ok(())
}

fn author_slices_from_commit_bytes(data: &[u8]) -> Result<(&str, &str)> {
    let author_bytes = scan_author_line_borrowed(data)
        .ok_or_else(|| crate::error::Error::CorruptObject("commit missing author header".into()))?;
    let line = std::str::from_utf8(author_bytes)
        .map_err(|_| crate::error::Error::CorruptObject("commit author is not UTF-8".into()))?;
    Ok(parse_contact_fields(strip_author_timestamp(line.trim())))
}

/// Author identity bytes (timestamp included) when the header is a single line with no `encoding`.
fn scan_author_line_borrowed(raw: &[u8]) -> Option<&[u8]> {
    let mut pos = 0usize;
    let mut author: Option<&[u8]> = None;
    while pos < raw.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < raw.len() && raw[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &raw[line_start..line_end];
        let after_nl = line_end.saturating_add(1);
        if line.is_empty() {
            return author;
        }
        if line.first() == Some(&b' ') {
            return None;
        }
        let key_end = line.iter().position(|&b| b == b' ')?;
        let key = &line[..key_end];
        let rest = line.get(key_end + 1..).unwrap_or_default();
        match key {
            b"author" => author = Some(rest),
            b"encoding" => return None,
            _ => {}
        }
        pos = after_nl;
    }
    None
}

fn author_from_commit_bytes(
    data: &[u8],
    mailmap: Option<&MailmapTable>,
) -> Result<(String, String)> {
    let (author_raw, encoding) = scan_author_raw_and_encoding(data)
        .ok_or_else(|| crate::error::Error::CorruptObject("commit missing author header".into()))?;
    let author_bytes = author_raw.as_bytes();
    if encoding.is_none() {
        if let Ok(line) = std::str::from_utf8(author_bytes) {
            return Ok(map_author_line(line, mailmap));
        }
    }
    let author = commit_encoding::decode_bytes(encoding.as_deref(), author_bytes);
    Ok(map_author_line(&author, mailmap))
}

fn map_author_line(line: &str, mailmap: Option<&MailmapTable>) -> (String, String) {
    let line = strip_author_timestamp(line);
    let (name, email) = parse_contact_fields(line);
    let Some(table) = mailmap else {
        return (name.to_owned(), email.to_owned());
    };
    table.map_user_str(name, email)
}

fn parse_contact_fields(contact: &str) -> (&str, &str) {
    let contact = contact.trim();
    if let Some(lt) = contact.find('<') {
        if let Some(gt) = contact.find('>') {
            let name = contact[..lt].trim();
            let email = contact[lt + 1..gt].trim();
            return (name, email);
        }
    }
    if contact.contains('@') && !contact.chars().any(char::is_whitespace) {
        return ("", contact);
    }
    (contact, "")
}

enum AuthorRaw<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl AuthorRaw<'_> {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(b) => b,
            Self::Owned(v) => v.as_slice(),
        }
    }

    fn push_continuation(&mut self, rest: &[u8]) {
        match self {
            Self::Borrowed(b) => {
                let mut owned = b.to_vec();
                owned.extend_from_slice(rest);
                *self = Self::Owned(owned);
            }
            Self::Owned(v) => v.extend_from_slice(rest),
        }
    }
}

/// Author header bytes and optional `encoding` header (Git continuation lines included).
fn scan_author_raw_and_encoding(raw: &[u8]) -> Option<(AuthorRaw<'_>, Option<String>)> {
    #[derive(Clone, Copy)]
    enum Continuation {
        Author,
        Ignore,
    }

    let mut pos = 0usize;
    let mut author_raw: Option<AuthorRaw<'_>> = None;
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
            return Some((author_raw?, encoding));
        }

        if line.first() == Some(&b' ') {
            let rest = line.get(1..).unwrap_or_default();
            if matches!(cont, Continuation::Author) {
                author_raw.as_mut()?.push_continuation(rest);
            }
            pos = after_nl;
            continue;
        }

        let key_end = line.iter().position(|&b| b == b' ')?;
        let key = &line[..key_end];
        let rest = line.get(key_end + 1..).unwrap_or_default();

        match key {
            b"author" => {
                author_raw = Some(AuthorRaw::Borrowed(rest));
                cont = Continuation::Author;
            }
            b"encoding" => {
                encoding = std::str::from_utf8(rest).ok().map(str::to_owned);
                cont = Continuation::Ignore;
            }
            _ => cont = Continuation::Ignore,
        }
        pos = after_nl;
    }
    None
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
