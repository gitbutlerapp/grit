//! Shared ref listing: namespace selection, glob patterns, sort keys, and limits.
//!
//! Used by branch/tag list, `for-each-ref`-style queries, and in-memory remote ref
//! lists. Pattern rules follow `git for-each-ref` / `git branch --list`: patterns
//! match the **short** ref name (suffix after `refs/heads/`, `refs/tags/`, etc.),
//! and a trailing `/` selects a directory prefix.
//!
//! ```rust
//! use grit_lib::refs::listing::{query_refs, RefNamespace, RefQuery, RefQueryOptions, SortKey};
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! let repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let query = RefQuery {
//!     namespaces: vec![RefNamespace::Heads],
//!     ..RefQuery::default()
//! };
//! let _branches = query_refs(&repo, &query, &RefQueryOptions::default())?;
//! # Ok(())
//! # }
//! ```

use std::cmp::Ordering;
use std::ops::ControlFlow;

use thiserror::Error;

use crate::error::Error;
use crate::ident::committer_unix_seconds_for_ordering_bytes;
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::refs::store::{RefEntry, RefStore};
use crate::repo::Repository;
use crate::wildmatch::{wildmatch, WM_CASEFOLD};

/// Which ref namespace to include in a [`RefQuery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefNamespace {
    /// `refs/heads/*`
    Heads,
    /// `refs/remotes/*`
    Remotes,
    /// `refs/tags/*`
    Tags,
    /// Any ref whose full name starts with this prefix (for example `refs/notes/`).
    Custom(String),
}

impl RefNamespace {
    fn prefix(&self) -> &str {
        match self {
            Self::Heads => "refs/heads/",
            Self::Remotes => "refs/remotes/",
            Self::Tags => "refs/tags/",
            Self::Custom(p) => p.as_str(),
        }
    }
}

/// Primary sort key for [`RefQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Full ref name (`refname`), byte order.
    #[default]
    Name,
    /// `version:refname` — [`compare_version_names`] on the full ref name, then refname tie-break.
    Version,
    /// `creatordate` — tagger or committer epoch from object headers.
    Date,
}

/// Sort direction and key for ref listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefSort {
    /// Which field to sort by.
    pub key: SortKey,
    /// When true, reverse the primary ordering (Git `--reverse` / leading `-` on `--sort`).
    pub reverse: bool,
}

impl Default for RefSort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            reverse: false,
        }
    }
}

/// Filters and ordering for a ref listing query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefQuery {
    /// Namespaces to scan (union). When empty, defaults to [`RefNamespace::Heads`].
    pub namespaces: Vec<RefNamespace>,
    /// Glob patterns matched against the short ref name; empty means no pattern filter.
    pub patterns: Vec<String>,
    /// Sort key and direction.
    pub sort: RefSort,
    /// Maximum number of refs to return after sorting (`None` = unlimited).
    pub limit: Option<usize>,
}

/// Class of ref for display and JSON (`head`, `remote`, `tag`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// Local branch (`refs/heads/`).
    Head,
    /// Remote-tracking branch (`refs/remotes/`).
    Remote,
    /// Tag (`refs/tags/`).
    Tag,
    /// Any other ref namespace.
    Other,
}

/// One ref returned from [`References::query`](super::handle::References::query) or
/// [`query_listings_from_pairs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefListing {
    /// Full ref name (for example `refs/tags/v1.0`).
    pub full_name: String,
    /// Short name within its namespace (for example `v1.0` for tags).
    pub short_name: String,
    /// Object id stored at the ref (annotated tag object id for annotated tags).
    pub oid: ObjectId,
    /// Peeled object id when the ref store or ODB provides one (often a commit).
    pub peeled: Option<ObjectId>,
    /// Ref classification derived from `full_name`.
    pub kind: RefKind,
}

/// Extra inputs for pattern matching and version sort (explicit config, no env reads).
#[derive(Debug, Clone, Default)]
pub struct RefQueryOptions {
    /// `versionsort.suffix` values in priority order (Git multi-valued config).
    pub versionsort_suffixes: Vec<String>,
    /// When true, patterns use [`WM_CASEFOLD`] like `core.ignorecase`.
    pub ignore_case: bool,
}

