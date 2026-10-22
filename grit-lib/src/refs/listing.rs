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
use std::collections::BTreeMap;
use std::ops::ControlFlow;

use thiserror::Error;

use crate::error::Error;
use crate::ident::committer_unix_seconds_for_ordering_bytes;
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::refs::query_reachability;
use crate::refs::store::{RefEntry, RefStore};
use crate::refs::version_sort::compare_refname_versions;
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
    /// Keep refs whose peeled commit contains every listed commit (`--contains`).
    pub contains: Vec<ObjectId>,
    /// Exclude refs whose peeled commit contains any listed commit (`--no-contains`).
    pub no_contains: Vec<ObjectId>,
    /// Keep branch tips merged into every listed commit (`--merged`).
    pub merged: Vec<ObjectId>,
    /// Exclude branch tips merged into any listed commit (`--no-merged`).
    pub no_merged: Vec<ObjectId>,
    /// Keep refs whose oid or tag peel chain matches any listed object (`--points-at`).
    pub points_at: Vec<ObjectId>,
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
    /// Filter object id is not present in the object database.
    #[error("object {0} not found")]
    MissingObject(ObjectId),
    /// `--contains` / `--merged` target does not resolve to a commit.
    #[error("object {0} is not a commit")]
    NotCommit(ObjectId),
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
        if ascii_eq(short_name, base, ignore_case)
            || ascii_starts_with(short_name, pattern, ignore_case)
        {
            return true;
        }
    }

    if wildmatch(pattern.as_bytes(), short_name.as_bytes(), flags) {
        return true;
    }

    // Literal prefix without glob metacharacters (Git `match_name_as_path` on short names).
    if !pattern.contains(['*', '?', '[', '\\']) {
        if ascii_eq(short_name, pattern, ignore_case) {
            return true;
        }
        if ascii_starts_with_path_segment(short_name, pattern, ignore_case) {
            return true;
        }
    }
    false
}

fn ascii_eq(left: &str, right: &str, ignore_case: bool) -> bool {
    ascii_eq_bytes(left.as_bytes(), right.as_bytes(), ignore_case)
}

