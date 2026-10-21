//! Group walked commits for summary views (author shortlog-style reports).

use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::path::Path;
use std::sync::Arc;

use crate::commit_encoding;
use crate::error::{Error, Result};
use crate::mailmap::{load_mailmap_table, MailmapTable};
use crate::objects::{ObjectId, ObjectKind};
use crate::pack::{read_object_from_pack_at_offset, PackIndex, SingleLocalPackReader};
use crate::repo::Repository;
use crate::rev_list::{
    try_bitmap_reachable_commit_set_for_specs, try_for_each_bitmap_reachable_commit_from_specs,
};
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
            entries: Vec::with_capacity(512),
            index: HashMap::with_capacity(4096),
        }
    }

    fn absorb(&mut self, other: Self) {
        for (name, email, count) in other.entries {
            let key = author_identity_key(&name, &email);
            if let Some(&idx) = self.index.get(&key) {
                self.entries[idx].2 += count;
            } else {
                let idx = self.entries.len();
                self.entries.push((name, email, count));
                self.index.insert(key, idx);
            }
        }
    }

    fn add_owned(&mut self, name: String, email: String) {
        self.add_borrowed(&name, &email);
    }

    fn add_with_mailmap(&mut self, name: &str, email: &str, mailmap: Option<&MailmapTable>) {
        match mailmap {
            Some(table) => {
                let (n, e) = table.map_user_cow(name, email);
                self.add_borrowed(n.as_ref(), e.as_ref());
            }
            None => self.add_borrowed(name, email),
        }
    }

    fn add_borrowed(&mut self, name: &str, email: &str) {
        let key = author_identity_key(name, email);
        if let Some(&idx) = self.index.get(&key) {
            self.entries[idx].2 += 1;
            return;
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
    if shortlog_stack_walk_compatible(walk_options, &parsed) && commit_graph_available(repo) {
        let mut tips = crate::rev_list::resolve_revision_specs_to_commits(repo, &positive)?;
        if all_refs {
            tips.extend(crate::rev_list::all_ref_commit_tips_for_walk(repo)?);
        }
        tips.sort();
        tips.dedup();
        let exclude = crate::rev_list::resolve_revision_specs_to_commits(repo, &parsed.negative)?;
        return repo.odb().with_pack_read_context(|| {
            count_authors_stack_walk(
                repo,
                &tips,
                &exclude,
                walk_options.first_parent,
                mailmap,
                true,
            )
        });
    }
    if let Some(groups) = try_count_authors_via_bitmap(
        repo,
        &positive,
        &parsed.negative,
        walk_options,
        &parsed,
        all_refs,
        mailmap,
    )? {
        return Ok(groups);
    }
    if shortlog_stack_walk_compatible(walk_options, &parsed) {
        let mut tips = crate::rev_list::resolve_revision_specs_to_commits(repo, &positive)?;
        if all_refs {
            tips.extend(crate::rev_list::all_ref_commit_tips_for_walk(repo)?);
        }
        tips.sort();
        tips.dedup();
        let exclude = crate::rev_list::resolve_revision_specs_to_commits(repo, &parsed.negative)?;
        return repo.odb().with_pack_read_context(|| {
            count_authors_stack_walk(
                repo,
                &tips,
                &exclude,
                walk_options.first_parent,
                mailmap,
                false,
            )
        });
    }
    let oids = rev_list_commits_for_author_group(repo, tokens, walk_options)?;
    count_authors_from_commit_oids(repo, &oids, mailmap)
}

fn try_count_authors_via_bitmap(
    repo: &Repository,
    positive: &[String],
    negative: &[String],
    walk_options: &RevWalkListOptions,
    parsed: &RevWalkParsedSpecs,
    all_refs: bool,
    mailmap: Option<&MailmapTable>,
) -> Result<Option<Vec<AuthorGroup>>> {
    let mut opts = rev_list_options_from_walk(repo, walk_options, parsed, all_refs, false)?;
    opts.use_bitmap_index = true;
    let objects_dir = repo.git_dir().join("objects");
    repo.odb().with_pack_read_context(|| {
        if let Some((index, reachable)) =
            try_bitmap_reachable_commit_set_for_specs(repo, positive, negative, &opts)?
        {
            if let Some(pack_idx) = index.backing_pack_index() {
                let positions = reachable.reachable_commit_bitmap_positions().map_err(|e| {
                    Error::CorruptObject(format!("pack bitmap: {e}"))
                })?;
                let mut counter = count_authors_at_pack_rows(
                    repo,
                    &objects_dir,
                    &index,
                    &pack_idx,
                    &positions,
                    mailmap,
                )?;
                for (oid, kind) in reachable.extended_objects() {
                    if *kind == ObjectKind::Commit {
                        let mut pack_reader = None;
                        accumulate_author_for_oid(
                            repo,
                            &objects_dir,
                            &mut pack_reader,
                            *oid,
                            mailmap,
                            &mut counter,
                            false,
                        )?;
                    }
                }
                return Ok(Some(counter.into_sorted_groups()));
            }
        }

        let mut counter = AuthorCounter::new();
        let mut pack_reader: Option<SingleLocalPackReader> = None;
        let ran = try_for_each_bitmap_reachable_commit_from_specs(
            repo,
            positive,
            negative,
            &opts,
            |oid| {
                accumulate_author_for_oid(
                    repo,
                    &objects_dir,
                    &mut pack_reader,
                    oid,
                    mailmap,
                    &mut counter,
                    true,
                )
            },
        )?;
        Ok(ran.then(|| counter.into_sorted_groups()))
    })
}

fn accumulate_author_from_pack_row(
    reader: &mut SingleLocalPackReader,
    row: usize,
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
) -> Result<bool> {
    reader.clear_commit_header_prefix();
    if let Some(prefix) = reader.read_commit_author_prefix_at_pack_row(row)? {
        if accumulate_author_count(prefix, mailmap, counter).is_ok() {
            return Ok(true);
        }
        reader.clear_commit_header_prefix();
    }
    Ok(false)
}

const PARALLEL_AUTHOR_COUNT_MIN: usize = 8192;

fn count_authors_at_pack_rows(
    repo: &Repository,
    objects_dir: &Path,
    index: &crate::pack_bitmap::BitmapIndex,
    pack_idx: &Arc<PackIndex>,
    positions: &[u32],
    mailmap: Option<&MailmapTable>,
) -> Result<AuthorCounter> {
    let mut entries: Vec<(usize, ObjectId)> = positions
        .iter()
        .filter_map(|pos| {
            let row = index.pack_index_row_for_bitmap_position(*pos)?;
            let oid = index.oid_at(*pos).ok()?;
            Some((row as usize, oid))
        })
        .collect();
    if entries.is_empty() {
        return Ok(AuthorCounter::new());
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8);
    if entries.len() < PARALLEL_AUTHOR_COUNT_MIN || threads <= 1 {
        let mut reader = SingleLocalPackReader::open_for_pack_index(
            objects_dir,
            Arc::clone(pack_idx),
        )?
        .ok_or_else(|| Error::CorruptObject("missing pack reader".into()))?;
        let mut counter = AuthorCounter::new();
        count_authors_at_pack_rows_serial(
            repo,
            &mut reader,
            pack_idx.as_ref(),
            &entries,
            mailmap,
            &mut counter,
        )?;
        return Ok(counter);
    }

    entries.sort_unstable_by_key(|(row, _)| *row);
    entries.dedup_by_key(|(row, _)| *row);
    let chunk = entries.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for chunk_entries in entries.chunks(chunk) {
            let chunk_entries = chunk_entries.to_vec();
            let pack_idx = Arc::clone(pack_idx);
            handles.push(scope.spawn(move || -> Result<AuthorCounter> {
                let mut reader = SingleLocalPackReader::open_for_pack_index(
                    objects_dir,
                    Arc::clone(&pack_idx),
                )?
                .ok_or_else(|| Error::CorruptObject("missing pack reader".into()))?;
                let mut counter = AuthorCounter::new();
                count_authors_at_pack_rows_serial(
                    repo,
                    &mut reader,
                    pack_idx.as_ref(),
                    &chunk_entries,
                    mailmap,
                    &mut counter,
                )?;
                Ok(counter)
            }));
        }
        let mut merged = AuthorCounter::new();
        for handle in handles {
            merged.absorb(handle.join().map_err(|_| {
                Error::CorruptObject("author count worker panicked".into())
            })??);
        }
        Ok(merged)
    })
}