/// Errors from ref query operations.
#[derive(Debug, Error)]
pub enum RefQueryError {
    /// Ref store iteration or resolution failed.
    #[error("ref store: {0}")]
    Store(#[from] Error),
    /// Creator-date sort was requested but no object database was supplied.
    #[error("creator-date sort requires an object database")]
    MissingObjectDatabase,
}

type QueryResult<T> = std::result::Result<T, RefQueryError>;

#[derive(Clone)]
struct CollectedRef {
    full_name: String,
    short_name: String,
    oid: ObjectId,
    peeled: Option<ObjectId>,
    kind: RefKind,
    creator_date: i64,
}

/// Classify `full_name` into a [`RefKind`].
#[must_use]
pub fn ref_kind_from_name(full_name: &str) -> RefKind {
    if full_name.starts_with("refs/heads/") {
        RefKind::Head
    } else if full_name.starts_with("refs/remotes/") {
        RefKind::Remote
    } else if full_name.starts_with("refs/tags/") {
        RefKind::Tag
    } else {
        RefKind::Other
    }
}

/// Strip the namespace prefix for `full_name` given the query namespace used to collect it.
#[must_use]
pub fn short_name_in_namespace(full_name: &str, namespace: &RefNamespace) -> String {
    let prefix = namespace.prefix();
    full_name
        .strip_prefix(prefix)
        .unwrap_or(full_name)
        .to_owned()
}

/// Whether `short_name` matches `pattern` (Git `for-each-ref` / branch list rules).
#[must_use]
pub fn pattern_matches_short_name(short_name: &str, pattern: &str, ignore_case: bool) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let mut flags = 0u32;
    if ignore_case {
        flags |= WM_CASEFOLD;
    }

    if pattern.ends_with('/') {
        let base = pattern.trim_end_matches('/');
        if short_name == base
            || short_name.starts_with(pattern)
            || short_name.starts_with(&format!("{base}/"))
        {
            return true;
        }
    }

    if wildmatch(pattern.as_bytes(), short_name.as_bytes(), flags) {
        return true;
    }

    // Literal prefix without glob metacharacters (Git `match_name_as_path` on short names).
    if !pattern.contains(['*', '?', '[', '\\']) {
        if short_name == pattern {
            return true;
        }
        if short_name.starts_with(&format!("{pattern}/")) {
            return true;
        }
    }
    false
}

/// Compare two ref names using Git's `version:refname` / `versioncmp` ordering.
///
/// `versionsort_suffixes` is the configured `versionsort.suffix` list (may be empty).
#[must_use]
pub fn compare_version_names(a: &str, b: &str, versionsort_suffixes: &[String]) -> Ordering {
    versioncmp(a, b, versionsort_suffixes)
}

/// Extract the epoch from a Git identity line (`Name <email> <epoch> <tz>`).
#[must_use]
pub fn epoch_from_identity(ident: &str) -> i64 {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse().unwrap_or(0)
    } else {
        0
    }
}