fn ascii_eq_bytes(left: &[u8], right: &[u8], ignore_case: bool) -> bool {
    if !ignore_case {
        return left == right;
    }
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn ascii_starts_with(name: &str, prefix: &str, ignore_case: bool) -> bool {
    ascii_starts_with_bytes(name.as_bytes(), prefix.as_bytes(), ignore_case)
}

fn ascii_starts_with_bytes(name: &[u8], prefix: &[u8], ignore_case: bool) -> bool {
    if name.len() < prefix.len() {
        return false;
    }
    if !ignore_case {
        return name.starts_with(prefix);
    }
    name[..prefix.len()]
        .iter()
        .zip(prefix.iter())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn ascii_starts_with_path_segment(name: &str, pattern: &str, ignore_case: bool) -> bool {
    let name = name.as_bytes();
    let pattern = pattern.as_bytes();
    if name.len() < pattern.len() + 1 || name[pattern.len()] != b'/' {
        return false;
    }
    ascii_starts_with_bytes(name, pattern, ignore_case)
}

/// Compare two ref names using Git's `version:refname` / `versioncmp` ordering.
///
/// `versionsort_suffixes` is the configured `versionsort.suffix` list (may be empty).
#[must_use]
pub fn compare_version_names(a: &str, b: &str, versionsort_suffixes: &[String]) -> Ordering {
    compare_refname_versions(a, b, versionsort_suffixes)
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
    let mut collected = BTreeMap::new();
    for (full_name, oid, peeled) in entries {
        if !namespaces
            .iter()
            .any(|ns| full_name.starts_with(ns.prefix()))
        {
            continue;
        }
        let short_name = short_name_for_query(&full_name, &namespaces);
        if !patterns_match(&short_name, &query.patterns, options.ignore_case) {
            continue;
        }
        let kind = ref_kind_from_name(&full_name);
        collected.entry(full_name.clone()).or_insert(CollectedRef {
            full_name,
            short_name,
            oid,
            peeled,
            kind,
            creator_date: 0,
        });
    }
    finish_query(collected.into_values().collect(), query, options, odb, None)
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
    let mut collected = BTreeMap::new();
    for ns in &namespaces {
        let prefix = ns.prefix();
        store.for_each_ref(prefix, &mut |entry: &RefEntry| {
            insert_collected_ref(&mut collected, entry, store, &namespaces, query, options);
            ControlFlow::Continue(())
        })?;
    }
    finish_query(collected.into_values().collect(), query, options, odb, None)
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
    let mut collected = BTreeMap::new();
    for ns in &namespaces {
        let prefix = ns.prefix();
        store.for_each_ref(prefix, &mut |entry: &RefEntry| {
            if crate::worktree_ref::is_linked_worktree_git_dir(git_dir)
                && !crate::worktree_ref::ref_visible_from_worktree(git_dir, &entry.name)
            {
                return ControlFlow::Continue(());
            }
            insert_collected_ref(&mut collected, entry, store, &namespaces, query, options);
            ControlFlow::Continue(())
        })?;
    }
    finish_query(
        collected.into_values().collect(),
        query,
        options,
        Some(repo.odb()),
        Some(repo),
    )
}

fn insert_collected_ref(
    collected: &mut BTreeMap<String, CollectedRef>,
    entry: &RefEntry,
    store: &dyn RefStore,
    namespaces: &[RefNamespace],
    query: &RefQuery,
    options: &RefQueryOptions,
) {
    let resolved = entry
        .peeled
        .or_else(|| match &entry.value {
            crate::refs::store::RawRef::Direct(oid) => Some(*oid),
            crate::refs::store::RawRef::Symbolic(target) => store.resolve(target).ok(),
        })
        .or_else(|| store.resolve(&entry.name).ok());
    let Some(resolved) = resolved else {
        return;
    };
    let short_name = short_name_for_query(&entry.name, namespaces);
    if !patterns_match(&short_name, &query.patterns, options.ignore_case) {
        return;
    }
    let direct_oid = match &entry.value {
        crate::refs::store::RawRef::Direct(oid) => *oid,
        crate::refs::store::RawRef::Symbolic(_) => resolved,
    };
    let kind = ref_kind_from_name(&entry.name);
    collected.entry(entry.name.clone()).or_insert(CollectedRef {
        full_name: entry.name.clone(),
        short_name,
        oid: direct_oid,
        peeled: entry.peeled,
        kind,
        creator_date: 0,
    });
}

fn effective_namespaces(query: &RefQuery) -> Vec<RefNamespace> {
    if query.namespaces.is_empty() {
        vec![RefNamespace::Heads]
    } else {
        query.namespaces.clone()
    }
}

fn short_name_for_query(full_name: &str, namespaces: &[RefNamespace]) -> String {
    let mut best = 0usize;
    for ns in namespaces {
        let prefix = ns.prefix();
        if full_name.starts_with(prefix) && prefix.len() > best {
            best = prefix.len();
        }
    }
    if best > 0 {
        full_name[best..].to_owned()
    } else {
        short_name_for_kind(full_name, ref_kind_from_name(full_name))
    }
}

fn short_name_for_kind(full_name: &str, kind: RefKind) -> String {
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

fn query_has_reachability(query: &RefQuery) -> bool {
    !query.contains.is_empty()
        || !query.no_contains.is_empty()
        || !query.merged.is_empty()
        || !query.no_merged.is_empty()
        || !query.points_at.is_empty()
}

fn finish_query(
    mut collected: Vec<CollectedRef>,
    query: &RefQuery,
    options: &RefQueryOptions,
    odb: Option<&Odb>,
    repo: Option<&Repository>,
) -> QueryResult<Vec<RefListing>> {
    if let Some(odb) = odb {
        for entry in &mut collected {
            fill_peeled_from_odb(odb, entry);
        }
    }
    if query_has_reachability(query) {
        let repo = repo.ok_or(RefQueryError::MissingObjectDatabase)?;
        let mut reach_rows: Vec<(String, ObjectId, Option<ObjectId>)> = collected
            .iter()
            .map(|c| (c.full_name.clone(), c.oid, c.peeled))
            .collect();
        query_reachability::filter_listings_by_reachability(repo, query, &mut reach_rows)?;
        let keep: std::collections::HashSet<String> =
            reach_rows.into_iter().map(|(name, _, _)| name).collect();
        collected.retain(|c| keep.contains(&c.full_name));
    }
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

fn fill_peeled_from_odb(odb: &Odb, entry: &mut CollectedRef) {
    if entry.peeled.is_some() {
        return;
    }
    let Ok(info) = odb.read_info(&entry.oid) else {
        return;
    };
    if info.kind != ObjectKind::Tag {
        return;
    }
    if let Ok((peeled, _)) = odb.peel_tags(entry.oid) {
        entry.peeled = Some(peeled);
    }
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

/// Build [`RefQueryOptions`] from repository config (`versionsort.suffix`, `core.ignorecase`).
///
/// # Errors
///
/// Propagates config load failures from [`Repository::config`](crate::repo::Repository::config).
pub fn ref_query_options_for_repo(repo: &Repository) -> crate::error::Result<RefQueryOptions> {
    let config = repo.config()?;
    let versionsort_suffixes = config.get_all("versionsort.suffix");
    let ignore_case = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    Ok(RefQueryOptions {
        versionsort_suffixes,
        ignore_case,
    })
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

    #[test]
    fn utf8_names_with_suffix_config_do_not_panic_in_compare() {
        let suffixes = vec!["x".to_owned()];
        let _ = compare_version_names("refs/tags/vα1", "refs/tags/vα2", &suffixes);
    }

    #[test]
    fn ignore_case_prefix_directory_match() {
        assert!(pattern_matches_short_name("Feature/x", "feature/", true));
    }

    #[test]
    fn ignore_case_prefix_does_not_panic_on_utf8_short_names() {
        assert!(!pattern_matches_short_name("€x", "a", true));
    }
}