fn count_authors_at_pack_rows_serial(
    repo: &Repository,
    reader: &mut SingleLocalPackReader,
    pack_idx: &PackIndex,
    entries: &[(usize, ObjectId)],
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
) -> Result<()> {
    for (row, oid) in entries {
        reader.clear_commit_header_prefix();
        if accumulate_author_from_pack_row(reader, *row, mailmap, counter)? {
            continue;
        }
        reader.clear_commit_header_prefix();
        let read_from_pack = pack_idx
            .offset_at(*row)
            .ok()
            .and_then(|offset| read_object_from_pack_at_offset(pack_idx, offset).ok());
        match read_from_pack {
            Some(obj) => accumulate_author_count(&obj.data, mailmap, counter)?,
            None => {
                let obj = repo.odb().read(oid)?;
                accumulate_author_count(&obj.data, mailmap, counter)?;
            }
        }
    }
    Ok(())
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

fn load_commit_bytes_and_count_author(
    repo: &Repository,
    objects_dir: &Path,
    pack_reader: &mut Option<SingleLocalPackReader>,
    oid: ObjectId,
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
    monotonic_pack_probe: bool,
) -> Result<Vec<u8>> {
    for _ in 0..3 {
        if pack_reader.is_none() {
            *pack_reader = SingleLocalPackReader::open_allow_loose_for_commits(objects_dir, &oid)?;
        }
        let Some(reader) = pack_reader.as_mut() else {
            break;
        };
        reader.clear_commit_header_prefix();
        let prefix = if monotonic_pack_probe {
            reader.read_commit_author_prefix(&oid)?
        } else {
            reader.read_commit_author_prefix_unordered(&oid)?
        };
        if let Some(prefix) = prefix {
            if accumulate_author_count(prefix, mailmap, counter).is_ok() {
                return Ok(prefix.to_vec());
            }
            reader.clear_commit_header_prefix();
        } else if !reader.contains_oid(&oid)? {
            reader.clear_commit_header_prefix();
            *pack_reader = None;
            continue;
        } else {
            reader.clear_commit_header_prefix();
        }
        match reader.read_object(&oid) {
            Ok(obj) => {
                reader.clear_commit_header_prefix();
                accumulate_author_count(&obj.data, mailmap, counter)?;
                return Ok(obj.data);
            }
            Err(Error::ObjectNotFound(_)) => {
                reader.clear_commit_header_prefix();
                *pack_reader = None;
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    if let Some(reader) = pack_reader.as_mut() {
        reader.clear_commit_header_prefix();
    }
    *pack_reader = None;
    let obj = repo.odb().read(&oid)?;
    accumulate_author_count(&obj.data, mailmap, counter)?;
    Ok(obj.data)
}

fn accumulate_author_for_oid(
    repo: &Repository,
    objects_dir: &Path,
    pack_reader: &mut Option<SingleLocalPackReader>,
    oid: ObjectId,
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
    monotonic_pack_probe: bool,
) -> Result<()> {
    load_commit_bytes_and_count_author(
        repo,
        objects_dir,
        pack_reader,
        oid,
        mailmap,
        counter,
        monotonic_pack_probe,
    )
    .map(|_| ())
}

fn count_authors_stack_walk(
    repo: &Repository,
    tips: &[ObjectId],
    exclude: &[ObjectId],
    first_parent: bool,
    mailmap: Option<&MailmapTable>,
    use_commit_graph: bool,
) -> Result<Vec<AuthorGroup>> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let objects_dir = repo.git_dir().join("objects");
    let excluded: HashSet<ObjectId> = if exclude.is_empty() {
        HashSet::new()
    } else {
        let mut graph = crate::rev_list::CommitGraph::with_commit_graph(
            repo,
            false,
            use_commit_graph,
        );
        crate::rev_list::walk_closure(&mut graph, exclude)?
    };
    let mut graph = use_commit_graph.then(|| {
        crate::rev_list::CommitGraph::with_commit_graph(repo, first_parent, true)
    });
    let mut pack_reader: Option<SingleLocalPackReader> = None;
    let mut counter = AuthorCounter::new();
    let mut stack: Vec<ObjectId> = tips.to_vec();
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if excluded.contains(&oid) || !seen.insert(oid) {
            continue;
        }
        let commit_bytes = load_commit_bytes_and_count_author(
            repo,
            &objects_dir,
            &mut pack_reader,
            oid,
            mailmap,
            &mut counter,
            false,
        )?;
        let parents = if let Some(graph) = graph.as_mut() {
            graph.parents_of(oid)?
        } else {
            parent_oids_from_commit_bytes(&commit_bytes, first_parent)?
        };
        stack.extend(parents);
    }
    Ok(counter.into_sorted_groups())
}

fn parent_oids_from_commit_bytes(data: &[u8], first_parent: bool) -> Result<Vec<ObjectId>> {
    let mut parents = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            break;
        }
        if line.starts_with(b"parent ") {
            let hex = std::str::from_utf8(&line[7..]).map_err(|_| {
                Error::CorruptObject("commit parent is not UTF-8".into())
            })?;
            parents.push(ObjectId::from_hex(hex.trim())?);
            if first_parent {
                break;
            }
        } else if line.starts_with(b"author ") || line.starts_with(b"committer ") {
            break;
        }
    }
    Ok(parents)
}