/// Creator date for a ref target: tagger date for tag objects, committer date for commits.
///
/// Reads only the object header region (stops at the first blank line). Returns `0` when
/// unknown.
#[must_use]
pub fn creator_date_from_bytes(kind: ObjectKind, data: &[u8]) -> i64 {
    let header = object_header_bytes(data);
    match kind {
        ObjectKind::Tag => {
            if let Some(line) = find_header_value(header, b"tagger") {
                committer_unix_seconds_for_ordering_bytes(line)
            } else {
                0
            }
        }
        ObjectKind::Commit => {
            if let Some(line) = find_header_value(header, b"committer") {
                committer_unix_seconds_for_ordering_bytes(line)
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// Creator date for `oid` in `odb` (see [`creator_date_from_bytes`]).
///
/// # Errors
///
/// Propagates object read failures.
pub fn creator_date(odb: &Odb, oid: &ObjectId) -> crate::error::Result<i64> {
    let obj = odb.read(oid)?;
    Ok(creator_date_from_bytes(obj.kind, &obj.data))
}

/// Run `query` over an in-memory iterator of `(full_name, oid)` pairs.
///
/// `peeled` is optional per entry; pass `None` when unknown. For creator-date sort,
/// `odb` must be `Some`.
///
/// # Errors
///
/// Returns [`RefQueryError::MissingObjectDatabase`] when sort key is [`SortKey::Date`] and
/// `odb` is `None`. Propagates object read errors as [`RefQueryError::Store`].
pub fn query_listings_from_pairs<I>(
    entries: I,
    query: &RefQuery,
    options: &RefQueryOptions,
    odb: Option<&Odb>,
) -> QueryResult<Vec<RefListing>>
where
    I: IntoIterator<Item = (String, ObjectId, Option<ObjectId>)>,
{
    let namespaces = effective_namespaces(query);
    let mut collected = Vec::new();
    for (full_name, oid, peeled) in entries {
        let kind = ref_kind_from_name(&full_name);
        let short_name = short_name_for_full(&full_name, kind);
        if !namespaces
            .iter()
            .any(|ns| full_name.starts_with(ns.prefix()))
        {
            continue;
        }
        if !patterns_match(&short_name, &query.patterns, options.ignore_case) {
            continue;
        }
        collected.push(CollectedRef {
            full_name,
            short_name,
            oid,
            peeled,
            kind,
            creator_date: 0,
        });
    }
    finish_query(collected, query, options, odb)
}

/// Collect refs from `store` under each namespace in `query`, then filter, sort, and limit.
///
/// # Errors
///
/// Propagates ref-store and object-read failures.
pub fn query_listings_on_store(
    store: &dyn RefStore,
    query: &RefQuery,
    options: &RefQueryOptions,
    odb: Option<&Odb>,
) -> QueryResult<Vec<RefListing>> {
    let namespaces = effective_namespaces(query);
    let mut collected = Vec::new();
    for ns in &namespaces {
        let prefix = ns.prefix();
        store.for_each_ref(prefix, &mut |entry: &RefEntry| {
            let oid = entry
                .peeled
                .or_else(|| match &entry.value {
                    crate::refs::store::RawRef::Direct(oid) => Some(*oid),
                    crate::refs::store::RawRef::Symbolic(target) => store.resolve(target).ok(),
                })
                .or_else(|| store.resolve(&entry.name).ok());
            let Some(oid) = oid else {
                return ControlFlow::Continue(());
            };
            let kind = ref_kind_from_name(&entry.name);
            let short_name = short_name_in_namespace(&entry.name, ns);
            if patterns_match(&short_name, &query.patterns, options.ignore_case) {
                collected.push(CollectedRef {
                    full_name: entry.name.clone(),
                    short_name,
                    oid: match &entry.value {
                        crate::refs::store::RawRef::Direct(oid) => *oid,
                        crate::refs::store::RawRef::Symbolic(_) => oid,
                    },
                    peeled: entry.peeled,
                    kind,
                    creator_date: 0,
                });
            }
            ControlFlow::Continue(())
        })?;
    }
    finish_query(collected, query, options, odb)
}

/// Query refs on `repo` (respects linked-worktree visibility and injected ref stores).
///
/// # Errors
///
/// Propagates ref-store and object-read failures.
pub fn query_refs(
    repo: &Repository,
    query: &RefQuery,
    options: &RefQueryOptions,
) -> QueryResult<Vec<RefListing>> {
    let git_dir = repo.git_dir();
    let store = repo.refs();
    let namespaces = effective_namespaces(query);
    let mut collected = Vec::new();
    for ns in &namespaces {
        let prefix = ns.prefix();
        store.for_each_ref(prefix, &mut |entry: &RefEntry| {
            if crate::worktree_ref::is_linked_worktree_git_dir(git_dir)
                && !crate::worktree_ref::ref_visible_from_worktree(git_dir, &entry.name)
            {
                return ControlFlow::Continue(());
            }
            let resolved = entry
                .peeled
                .or_else(|| match &entry.value {
                    crate::refs::store::RawRef::Direct(oid) => Some(*oid),
                    crate::refs::store::RawRef::Symbolic(target) => store.resolve(target).ok(),
                })
                .or_else(|| store.resolve(&entry.name).ok());
            let Some(resolved) = resolved else {
                return ControlFlow::Continue(());
            };
            let kind = ref_kind_from_name(&entry.name);
            let short_name = short_name_in_namespace(&entry.name, ns);
            if patterns_match(&short_name, &query.patterns, options.ignore_case) {
                let direct_oid = match &entry.value {
                    crate::refs::store::RawRef::Direct(oid) => *oid,
                    crate::refs::store::RawRef::Symbolic(_) => resolved,
                };
                collected.push(CollectedRef {
                    full_name: entry.name.clone(),
                    short_name,
                    oid: direct_oid,
                    peeled: entry.peeled,
                    kind,
                    creator_date: 0,
                });
            }
            ControlFlow::Continue(())
        })?;
    }
    finish_query(collected, query, options, Some(repo.odb()))
}

fn effective_namespaces(query: &RefQuery) -> Vec<RefNamespace> {
    if query.namespaces.is_empty() {
        vec![RefNamespace::Heads]
    } else {
        query.namespaces.clone()
    }
}

fn short_name_for_full(full_name: &str, kind: RefKind) -> String {
    match kind {
        RefKind::Head => full_name.strip_prefix("refs/heads/").unwrap_or(full_name),
        RefKind::Remote => full_name.strip_prefix("refs/remotes/").unwrap_or(full_name),
        RefKind::Tag => full_name.strip_prefix("refs/tags/").unwrap_or(full_name),
        RefKind::Other => full_name.strip_prefix("refs/").unwrap_or(full_name),
    }
    .to_owned()
}

fn patterns_match(short_name: &str, patterns: &[String], ignore_case: bool) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns
        .iter()
        .any(|p| pattern_matches_short_name(short_name, p, ignore_case))
}

fn finish_query(
    mut collected: Vec<CollectedRef>,
    query: &RefQuery,
    options: &RefQueryOptions,
    odb: Option<&Odb>,
) -> QueryResult<Vec<RefListing>> {
    if matches!(query.sort.key, SortKey::Date) {
        let odb = odb.ok_or(RefQueryError::MissingObjectDatabase)?;
        for entry in &mut collected {
            entry.creator_date = creator_date(odb, &entry.oid)?;
        }
    }
    let suffixes = &options.versionsort_suffixes;
    collected.sort_by(|a, b| {
        let primary = compare_entries(a, b, query.sort.key, suffixes);
        let primary = if query.sort.reverse {
            primary.reverse()
        } else {
            primary
        };
        if primary != Ordering::Equal {
            return primary;
        }
        a.full_name.cmp(&b.full_name)
    });
    if let Some(limit) = query.limit {
        collected.truncate(limit);
    }
    Ok(collected
        .into_iter()
        .map(|c| RefListing {
            full_name: c.full_name,
            short_name: c.short_name,
            oid: c.oid,
            peeled: c.peeled,
            kind: c.kind,
        })
        .collect())
}

fn compare_entries(
    a: &CollectedRef,
    b: &CollectedRef,
    key: SortKey,
    suffixes: &[String],
) -> Ordering {
    match key {
        SortKey::Name => a.full_name.cmp(&b.full_name),
        SortKey::Version => compare_version_names(&a.full_name, &b.full_name, suffixes),
        SortKey::Date => a.creator_date.cmp(&b.creator_date),
    }
}

fn object_header_bytes(data: &[u8]) -> &[u8] {
    if let Some(pos) = data.windows(2).position(|w| w == b"\n\n") {
        &data[..pos]
    } else {
        data
    }
}

fn find_header_value<'a>(header: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut idx = 0;
    while idx < header.len() {
        let line_end = header[idx..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| idx + p)
            .unwrap_or(header.len());
        let line = &header[idx..line_end];
        if line.is_empty() {
            break;
        }
        if line.len() > name.len() + 1 && line.starts_with(name) && line[name.len()] == b' ' {
            return Some(&line[name.len() + 1..]);
        }
        idx = line_end.saturating_add(1);
    }
    None
}

// Git `versioncmp` / glibc `strverscmp` state machine (behavior only; independent Rust code).

const S_N: u8 = 0;
const S_I: u8 = 3;
const S_F: u8 = 6;
const S_Z: u8 = 9;

const CMP: i8 = 2;
const LEN: i8 = 3;

fn versioncmp(s1: &str, s2: &str, suffixes: &[String]) -> Ordering {
    if s1 == s2 {
        return Ordering::Equal;
    }
    let b1 = s1.as_bytes();
    let b2 = s2.as_bytes();
    let mut i1 = 0usize;
    let mut i2 = 0usize;
    let mut c1 = *b1.get(i1).unwrap_or(&0);
    let mut c2 = *b2.get(i2).unwrap_or(&0);
    if !b1.is_empty() {
        i1 += 1;
    }
    if !b2.is_empty() {
        i2 += 1;
    }
    let mut state = S_N + digit_class(c1) + zero_class(c1);

    while c1 == c2 {
        if c1 == 0 {
            return Ordering::Equal;
        }
        state = next_state(state);
        c1 = *b1.get(i1).unwrap_or(&0);
        c2 = *b2.get(i2).unwrap_or(&0);
        if i1 < b1.len() {
            i1 += 1;
        }
        if i2 < b2.len() {
            i2 += 1;
        }
        state += digit_class(c1) + zero_class(c1);
    }

    let off = if c1 == 0 {
        i1
    } else if c2 == 0 {
        i2
    } else {
        i1.min(i2).saturating_sub(1)
    };
    if let Some(diff) = swap_prereleases(s1, s2, off, suffixes) {
        return diff;
    }

    let tail1 = &b1[i1.saturating_sub(1)..];
    let tail2 = &b2[i2.saturating_sub(1)..];
    let state = result_type(state, c2);
    match state {
        CMP => (c1 as i32).cmp(&(c2 as i32)),
        LEN => compare_numeric_tail(tail1, tail2, c1, c2),
        n => n.cmp(&0),
    }
}

fn digit_class(c: u8) -> u8 {
    u8::from(c.is_ascii_digit())
}

fn zero_class(c: u8) -> u8 {
    u8::from(c == b'0')
}

fn next_state(state: u8) -> u8 {
    const TABLE: [u8; 12] = [S_N, S_I, S_Z, S_N, S_I, S_I, S_N, S_F, S_F, S_N, S_F, S_Z];
    TABLE[state as usize]
}

fn result_type(state: u8, c2: u8) -> i8 {
    const TABLE: [i8; 36] = [
        CMP, CMP, CMP, CMP, LEN, CMP, CMP, CMP, CMP, CMP, -1, -1, 1, LEN, LEN, 1, LEN, LEN, CMP,
        CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, 1, 1, -1, CMP, CMP, -1, CMP, CMP,
    ];
    let idx = (state as usize) * 3 + (digit_class(c2) + zero_class(c2)) as usize;
    TABLE[idx]
}

fn compare_numeric_tail(p1: &[u8], p2: &[u8], c1: u8, c2: u8) -> Ordering {
    let mut p1 = p1;
    let mut p2 = p2;
    if c1.is_ascii_digit() {
        while p1.first().is_some_and(|c| c.is_ascii_digit()) {
            if !p2.first().is_some_and(|c| c.is_ascii_digit()) {
                return Ordering::Greater;
            }
            p1 = &p1[1..];
            p2 = &p2[1..];
        }
        if p2.first().is_some_and(|c| c.is_ascii_digit()) {
            return Ordering::Less;
        }
    }
    (c1 as i32).cmp(&(c2 as i32))
}

fn swap_prereleases(s1: &str, s2: &str, off: usize, suffixes: &[String]) -> Option<Ordering> {
    if suffixes.is_empty() {
        return None;
    }
    let mut match1 = SuffixMatch {
        conf_pos: None,
        start: off,
        len: 0,
    };
    let mut match2 = SuffixMatch {
        conf_pos: None,
        start: off,
        len: 0,
    };
    for (i, suffix) in suffixes.iter().enumerate() {
        let suffix_len = suffix.len();
        let start = off.saturating_sub(suffix_len);
        find_better_suffix(s1, suffix, start, i, &mut match1);
        find_better_suffix(s2, suffix, start, i, &mut match2);
    }
    match (match1.conf_pos, match2.conf_pos) {
        (None, None) => None,
        (Some(a), Some(b)) if a == b => None,
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        (Some(_), None) => Some(Ordering::Less),
        (None, Some(_)) => Some(Ordering::Greater),
    }
}

struct SuffixMatch {
    conf_pos: Option<usize>,
    start: usize,
    len: usize,
}

fn find_better_suffix(tag: &str, suffix: &str, start: usize, conf_pos: usize, m: &mut SuffixMatch) {
    if suffix.is_empty() {
        return;
    }
    let end = if m.len < suffix.len() {
        m.start
    } else {
        m.start.saturating_sub(1)
    };
    for i in start..=end.min(tag.len().saturating_sub(suffix.len())) {
        if tag[i..].starts_with(suffix) {
            m.conf_pos = Some(conf_pos);
            m.start = i;
            m.len = suffix.len();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn version_order_matches_git_tag_examples() {
        assert_eq!(
            compare_version_names("refs/tags/foo1.3", "refs/tags/foo1.10", &[]),
            Ordering::Less
        );
        assert_eq!(
            compare_version_names("refs/tags/foo1.10", "refs/tags/foo1.6", &[]),
            Ordering::Greater
        );
    }

    #[test]
    fn pattern_prefix_slash_and_glob() {
        assert!(pattern_matches_short_name("feature/x", "feature/", false));
        assert!(pattern_matches_short_name("v1.9", "v1.*", false));
        assert!(pattern_matches_short_name("release-rc1", "*rc*", false));
        assert!(!pattern_matches_short_name("other", "feature/", false));
    }

    #[test]
    fn versionsort_suffix_orders_prerelease() {
        let suffixes = vec!["-rc".to_owned()];
        assert_eq!(
            compare_version_names("v1.0", "v1.0-rc1", &suffixes),
            Ordering::Greater
        );
    }
}