fn count_authors_from_commit_oids(
    repo: &Repository,
    oids: &[ObjectId],
    mailmap: Option<&MailmapTable>,
) -> Result<Vec<AuthorGroup>> {
    let objects_dir = repo.git_dir().join("objects");
    let mut counter = AuthorCounter::new();
    repo.odb().with_pack_read_context(|| {
        count_authors_single_pack(repo, &objects_dir, oids, mailmap, &mut counter)
    })?;
    Ok(counter.into_sorted_groups())
}

fn count_authors_single_pack(
    repo: &Repository,
    objects_dir: &Path,
    oids: &[ObjectId],
    mailmap: Option<&MailmapTable>,
    counter: &mut AuthorCounter,
) -> Result<()> {
    let mut pack_reader: Option<SingleLocalPackReader> = None;
    for oid in oids {
        accumulate_author_for_oid(
            repo,
            objects_dir,
            &mut pack_reader,
            *oid,
            mailmap,
            counter,
            true,
        )?;
    }
    Ok(())
}

fn author_identity_key(name: &str, email: &str) -> u128 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_ascii_insensitive(name, &mut hasher);
    hash_ascii_insensitive(email, &mut hasher);
    let lo = hasher.finish();
    hasher = std::collections::hash_map::DefaultHasher::new();
    hash_ascii_insensitive(email, &mut hasher);
    hash_ascii_insensitive(name, &mut hasher);
    let hi = hasher.finish();
    ((hi as u128) << 64) | lo as u128
}

fn hash_ascii_insensitive(s: &str, hasher: &mut impl Hasher) {
    for b in s.bytes() {
        hasher.write(&[b.to_ascii_lowercase()]);
    }
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
