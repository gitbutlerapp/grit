//! Commit traversal and output planning for `rev-list`.
//!
//! This module implements a focused `rev-list` subset used by the v2 test
//! wave: revision ranges, `--all`, `--stdin` argument ingestion, commit walk
//! limits, ordering (`--topo-order`, `--date-order`, `--reverse`), and basic
//! output shaping (`--count`, `--parents`, `--format`).

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::bitmap_walk::{bitmap_filter_supported, BitmapWalkError, PeeledTips};
use crate::commit_graph_file::{BloomPrecheck, BloomWalkStatsHandle, CommitGraphChain};
use crate::config::ConfigSet;
use crate::diff::zero_oid;
use crate::error::{Error, Result};
use crate::ident::{committer_unix_seconds_for_ordering, parse_signature_times};
use crate::ignore::{parse_sparse_patterns_from_blob, path_in_sparse_checkout};
use crate::index::{CacheTreeNode, Index, MODE_GITLINK, MODE_TREE};
use crate::merge_base::commits_reachable_excluding_ancestors_of;
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::odb::store::ObjectStore;
use crate::pack_bitmap::BitmapIndex;
use crate::patch_ids::{compute_patch_id, compute_patch_id_for_paths};
use crate::ref_exclusions::{git_namespace_prefix, strip_git_namespace, RefExclusions};
use crate::reflog::{list_reflog_refs, read_reflog};
use crate::refs;
use crate::repo::Repository;
use crate::rev_list_error::RevListError;
use crate::rev_parse::{resolve_revision_for_range_end, resolve_treeish_path, split_treeish_spec};

/// User-facing output mode for `rev-list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputMode {
    /// Print only object IDs.
    OidOnly,
    /// Print object ID followed by all parent IDs.
    Parents,
    /// Print a custom `%` placeholder format.
    Format(String),
}

/// Behavior when reachable objects are missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingAction {
    /// Fail traversal when a referenced object is missing.
    Error,
    /// Continue traversal and report each missing object.
    Print,
    /// Continue traversal and report missing objects with inferred metadata.
    PrintInfo,
    /// Continue traversal and silently ignore missing objects.
    Allow,
}

impl MissingAction {
    fn reports_missing(self) -> bool {
        matches!(self, Self::Print | Self::PrintInfo)
    }
}

type ResolvedSpecsWithTagMap = (Vec<ObjectId>, Vec<RootObject>, HashMap<ObjectId, ObjectId>);
type StdinRevisionSpecs = (Vec<String>, Vec<String>, bool, Vec<String>);
type ReachableObjectsSegmented = (
    Vec<(ObjectId, String)>,
    Vec<ObjectId>,
    Vec<ObjectId>,
    Vec<Vec<(ObjectId, String)>>,
);
type ReachableObjectsInOrder = (
    Vec<(ObjectId, String)>,
    Vec<ObjectId>,
    Vec<ObjectId>,
    Vec<usize>,
);

/// Kind selector for `object:type=<kind>` filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

/// Object filter specification for `--filter=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectFilter {
    /// `blob:none` — omit all blobs.
    BlobNone,
    /// `blob:limit=<n>` — omit blobs larger than `n` bytes.
    BlobLimit(u64),
    /// `tree:<depth>` — omit trees deeper than `depth`.
    TreeDepth(u64),
    /// `sparse:oid=<rev>:<path>` or raw hex — sparse-checkout style path filter from a blob.
    SparseOid(String),
    /// `object:type=(blob|tree|commit|tag)` — keep only objects of that type.
    ObjectType(FilterObjectKind),
    /// `combine:<filter>+<filter>+…` — apply multiple filters.
    Combine(Vec<ObjectFilter>),
}

impl ObjectFilter {
    /// Parse a `--filter=<spec>` value.
    pub fn parse(spec: &str) -> std::result::Result<Self, String> {
        Self::parse_inner(spec.trim(), false)
    }

    fn parse_inner(spec: &str, from_combine_subfilter: bool) -> std::result::Result<Self, String> {
        if spec == "blob:none" {
            return Ok(ObjectFilter::BlobNone);
        }
        if let Some(rest) = spec.strip_prefix("blob:limit=") {
            let bytes = parse_size_suffix(rest)
                .ok_or_else(|| format!("invalid blob:limit value: {rest}"))?;
            return Ok(ObjectFilter::BlobLimit(bytes));
        }
        if let Some(rest) = spec.strip_prefix("tree:") {
            if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
                return Err(if from_combine_subfilter {
                    "expected 'tree:<depth>'.".to_owned()
                } else {
                    format!("invalid tree depth: {rest}")
                });
            }
            let depth: u64 = rest.parse().map_err(|_| {
                if from_combine_subfilter {
                    "expected 'tree:<depth>'.".to_owned()
                } else {
                    format!("invalid tree depth: {rest}")
                }
            })?;
            return Ok(ObjectFilter::TreeDepth(depth));
        }
        if let Some(rest) = spec.strip_prefix("object:type=") {
            let kind = match rest {
                "blob" => FilterObjectKind::Blob,
                "tree" => FilterObjectKind::Tree,
                "commit" => FilterObjectKind::Commit,
                "tag" => FilterObjectKind::Tag,
                "" => return Err("invalid object type".to_owned()),
                _ => return Err(format!("invalid object type: {rest}")),
            };
            return Ok(ObjectFilter::ObjectType(kind));
        }
        if let Some(rest) = spec.strip_prefix("sparse:oid=") {
            if rest.is_empty() {
                return Err("invalid sparse:oid value: ".to_owned());
            }
            return Ok(ObjectFilter::SparseOid(rest.to_owned()));
        }
        if let Some(rest) = spec.strip_prefix("combine:") {
            if rest.is_empty() {
                return Err("expected something after combine:".to_owned());
            }
            let parts = split_combine_raw_parts(rest);
            if parts.is_empty() {
                return Err("expected something after combine:".to_owned());
            }
            let mut filters = Vec::new();
            for part in parts {
                filters.push(Self::parse_from_combine_subfilter(part)?);
            }
            return Ok(ObjectFilter::Combine(filters));
        }
        Err(format!("invalid filter-spec '{spec}'"))
    }

    fn parse_from_combine_subfilter(encoded: &str) -> std::result::Result<Self, String> {
        if let Some(ch) = combine_subfilter_has_reserved(encoded) {
            return Err(format!("must escape char in sub-filter-spec: '{ch}'"));
        }
        let decoded = url_decode(encoded);
        Self::parse_inner(&decoded, true)
    }

    /// Merge another `--filter` argument (Git joins multiple filters with AND).
    #[must_use]
    pub fn merge_with(self, other: Self) -> Self {
        match (self, other) {
            (ObjectFilter::Combine(mut a), ObjectFilter::Combine(mut b)) => {
                a.append(&mut b);
                ObjectFilter::Combine(a)
            }
            (ObjectFilter::Combine(mut a), b) => {
                a.push(b);
                ObjectFilter::Combine(a)
            }
            (a, ObjectFilter::Combine(mut b)) => {
                let mut v = vec![a];
                v.append(&mut b);
                ObjectFilter::Combine(v)
            }
            (a, b) => ObjectFilter::Combine(vec![a, b]),
        }
    }

    /// Check if a blob should be included given its size.
    pub fn includes_blob(&self, size: u64) -> bool {
        match self {
            ObjectFilter::BlobNone => false,
            ObjectFilter::BlobLimit(limit) => size < *limit,
            // Depth is applied via [`ObjectFilter::includes_blob_under_tree`]; this stays permissive
            // for callers that only have a size (e.g. loose-object scans).
            ObjectFilter::TreeDepth(_) => true,
            ObjectFilter::SparseOid(_) => true,
            ObjectFilter::ObjectType(kind) => *kind == FilterObjectKind::Blob,
            ObjectFilter::Combine(filters) => filters.iter().all(|f| f.includes_blob(size)),
        }
    }

    /// Whether a blob that lives directly under a tree at `parent_tree_depth` passes this filter.
    ///
    /// For `tree:<n>` filters, Git assigns blobs the traversal depth after entering the parent tree,
    /// which matches `parent_tree_depth + 1` in our walk where the commit root tree is depth `0`.
    #[must_use]
    pub fn includes_blob_under_tree(&self, size: u64, parent_tree_depth: u64) -> bool {
        match self {
            ObjectFilter::BlobNone => false,
            ObjectFilter::BlobLimit(limit) => size < *limit,
            ObjectFilter::TreeDepth(max_depth) => parent_tree_depth.saturating_add(1) < *max_depth,
            ObjectFilter::SparseOid(_) => true,
            ObjectFilter::ObjectType(kind) => *kind == FilterObjectKind::Blob,
            ObjectFilter::Combine(filters) => filters
                .iter()
                .all(|f| f.includes_blob_under_tree(size, parent_tree_depth)),
        }
    }

    /// Check if a tree at given depth should be included.
    pub fn includes_tree(&self, depth: u64) -> bool {
        match self {
            ObjectFilter::BlobNone => true,
            ObjectFilter::BlobLimit(_) => true,
            ObjectFilter::TreeDepth(max_depth) => depth < *max_depth,
            ObjectFilter::SparseOid(_) => true,
            ObjectFilter::ObjectType(kind) => *kind == FilterObjectKind::Tree,
            ObjectFilter::Combine(filters) => filters.iter().all(|f| f.includes_tree(depth)),
        }
    }

    /// Whether a commit or tag object should appear in a flat object scan (e.g. `cat-file --batch-all-objects`).
    pub fn includes_commit_or_tag_object(&self, kind: ObjectKind) -> bool {
        let expected = match kind {
            ObjectKind::Commit => Some(FilterObjectKind::Commit),
            ObjectKind::Tag => Some(FilterObjectKind::Tag),
            _ => None,
        };
        match self {
            ObjectFilter::BlobNone | ObjectFilter::BlobLimit(_) => true,
            ObjectFilter::TreeDepth(_) => true,
            ObjectFilter::SparseOid(_) => true,
            ObjectFilter::ObjectType(t) => expected == Some(*t),
            ObjectFilter::Combine(filters) => filters
                .iter()
                .all(|f| f.includes_commit_or_tag_object(kind)),
        }
    }

    /// True if `kind` / `size` pass this filter when enumerating a single object (no tree path).
    pub fn includes_loose_object(&self, kind: ObjectKind, size: u64) -> bool {
        match kind {
            ObjectKind::Blob => self.includes_blob(size),
            ObjectKind::Tree => self.includes_tree(0),
            ObjectKind::Commit | ObjectKind::Tag => self.includes_commit_or_tag_object(kind),
        }
    }

    /// Whether an object passes this filter for direct OID lookup (`git cat-file --filter`).
    #[must_use]
    pub fn passes_for_object(&self, kind: ObjectKind, size: usize) -> bool {
        self.includes_loose_object(kind, size as u64)
    }
}

/// Reachable object IDs enumerated the same way as `git rev-list --objects --no-object-names --all`,
/// optionally with `--filter` and `--filter-provided-objects` (used by `git cat-file --batch-all-objects`).
pub fn reachable_object_ids_for_cat_file(
    repo: &Repository,
    filter: Option<&ObjectFilter>,
    filter_provided_objects: bool,
) -> Result<Vec<ObjectId>> {
    let opts = RevListOptions {
        all_refs: true,
        objects: true,
        no_object_names: true,
        quiet: true,
        filter: filter.cloned(),
        filter_provided_objects,
        ..Default::default()
    };
    let result = rev_list(repo, &[], &[], &opts)?;
    let mut set = BTreeSet::new();
    for (i, oid) in result.commits.iter().enumerate() {
        if result.objects_print_commit.get(i).copied().unwrap_or(true) {
            set.insert(*oid);
        }
    }
    for (oid, _) in &result.objects {
        set.insert(*oid);
    }
    Ok(set.into_iter().collect())
}

/// Objects matching `filter`, for `cat-file --batch-all-objects --filter` (same set as
/// `rev-list --objects --all --filter --filter-provided-objects`).
pub fn object_ids_for_cat_file_filtered(
    repo: &Repository,
    filter: &ObjectFilter,
) -> Result<Vec<ObjectId>> {
    reachable_object_ids_for_cat_file(repo, Some(filter), true)
}

/// Parse a size with optional k/m/g suffix.
fn parse_size_suffix(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, multiplier) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1024u64),
        b'm' | b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1u64),
    };
    let num: u64 = num_str.parse().ok()?;
    Some(num * multiplier)
}

/// Raw substrings between `+` in a `combine:` value (still percent-encoded).
fn split_combine_raw_parts(spec: &str) -> Vec<&str> {
    spec.split('+').filter(|s| !s.is_empty()).collect()
}

/// Characters that must not appear raw inside a `combine:` sub-filter (matches Git
/// `RESERVED_NON_WS` + whitespace; `%` is allowed because subspecs are percent-encoded).
fn combine_subfilter_has_reserved(encoded: &str) -> Option<char> {
    const RESERVED: &str = "~`!@#$^&*()[]{}\\;'\",<>?";
    for ch in encoded.chars() {
        if ch.is_control() || ch.is_whitespace() {
            return Some(ch);
        }
        if RESERVED.contains(ch) {
            return Some(ch);
        }
    }
    None
}

/// Expand a user filter for protocol lines (`blob:limit=1k` → `blob:limit=1024`).
pub fn expand_object_filter_for_protocol(spec: &str) -> std::result::Result<String, String> {
    let f = ObjectFilter::parse(spec)?;
    match f {
        ObjectFilter::BlobLimit(n) => Ok(format!("blob:limit={n}")),
        _ => Ok(spec.to_owned()),
    }
}

fn combine_filter_allow_unencoded(ch: char) -> bool {
    if ch.is_control() || ch.is_whitespace() || ch == '%' || ch == '+' {
        return false;
    }
    !"~`!@#$^&*()[]{}\\;'\",<>?".contains(ch)
}

/// Append URL-encoded `raw` for Git `filter_spec` / trace (matches `allow_unencoded` in Git).
pub fn url_encode_object_filter_subspec(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.as_bytes() {
        let ch = *b as char;
        if combine_filter_allow_unencoded(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02x}", b));
        }
    }
    out
}

/// Emit `Add to combine filter-spec: …` when `GIT_TRACE` is enabled (Git `list-objects-filter-options.c`).
pub fn trace_combine_filter_append(encoded_segment: &str) {
    let Some(trace_val) = crate::environment::Environment::empty().var("GIT_TRACE") else {
        return;
    };
    if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
        return;
    }
    let line = format!("Add to combine filter-spec: {encoded_segment}\n");
    match trace_val.as_str() {
        "1" | "true" | "2" => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        path => {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

/// Simple URL percent-decoding.
fn url_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hi = chars.next().unwrap_or('0');
            let lo = chars.next().unwrap_or('0');
            let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16).unwrap_or(b'?');
            result.push(byte as char);
        } else {
            result.push(ch);
        }
    }
    result
}

/// Ordering mode for commit output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderingMode {
    /// Reverse-chronological by committer date (default `rev-list` / `log` when no ordering flags).
    Default,
    /// Commit-date heap walk (`--date-order` without `--topo-order`; matches Git for `t6012`).
    DateOrderWalk,
    /// Author-date heap walk (`--author-date-order` without `--topo-order`).
    AuthorDateWalk,
    /// Topological walk with committer-date tie-breaks (`--topo-order`, `--simplify-merges`).
    Topo,
    /// Topological walk with author-date tie-breaks (`--topo-order --author-date-order`).
    AuthorDateTopo,
}

/// Parsed and normalized options for rev-list traversal.
#[derive(Debug, Clone)]
pub struct RevListOptions {
    /// Include all refs (`--all`) as positive tips.
    pub all_refs: bool,
    /// Follow only first parent when walking merges.
    pub first_parent: bool,
    /// Enable ancestry-path filtering.
    pub ancestry_path: bool,
    /// Optional explicit ancestry-path pivot commits.
    pub ancestry_path_bottoms: Vec<ObjectId>,
    /// Keep only decorated commits after traversal.
    pub simplify_by_decoration: bool,
    /// The set of decorated ("interesting") commit OIDs for `--simplify-by-decoration`.
    /// When non-empty, proper history simplification (parent rewriting) is applied: a commit is
    /// kept iff it is decorated, a relevant merge in the rewritten graph, or a root. When empty,
    /// the legacy crude retain (decorated tips only) is used as a fallback. The caller (CLI)
    /// computes this set honoring the display decoration filter (`--decorate-refs[-exclude]`,
    /// `log.excludeDecoration`, and the default hidden-ref behavior), matching Git's
    /// `load_ref_decorations(NULL, ...)` set used by `rev_compare_tree`.
    pub simplify_by_decoration_oids: HashSet<ObjectId>,
    /// Commit output mode.
    pub output_mode: OutputMode,
    /// Suppress commit output.
    pub quiet: bool,
    /// Print only final count.
    pub count: bool,
    /// Skip N commits from selected list.
    pub skip: usize,
    /// Optional maximum selected commits.
    pub max_count: Option<usize>,
    /// Ordering strategy.
    pub ordering: OrderingMode,
    /// Reverse selected output order.
    pub reverse: bool,
    /// Keep only commits not reachable from another selected commit (`--maximal-only`).
    pub maximal_only: bool,
    /// List reachable objects (trees, blobs) in addition to commits.
    pub objects: bool,
    /// Suppress object path names in --objects output.
    pub no_object_names: bool,
    /// Show boundary commits with `-` prefix.
    pub boundary: bool,
    /// Show left/right markers for symmetric diff.
    pub left_right: bool,
    /// True when `--left-right` was explicitly requested by the caller.
    pub left_right_explicit: bool,
    /// Filter to left-only commits in symmetric diff.
    pub left_only: bool,
    /// Filter to right-only commits in symmetric diff.
    pub right_only: bool,
    /// Cherry-mark equivalent commits with `=` instead of `+`.
    pub cherry_mark: bool,
    /// Cherry-pick: omit equivalent commits from output.
    pub cherry_pick: bool,
    /// Minimum number of parents a commit must have to be included.
    pub min_parents: Option<usize>,
    /// Maximum number of parents a commit may have to be included.
    pub max_parents: Option<usize>,
    /// Symmetric-diff left OID (set by caller when A...B is used).
    pub symmetric_left: Option<ObjectId>,
    /// Symmetric-diff right OID (set by caller when A...B is used).
    pub symmetric_right: Option<ObjectId>,
    /// Path filters (files after `--`).
    pub paths: Vec<String>,
    /// Show full history (don't simplify) for path-limited walks.
    pub full_history: bool,
    /// Parent rewriting was requested (`--parents`, `--children`, or `%P`/`%p` output).
    pub parent_rewrite: bool,
    /// Sparse mode: don't prune non-matching commits.
    pub sparse: bool,
    /// Further simplify history after path limiting (`--simplify-merges`).
    pub simplify_merges: bool,
    /// Preserve path-limited simplify-merge nodes long enough for `log --graph` lane rendering.
    pub preserve_simplify_merges_graph_merges: bool,
    /// Include "diverted" merge commits on the first-parent spine (`--show-pulls`).
    pub show_pulls: bool,
    /// When walking excluded commits, only follow the first parent (`--exclude-first-parent-only`).
    pub exclude_first_parent_only: bool,
    /// Object filter for `--filter=<spec>`.
    pub filter: Option<ObjectFilter>,
    /// Raw `--filter=` argument strings in order (for `GIT_TRACE` when Git combines multiple filters).
    pub filter_raw_specs: Vec<String>,
    /// When set with `--filter`, explicitly given revision objects are filtered too.
    pub filter_provided_objects: bool,
    /// Print omitted objects prefixed with `~`.
    pub filter_print_omitted: bool,
    /// Emit objects interleaved with their introducing commit.
    pub in_commit_order: bool,
    /// Exclude objects in `.keep` pack files.
    pub no_kept_objects: bool,
    /// Behavior when referenced objects are missing.
    pub missing_action: MissingAction,
    /// Stop traversal at objects promised by promisor packs.
    pub exclude_promisor_objects: bool,
    /// Ignore missing command-line revision/object arguments.
    pub ignore_missing: bool,
    /// When set with `--objects`, omit path names from non-commit object lines (bitmap-style output).
    pub use_bitmap_index: bool,
    /// When set with `--objects`, list only objects not present in any pack file.
    pub unpacked_only: bool,
    /// With `--use-bitmap-index`, emit OID-only object lines (no paths / trailing space) for filters
    /// that match Git's bitmap object formatting.
    pub bitmap_oid_only_objects: bool,
    /// Reorder path-limited results for graph-friendly parent ordering (Git `log` / `rev-list`).
    /// Internal dense passes for `--sparse` set this to `false` to avoid recursion.
    pub path_graph_reorder: bool,
    /// Exclude commits with committer date strictly after this Unix timestamp (`--until` / `--before`).
    pub until_cutoff: Option<i64>,
    /// Exclude commits with committer date strictly before this Unix timestamp (`--since` / `--after`).
    pub since_cutoff: Option<i64>,
    /// Include OIDs from all reflogs as extra commit tips (`git pack-objects --reflog`).
    pub include_reflog_entries: bool,
    /// Include index blobs and valid cache-tree nodes as object roots (`--indexed-objects`).
    pub include_indexed_objects: bool,
    /// Exclude index blobs and valid cache-tree nodes as object roots (`--not --indexed-objects`).
    pub exclude_indexed_objects: bool,
    /// When true, read parent links and committer dates from the on-disk commit-graph
    /// when a commit is present in the file (Git commit-graph walk acceleration).
    pub use_commit_graph: bool,
    /// When true with pathspecs, consult commit-graph Bloom filters (matches `core.commitGraph`).
    pub use_commit_graph_bloom: bool,
    /// `commitGraph.readChangedPaths` (default true).
    pub commit_graph_read_changed_paths: bool,
    /// `commitGraph.changedPathsVersion` (-1 = autodetect from graph).
    pub commit_graph_changed_paths_version: i32,
    /// Optional trace counters for `GIT_TRACE2_PERF` Bloom statistics.
    pub bloom_stats: Option<BloomWalkStatsHandle>,
}

impl Default for RevListOptions {
    fn default() -> Self {
        Self {
            all_refs: false,
            first_parent: false,
            ancestry_path: false,
            ancestry_path_bottoms: Vec::new(),
            simplify_by_decoration: false,
            simplify_by_decoration_oids: HashSet::new(),
            output_mode: OutputMode::OidOnly,
            quiet: false,
            count: false,
            skip: 0,
            max_count: None,
            ordering: OrderingMode::Default,
            reverse: false,
            maximal_only: false,
            objects: false,
            no_object_names: false,
            boundary: false,
            left_right: false,
            left_right_explicit: false,
            left_only: false,
            right_only: false,
            cherry_mark: false,
            cherry_pick: false,
            min_parents: None,
            max_parents: None,
            symmetric_left: None,
            symmetric_right: None,
            paths: Vec::new(),
            full_history: false,
            parent_rewrite: false,
            sparse: false,
            simplify_merges: false,
            preserve_simplify_merges_graph_merges: false,
            show_pulls: false,
            exclude_first_parent_only: false,
            filter: None,
            filter_raw_specs: Vec::new(),
            filter_provided_objects: false,
            filter_print_omitted: false,
            in_commit_order: false,
            no_kept_objects: false,
            missing_action: MissingAction::Error,
            exclude_promisor_objects: false,
            ignore_missing: false,
            use_bitmap_index: false,
            unpacked_only: false,
            bitmap_oid_only_objects: false,
            path_graph_reorder: true,
            until_cutoff: None,
            since_cutoff: None,
            include_reflog_entries: false,
            include_indexed_objects: false,
            exclude_indexed_objects: false,
            use_commit_graph: false,
            use_commit_graph_bloom: false,
            commit_graph_read_changed_paths: true,
            commit_graph_changed_paths_version: -1,
            bloom_stats: None,
        }
    }
}

/// Final commit selection result.
#[derive(Debug, Clone)]
pub struct RevListResult {
    /// Selected commits in final output order, after skip/max/reverse.
    pub commits: Vec<ObjectId>,
    /// Reachable non-commit objects when `--objects` is active.
    /// Each entry is `(oid, optional_path)`.
    pub objects: Vec<(ObjectId, String)>,
    /// Objects omitted by `--filter` (for `--filter-print-omitted`).
    pub omitted_objects: Vec<ObjectId>,
    /// Referenced objects missing from the object database.
    pub missing_objects: Vec<ObjectId>,
    /// Boundary commits (excluded parents shown with `-` prefix).
    pub boundary_commits: Vec<ObjectId>,
    /// For `--left-right`: mapping commit OID -> true=left, false=right.
    pub left_right_map: HashMap<ObjectId, bool>,
    /// For `--cherry-mark`: set of commits that are equivalent (patch-id match).
    pub cherry_equivalent: HashSet<ObjectId>,
    /// Per-commit object counts (parallel to `commits`) for `--in-commit-order`.
    /// When non-empty, `objects[sum(counts[..i])..sum(counts[..=i])]` are the objects
    /// introduced by `commits[i]`.
    pub per_commit_object_counts: Vec<usize>,
    /// Commit OIDs given as positive revision tips (for Git `USER_GIVEN` / filter edge cases).
    pub object_walk_tips: Vec<ObjectId>,
    /// When `--objects` is active, whether to print the commit line before that commit's objects.
    /// Aligns with Git marking user-given tips vs `NOT_USER_GIVEN` commits in list-objects.
    pub objects_print_commit: Vec<bool>,
    /// When `--objects` is active and not `--in-commit-order`, objects grouped per commit walk plus
    /// a final segment for explicit `object_roots` (length `commits.len() + 1`).
    pub object_segments: Vec<Vec<(ObjectId, String)>>,
    /// True when `--use-bitmap-index --objects` should format trees/blobs as bare OIDs (no paths).
    pub bitmap_object_format: bool,
    /// When set, `--count` used a fast path that did not materialize [`Self::commits`].
    pub reachable_commit_count: Option<usize>,
    /// Non-commit objects counted for `--count --objects` without listing [`Self::objects`].
    pub reachable_non_commit_object_count: Option<usize>,
    /// When a positive spec named a ref to an annotated tag of a commit, maps peeled commit → tag OID.
    pub tip_annotated_tag_by_commit: HashMap<ObjectId, ObjectId>,
}

impl RevListResult {
    /// Commit count for `--count` (fast path or [`Self::commits`] length).
    #[must_use]
    pub fn commit_count(&self) -> usize {
        self.reachable_commit_count.unwrap_or(self.commits.len())
    }

    /// Total printed lines for `--count --objects`.
    #[must_use]
    pub fn count_with_objects_total(&self) -> usize {
        if let (Some(c), Some(o)) = (
            self.reachable_commit_count,
            self.reachable_non_commit_object_count,
        ) {
            return c + o;
        }
        self.commits.len() + self.objects.len()
    }
}

/// Per-commit bisection score for `rev-list --bisect*` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectEntry {
    /// Candidate commit object ID.
    pub oid: ObjectId,
    /// Number of candidate commits reachable from `oid`, including `oid`.
    pub reaches: usize,
    /// Git's bisection distance, `min(reaches, all - reaches)`.
    pub distance: usize,
}

/// Bisection analysis result for a selected revision set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectSelection {
    /// Best midpoint candidate, if the selected set is non-empty.
    pub best: Option<BisectEntry>,
    /// All candidates sorted as `--bisect-all` requires.
    pub all: Vec<BisectEntry>,
    /// Total number of commits in the bisection set.
    pub total: usize,
}

/// Rank rev-list commits for Git-compatible bisection helpers.
///
/// # Parameters
///
/// - `repo` - repository used for parent lookup.
/// - `commits` - selected commit set from a normal `rev-list` traversal.
/// - `first_parent` - when true, follow only first parents while counting reachability.
///
/// # Returns
///
/// A [`BisectSelection`] containing the best midpoint plus the complete sorted candidate list.
///
/// # Errors
///
/// Returns repository/object errors when parent commits cannot be loaded.
pub fn select_bisect_commits(
    repo: &Repository,
    commits: &[ObjectId],
    first_parent: bool,
) -> Result<BisectSelection> {
    let total = commits.len();
    if total == 0 {
        return Ok(BisectSelection {
            best: None,
            all: Vec::new(),
            total: 0,
        });
    }

    let candidates: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut graph = CommitGraph::new(repo, first_parent);
    let mut entries = Vec::with_capacity(commits.len());
    for &oid in commits {
        let reaches = count_reachable_candidates(&mut graph, oid, &candidates)?;
        let distance = reaches.min(total.saturating_sub(reaches));
        entries.push(BisectEntry {
            oid,
            reaches,
            distance,
        });
    }

    let mut sorted = entries.clone();
    sorted.sort_by(|a, b| b.distance.cmp(&a.distance).then_with(|| a.oid.cmp(&b.oid)));

    let mut best = None;
    for entry in &entries {
        if best
            .as_ref()
            .is_none_or(|current: &BisectEntry| entry.distance > current.distance)
        {
            best = Some(entry.clone());
        }
    }
    Ok(BisectSelection {
        best,
        all: sorted,
        total,
    })
}

fn count_reachable_candidates(
    graph: &mut CommitGraph<'_>,
    oid: ObjectId,
    candidates: &HashSet<ObjectId>,
) -> Result<usize> {
    let mut stack = vec![oid];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        for parent in graph.parents_of(current)? {
            if candidates.contains(&parent) {
                stack.push(parent);
            }
        }
    }
    Ok(seen.len())
}

fn retain_maximal_commits(
    graph: &mut CommitGraph<'_>,
    candidates: &mut HashSet<ObjectId>,
) -> Result<()> {
    if candidates.len() < 2 {
        return Ok(());
    }

    let candidate_list: Vec<ObjectId> = candidates.iter().copied().collect();
    let candidate_set: HashSet<ObjectId> = candidate_list.iter().copied().collect();
    let mut reachable_from_another = HashSet::new();

    for tip in candidate_list {
        for reachable in walk_closure(graph, &[tip])? {
            if reachable != tip && candidate_set.contains(&reachable) {
                reachable_from_another.insert(reachable);
            }
        }
    }

    candidates.retain(|oid| !reachable_from_another.contains(oid));
    Ok(())
}

/// Resolve and walk revisions for the requested options.
///
/// # Parameters
///
/// - `repo` - repository used for ref/object lookup.
/// - `positive_specs` - positive revision tokens (e.g. `HEAD`, `A..B` rhs).
/// - `negative_specs` - negative revision tokens (`^A`, `A..B` lhs).
/// - `options` - traversal and output selection options.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] / [`Error::InvalidRef`] for bad revision
/// specs and [`Error::CorruptObject`] for non-commit or malformed commit data.
pub fn rev_list(
    repo: &Repository,
    positive_specs: &[String],
    negative_specs: &[String],
    options: &RevListOptions,
) -> Result<RevListResult> {
    let repo_config = repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(ConfigSet::new()));
    let mut graph =
        CommitGraph::with_commit_graph(repo, options.first_parent, options.use_commit_graph);

    let (mut include, mut object_roots, tip_annotated_tag_by_commit) = if options.objects {
        resolve_specs_for_objects_with_options(
            repo,
            positive_specs,
            options.ignore_missing,
            options.missing_action,
        )?
    } else {
        (
            resolve_specs_with_options(repo, positive_specs, options.ignore_missing)?,
            Vec::new(),
            HashMap::new(),
        )
    };
    let (exclude, negative_object_roots) = if options.objects {
        let (commits, roots, _) = resolve_specs_for_objects_with_options(
            repo,
            negative_specs,
            options.ignore_missing,
            options.missing_action,
        )?;
        (commits, roots)
    } else {
        (
            resolve_specs_with_options(repo, negative_specs, options.ignore_missing)?,
            Vec::new(),
        )
    };

    if options.all_refs {
        if options.objects {
            let (tips, roots) =
                all_ref_commit_tips_and_object_roots(repo, &RefExclusions::default())?;
            include.extend(tips);
            object_roots.extend(roots);
        } else {
            include.extend(all_ref_tips(repo, &RefExclusions::default())?);
        }
    }

    if options.objects && options.include_reflog_entries {
        include.extend(reflog_commit_tips(repo)?);
    }

    let index_object_roots = if options.objects
        && (options.include_indexed_objects || options.exclude_indexed_objects)
    {
        indexed_object_roots(repo)?
    } else {
        Vec::new()
    };

    let object_roots = if !options.include_indexed_objects || index_object_roots.is_empty() {
        object_roots
    } else {
        let mut merged = object_roots;
        merged.extend(index_object_roots.iter().cloned());
        merged
    };
    let negative_object_roots = if options.exclude_indexed_objects && !index_object_roots.is_empty()
    {
        let mut merged = negative_object_roots;
        merged.extend(index_object_roots);
        merged
    } else {
        negative_object_roots
    };

    if include.is_empty() && object_roots.is_empty() {
        if options.ignore_missing {
            return Ok(RevListResult {
                commits: Vec::new(),
                objects: Vec::new(),
                omitted_objects: Vec::new(),
                missing_objects: Vec::new(),
                boundary_commits: Vec::new(),
                left_right_map: HashMap::new(),
                cherry_equivalent: HashSet::new(),
                per_commit_object_counts: Vec::new(),
                object_walk_tips: Vec::new(),
                objects_print_commit: Vec::new(),
                object_segments: Vec::new(),
                bitmap_object_format: false,
                reachable_commit_count: None,
                reachable_non_commit_object_count: None,
                tip_annotated_tag_by_commit: HashMap::new(),
            });
        }
        return Err(Error::InvalidRef("no revisions specified".to_owned()));
    }

    if let Some(result) = try_rev_list_accelerated(
        repo,
        &include,
        &exclude,
        &object_roots,
        &negative_object_roots,
        options,
    )? {
        return Ok(result);
    }

    let object_walk_tip_commits: Vec<ObjectId> = if options.objects {
        include.clone()
    } else {
        Vec::new()
    };

    let excluded_promisor = if options.exclude_promisor_objects {
        crate::promisor::promisor_expanded_object_ids(repo)?
    } else {
        HashSet::new()
    };

    if let Some(ordered) = try_bounded_limit_list(repo, &include, &exclude, options)? {
        return Ok(rev_list_result_from_commits(ordered));
    }

    let mut traversal_missing = Vec::new();
    let mut traversal_missing_seen = HashSet::new();
    let (mut included, discovery_order) = if include.is_empty() {
        (HashSet::new(), Vec::new())
    } else if options.exclude_promisor_objects {
        walk_closure_ordered_excluding(&mut graph, &include, &excluded_promisor)?
    } else if options.missing_action != MissingAction::Error {
        walk_closure_ordered_with_missing(
            &mut graph,
            &include,
            &HashSet::new(),
            options.missing_action,
            &mut traversal_missing,
            &mut traversal_missing_seen,
        )?
    } else {
        walk_closure_ordered(&mut graph, &include)?
    };
    let excluded = if exclude.is_empty() {
        HashSet::new()
    } else if options.exclude_promisor_objects {
        walk_closure_ordered_excluding(&mut graph, &exclude, &excluded_promisor)?.0
    } else if options.exclude_first_parent_only {
        walk_closure_first_parent_only(&mut graph, &exclude)?
    } else if options.missing_action != MissingAction::Error {
        walk_closure_ordered_with_missing(
            &mut graph,
            &exclude,
            &HashSet::new(),
            options.missing_action,
            &mut traversal_missing,
            &mut traversal_missing_seen,
        )?
        .0
    } else {
        walk_closure(&mut graph, &exclude)?
    };
    included.retain(|oid| !excluded.contains(oid));

    // Default dense path limiting is not just a post-walk filter: when a merge is TREESAME to a
    // parent for the pathspec, Git follows only that parent and never walks the other sides.
    // `--full-history`, `--ancestry-path`, and `--simplify-merges` intentionally keep the full
    // parent walk and simplify later.
    //
    // The dense closure narrows `included` to the path-matching commits, but the user tips
    // themselves may be dropped (they did not touch the path). Keep a copy of the full reachable
    // set so date-order emission can still traverse *through* those dropped tips to reach matching
    // ancestors — otherwise a `log -- <path>` whose only matching commit is an ancestor of HEAD
    // would emit nothing (Git's `limit_list` walks the full graph and only filters emission).
    // Compute the changed-path Bloom-filter configuration once up front so that both the dense
    // path-limited walk (which performs the real TREESAME comparison) and any later re-check use
    // the same chain. Git records Bloom statistics during its single `limit_list` walk, so the
    // dense walk is the natural place to consult the filters and emit the `GIT_TRACE2_PERF`
    // counters; doing it later (after the set is already reduced) would miss TREESAME-dropped
    // commits like the latest layer's empty filters.
    let (bloom_chain, bloom_read_changed, bloom_version, bloom_cwd) = if !options.paths.is_empty() {
        let cfg = repo_config.as_ref();
        let mut core_cg = match cfg.get_bool("core.commitgraph") {
            Some(Ok(b)) => b,
            _ => true,
        };
        if repo.environment().var("GIT_TEST_COMMIT_GRAPH").as_deref() == Some("0") {
            core_cg = false;
        }
        let read_paths = cfg
            .get("commitgraph.readchangedpaths")
            .and_then(|v| crate::config::parse_bool(&v).ok())
            .unwrap_or(true);
        let version = cfg
            .get("commitgraph.changedpathsversion")
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(-1);
        let use_bloom = core_cg
            && options.use_commit_graph_bloom
            && crate::pathspec::pathspecs_allow_bloom(&options.paths);
        let read_changed = read_paths && options.commit_graph_read_changed_paths;
        let chain = if use_bloom {
            CommitGraphChain::try_load_with_caches(
                &repo.git_dir.join("objects"),
                Some(repo.caches().as_ref()),
            )
            .ok()
            .flatten()
        } else {
            None
        };
        let version = if options.commit_graph_changed_paths_version != -1 {
            options.commit_graph_changed_paths_version
        } else {
            version
        };
        (chain, read_changed, version, repo.bloom_pathspec_cwd())
    } else {
        (None, false, -1, None)
    };

    let mut dense_path_limited = false;
    let dense_traversable: HashSet<ObjectId> = if !options.paths.is_empty()
        && !options.full_history
        && !options.ancestry_path
        && !options.simplify_merges
    {
        let traversable = included.clone();
        // Consult the Bloom filter over the full reachable set *before* dense limiting prunes the
        // traversal. Git loads a first-parent Bloom filter for every commit its `limit_list` walk
        // reaches — including those simplified away by TREESAME merges — which both feeds the
        // `GIT_TRACE2_PERF` statistics and surfaces the reader's corrupt-graph integrity warnings.
        // Consulting only the pruned/selected set would undercount the statistics (e.g. side-branch
        // commits hidden by a TREESAME merge) and miss warnings for commits that don't touch the
        // path. This runs whenever a Bloom chain is present, independent of statistics collection.
        if bloom_chain.is_some() {
            let bloom_ctx = DenseBloomCtx {
                chain: bloom_chain.as_ref(),
                read_changed_paths: bloom_read_changed,
                changed_paths_version: bloom_version,
                stats: options.bloom_stats.as_ref(),
                cwd: bloom_cwd.as_deref(),
            };
            consult_dense_bloom_filters(
                repo,
                &mut graph,
                &traversable,
                &options.paths,
                &bloom_ctx,
            )?;
        }
        included = walk_dense_path_limited_closure(
            repo,
            &mut graph,
            &include,
            &excluded,
            &options.paths,
            options.sparse,
            options.show_pulls,
        )?;
        dense_path_limited = true;
        traversable
    } else {
        HashSet::new()
    };

    if options.simplify_by_decoration && options.simplify_by_decoration_oids.is_empty() {
        // Legacy fallback when the caller did not supply the decorated set (e.g. plain
        // `rev-list --simplify-by-decoration` with no CLI-computed decoration filter): keep
        // commits pointed at by a ref tip. Proper parent-rewriting simplification (below,
        // after ordering) runs when `simplify_by_decoration_oids` is populated.
        let decorated = all_ref_tips(repo, &RefExclusions::default())?;
        included.retain(|oid| decorated.contains(oid));
    }

    let mut effective_ancestry_path_bottoms = Vec::new();
    if options.ancestry_path {
        let mut bottoms = options.ancestry_path_bottoms.clone();
        if bottoms.is_empty() {
            bottoms.extend(exclude.iter().copied());
        }
        if bottoms.is_empty() {
            return Err(Error::InvalidRef(
                "--ancestry-path requires a range with excluded tips".to_owned(),
            ));
        }
        limit_to_ancestry(&mut graph, &mut included, &bottoms)?;
        effective_ancestry_path_bottoms = bottoms;
    }

    // Git: `--ancestry-path` implies `--full-history` for path-limited walks.
    // `--simplify-merges` with pathspecs uses a full parent walk first, then simplifies merges.
    let path_effective_full = options.full_history
        || options.ancestry_path
        || (options.simplify_merges && !options.paths.is_empty());

    // Filter by parent count (--merges, --no-merges, --min-parents, --max-parents). The dropped
    // commits must still be *traversable* (Git only suppresses them from output, never severs the
    // graph), so remember the full reachable set before narrowing `included` to the survivors.
    let parent_count_filter_active = options.min_parents.is_some() || options.max_parents.is_some();
    let traversable = if parent_count_filter_active {
        // The walk must be able to traverse *through* commits that path limiting already dropped
        // from `included` (e.g. a tip that does not touch the pathspec): Git never severs the graph
        // for path limiting, it only suppresses emission. When the dense path-limited closure ran it
        // pruned `included`, so seed the walk from the full reachable set (`dense_traversable`) it
        // saved beforehand; otherwise the no-merges walk would start at a dropped tip and emit nothing
        // (`range-diff <range> -- <path>` whose matching commits are all behind a non-matching tip).
        if dense_path_limited {
            dense_traversable.clone()
        } else {
            included.clone()
        }
    } else {
        HashSet::new()
    };
    if parent_count_filter_active {
        let min_p = options.min_parents.unwrap_or(0);
        let max_p = options.max_parents.unwrap_or(usize::MAX);
        included.retain(|oid| {
            let count = graph.parents_of(*oid).map(|p| p.len()).unwrap_or(0);
            count >= min_p && count <= max_p
        });
    }

    if options.maximal_only {
        retain_maximal_commits(&mut graph, &mut included)?;
    }

    let mut ordered = match options.ordering {
        OrderingMode::Default | OrderingMode::DateOrderWalk | OrderingMode::AuthorDateWalk => {
            let author_dates = options.ordering == OrderingMode::AuthorDateWalk;
            if parent_count_filter_active {
                // When parent-count filters (`--max-parents=1` / `--no-merges`, …) drop a commit
                // (e.g. a merge under `--no-merges`), Git still walks *through* it: the dropped
                // commit is simply not emitted, but its parents stay reachable. Seed from every
                // user-given tip and traverse the full reachable set, skipping dropped commits in
                // the output while continuing the walk past them (`format-patch -3` across a merge,
                // `rev-list --no-merges -N`).
                date_order_walk_through_dropped(
                    &mut graph,
                    &include,
                    &traversable,
                    &included,
                    author_dates,
                )?
            } else if dense_path_limited && !include.is_empty() {
                // Path-limiting may have dropped the user tips from `included`; traverse the full
                // reachable set (`dense_traversable`) so matching ancestors are still emitted in
                // date order, seeding from the user tips for Git's insertion-order tiebreak.
                date_order_walk_through_dropped(
                    &mut graph,
                    &include,
                    &dense_traversable,
                    &included,
                    author_dates,
                )?
            } else {
                date_order_walk(&mut graph, &included, &include, author_dates)?
            }
        }
        OrderingMode::Topo => graph_order_topo_sort(&mut graph, &included, &discovery_order)?,
        OrderingMode::AuthorDateTopo => topo_sort(&mut graph, &included, true)?,
    };

    // Path filtering: keep only commits that modify given paths.
    if !options.paths.is_empty() {
        let paths = &options.paths;
        // The dense path-limited walk above already performed the first-parent Bloom precheck and
        // recorded its `GIT_TRACE2_PERF` statistics for every visited commit (matching Git's single
        // `limit_list` pass). Re-running the precheck here would double-count those counters, so
        // suppress stats in the dense case while still consulting the chain to filter the set.
        // For `--full-history`/`--simplify-merges`/`--ancestry-path`, the dense walk was skipped, so
        // this re-check is the only place the precheck runs and must record the statistics.
        let retain_stats = if dense_path_limited {
            None
        } else {
            options.bloom_stats.as_ref()
        };
        ordered.retain(|oid| {
            commit_touches_paths(
                repo,
                &mut graph,
                *oid,
                paths,
                path_effective_full,
                options.sparse,
                options.simplify_merges,
                options.preserve_simplify_merges_graph_merges,
                options.show_pulls,
                options.parent_rewrite,
                options.ancestry_path,
                &effective_ancestry_path_bottoms,
                &excluded,
                bloom_chain.as_ref(),
                bloom_read_changed,
                bloom_version,
                retain_stats,
                bloom_cwd.as_deref(),
            )
            .unwrap_or(false)
        });
    }

    if !options.paths.is_empty() && options.simplify_merges && !ordered.is_empty() {
        ordered = simplify_merges_commit_list(
            repo,
            &ordered,
            &options.paths,
            &excluded,
            &effective_ancestry_path_bottoms,
            options.ordering,
            options.show_pulls,
            options.preserve_simplify_merges_graph_merges,
        )?;
    }

    // Git-style path-limited parent reordering (dense history and `--sparse` only). Pure
    // `--full-history` walks keep rev-list order (`t6012` full-history path expectations).
    let path_needs_graph_reorder = !options.paths.is_empty()
        && options.path_graph_reorder
        && !options.simplify_merges
        && (!options.full_history || options.sparse);
    if path_needs_graph_reorder && !ordered.is_empty() {
        if options.sparse {
            let mut dense_opts = options.clone();
            dense_opts.sparse = false;
            dense_opts.path_graph_reorder = false;
            let dense_result = rev_list(repo, positive_specs, negative_specs, &dense_opts)?;
            let dense_ordered = reorder_path_limited_graph_commits(
                repo,
                &dense_result.commits,
                options.first_parent,
            )?;
            ordered = expand_sparse_path_limited_graph_history(repo, &dense_ordered)?;
        } else {
            ordered = reorder_path_limited_graph_commits(repo, &ordered, options.first_parent)?;
        }
    }

    if matches!(
        options.ordering,
        OrderingMode::Topo | OrderingMode::AuthorDateTopo
    ) && !options.left_right
        && !options.left_only
        && !options.right_only
        && !options.cherry_mark
        && !options.cherry_pick
    {
        if let (Some(left_oid), Some(right_oid)) = (options.symmetric_left, options.symmetric_right)
        {
            reorder_symmetric_topo_right_first(&mut graph, &mut ordered, left_oid, right_oid)?;
        }
    }

    // Left-right classification for symmetric diffs
    let mut left_right_map = HashMap::new();
    if options.left_right
        || options.left_only
        || options.right_only
        || options.cherry_mark
        || options.cherry_pick
    {
        if let (Some(left_oid), Some(right_oid)) = (options.symmetric_left, options.symmetric_right)
        {
            // Match Git's `SYMMETRIC_LEFT` / right-only classification (`revision.c`): a commit is
            // "left" iff it is reachable from the left tip but not from the right tip, and vice
            // versa.  Using plain set intersection incorrectly labels the shared spine as "right"
            // only, which breaks `--cherry-pick` on `A...B` (t3419-rebase-patch-id).
            let left_reach = walk_closure(&mut graph, &[left_oid])?;
            let right_reach = walk_closure(&mut graph, &[right_oid])?;
            for &oid in &ordered {
                let from_left = left_reach.contains(&oid);
                let from_right = right_reach.contains(&oid);
                left_right_map.insert(oid, from_left && !from_right);
            }
        }
    }

    // Cherry-pick / cherry-mark: match commits by Git-compatible patch-id (see `git revision.c`
    // `cherry_pick_list`, used by `git rebase` todo generation).
    let mut cherry_equivalent = HashSet::new();
    if options.cherry_pick || options.cherry_mark {
        let left_commits: Vec<_> = ordered
            .iter()
            .filter(|o| left_right_map.get(o) == Some(&true))
            .copied()
            .collect();
        let right_commits: Vec<_> = ordered
            .iter()
            .filter(|o| left_right_map.get(o) == Some(&false))
            .copied()
            .collect();
        let left_first = !left_commits.is_empty()
            && !right_commits.is_empty()
            && left_commits.len() < right_commits.len();

        let mut by_patch: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        if left_first {
            for oid in &left_commits {
                if let Ok(Some(pid)) = compute_cherry_patch_id(repo, oid, &options.paths) {
                    by_patch.entry(pid).or_default().push(*oid);
                }
            }
            for oid in &right_commits {
                if let Ok(Some(pid)) = compute_cherry_patch_id(repo, oid, &options.paths) {
                    if let Some(others) = by_patch.get(&pid) {
                        cherry_equivalent.insert(*oid);
                        cherry_equivalent.extend(others.iter().copied());
                    }
                }
            }
        } else {
            for oid in &right_commits {
                if let Ok(Some(pid)) = compute_cherry_patch_id(repo, oid, &options.paths) {
                    by_patch.entry(pid).or_default().push(*oid);
                }
            }
            for oid in &left_commits {
                if let Ok(Some(pid)) = compute_cherry_patch_id(repo, oid, &options.paths) {
                    if let Some(others) = by_patch.get(&pid) {
                        cherry_equivalent.insert(*oid);
                        cherry_equivalent.extend(others.iter().copied());
                    }
                }
            }
        }
    }

    // Filter left-only / right-only
    if options.left_only {
        ordered.retain(|oid| left_right_map.get(oid) == Some(&true));
    }
    if options.right_only {
        ordered.retain(|oid| left_right_map.get(oid) == Some(&false));
    }

    // Cherry-pick: remove equivalent commits
    if options.cherry_pick {
        ordered.retain(|oid| !cherry_equivalent.contains(oid));
    }

    if options.until_cutoff.is_some() || options.since_cutoff.is_some() {
        let until = options.until_cutoff;
        let since = options.since_cutoff;
        ordered.retain(|oid| {
            let ts = graph.committer_time(*oid);
            if let Some(u) = until {
                if ts > u {
                    return false;
                }
            }
            if let Some(s) = since {
                if ts < s {
                    return false;
                }
            }
            true
        });
    }

    // `--simplify-by-decoration` with a caller-supplied decorated set: run proper
    // history simplification with parent rewriting (Git's `simplify_commit`). A commit is
    // kept iff it is decorated ("tree different" per `rev_compare_tree`), a root, or a
    // relevant merge (>= 2 distinct rewritten relevant parents). `ordered` is already in
    // the walk order (topo for default ordering), so we just retain the kept subset.
    if options.simplify_by_decoration && !options.simplify_by_decoration_oids.is_empty() {
        let keep = compute_simplify_by_decoration_keep_set(
            &mut graph,
            &ordered,
            &options.simplify_by_decoration_oids,
        )?;
        ordered.retain(|oid| keep.contains(oid));
    }

    if options.skip > 0 {
        ordered = ordered.into_iter().skip(options.skip).collect();
    }
    if let Some(max_count) = options.max_count {
        ordered.truncate(max_count);
    }
    if options.reverse {
        ordered.reverse();
    }

    // Collect boundary commits: parents of included commits that are in the excluded set
    let boundary_commits = if options.boundary {
        let included_set: HashSet<ObjectId> = ordered.iter().copied().collect();
        let mut boundary = Vec::new();
        let mut boundary_seen = HashSet::new();
        for &oid in &ordered {
            if let Ok(parents) = graph.parents_of(oid).map(|p| p.to_vec()) {
                for parent in parents {
                    if !included_set.contains(&parent) && boundary_seen.insert(parent) {
                        boundary.push(parent);
                    }
                }
            }
        }
        boundary
    } else {
        Vec::new()
    };

    // Filter kept objects when --no-kept-objects is set
    let kept_set = if options.no_kept_objects {
        kept_object_ids(repo).unwrap_or_default()
    } else {
        HashSet::new()
    };

    if options.no_kept_objects {
        ordered.retain(|oid| !kept_set.contains(oid));
    }

    if options.unpacked_only {
        let packed = packed_object_set(repo);
        ordered.retain(|oid| !packed.contains(oid));
    }

    let commit_tips_set: HashSet<ObjectId> = object_walk_tip_commits.iter().copied().collect();
    let objects_print_commit: Vec<bool> = if options.objects {
        ordered
            .iter()
            .map(|&c| {
                let user_given = !options.filter_provided_objects && commit_tips_set.contains(&c);
                object_walk_print_commit_line(
                    options.filter_provided_objects,
                    options.filter.as_ref(),
                    user_given,
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    let sparse_lines = sparse_oid_lines_from_filter(repo, options.filter.as_ref())?;
    let skip_trees = skip_tree_descent_for_object_type_filter(options.filter.as_ref());
    let walk_cfg = repo_config.as_ref().clone();
    let promisor_repo = crate::promisor::repo_treats_promisor_packs(&repo.git_dir, &walk_cfg);
    let object_walk_missing_action = if options.objects
        && options.missing_action == MissingAction::Error
        && (promisor_repo || options.exclude_promisor_objects)
    {
        MissingAction::Allow
    } else {
        options.missing_action
    };
    let bitmap_object_format = false;
    let omit_object_paths = options.bitmap_oid_only_objects;
    // `--unpacked` selects unpacked commits for the revision walk. Git's object walk still emits
    // the full object closure for those commits, including tree/blob objects that already live in a
    // pack (t6000, t6113). Do not pass the packed set into tree traversal here.
    let packed_set = None;

    // Git only enables provisional omit recursion (`omits` non-NULL) with `--filter-print-omitted`.
    let collect_tree_omits = options.filter_print_omitted;

    // Collect reachable objects if --objects
    let (objects, omitted_objects, missing_objects, per_commit_object_counts, object_segments) =
        if options.objects {
            repo.odb.with_pack_read_context(|| {
                let excluded_object_ids = excluded_object_root_ids(
                    repo,
                    &negative_object_roots,
                    object_walk_missing_action,
                    sparse_lines.as_deref(),
                    skip_trees,
                    omit_object_paths,
                    collect_tree_omits,
                )?;
                let filter_provided = options.filter_provided_objects;
                let (mut objs, mut omit, mut miss, mut counts, mut segments) =
                    if options.in_commit_order {
                        let (o, om, mi, c) = collect_reachable_objects_in_commit_order(
                            repo,
                            &mut graph,
                            &ordered,
                            &object_roots,
                            &tip_annotated_tag_by_commit,
                            options.filter.as_ref(),
                            filter_provided,
                            object_walk_missing_action,
                            sparse_lines.as_deref(),
                            skip_trees,
                            omit_object_paths,
                            packed_set.as_ref(),
                            collect_tree_omits,
                        )?;
                        (o, om, mi, c, Vec::<Vec<(ObjectId, String)>>::new())
                    } else {
                        let (o, om, mi, segments) = collect_reachable_objects_segmented(
                            repo,
                            &mut graph,
                            &ordered,
                            &object_roots,
                            &tip_annotated_tag_by_commit,
                            options.filter.as_ref(),
                            filter_provided,
                            object_walk_missing_action,
                            sparse_lines.as_deref(),
                            skip_trees,
                            omit_object_paths,
                            packed_set.as_ref(),
                            collect_tree_omits,
                        )?;
                        (o, om, mi, Vec::new(), segments)
                    };
                if !excluded_object_ids.is_empty() {
                    if !counts.is_empty() {
                        let mut offset = 0usize;
                        for count in &mut counts {
                            let end = offset.saturating_add(*count);
                            *count = objs[offset..end]
                                .iter()
                                .filter(|(oid, _)| !excluded_object_ids.contains(oid))
                                .count();
                            offset = end;
                        }
                    }
                    objs.retain(|(oid, _)| !excluded_object_ids.contains(oid));
                    omit.retain(|oid| !excluded_object_ids.contains(oid));
                    miss.retain(|oid| !excluded_object_ids.contains(oid));
                    for segment in &mut segments {
                        segment.retain(|(oid, _)| !excluded_object_ids.contains(oid));
                    }
                }
                if options.no_kept_objects {
                    objs.retain(|(oid, _)| !kept_set.contains(oid));
                }
                if options.exclude_promisor_objects {
                    objs.retain(|(oid, _)| !excluded_promisor.contains(oid));
                    for segment in &mut segments {
                        segment.retain(|(oid, _)| !excluded_promisor.contains(oid));
                    }
                }
                if !options.paths.is_empty() && !omit_object_paths {
                    retain_objects_matching_pathspecs(&mut objs, &options.paths);
                    let mut seen_oids: HashSet<ObjectId> =
                        objs.iter().map(|(oid, _)| *oid).collect();
                    for (oid, path) in
                        collect_pathspec_matching_tree_objects(repo, &ordered, &options.paths)?
                    {
                        if seen_oids.insert(oid) {
                            objs.push((oid, path));
                        }
                    }
                    for segment in &mut segments {
                        retain_objects_matching_pathspecs(segment, &options.paths);
                    }
                    if !counts.is_empty() {
                        counts = segments.iter().map(Vec::len).collect();
                    }
                }
                Ok::<_, Error>((objs, omit, miss, counts, segments))
            })
        } else {
            Ok::<_, Error>((Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()))
        }?;

    let omitted_objects = if omitted_objects.is_empty() {
        omitted_objects
    } else {
        let emitted: HashSet<ObjectId> = objects.iter().map(|(o, _)| *o).collect();
        omitted_objects
            .into_iter()
            .filter(|o| !emitted.contains(o))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };

    let missing_objects = if traversal_missing.is_empty() {
        missing_objects
    } else {
        let mut seen = HashSet::new();
        traversal_missing
            .into_iter()
            .chain(missing_objects)
            .filter(|oid| seen.insert(*oid))
            .collect()
    };

    Ok(RevListResult {
        commits: ordered,
        objects,
        omitted_objects,
        missing_objects,
        boundary_commits,
        left_right_map,
        cherry_equivalent,
        per_commit_object_counts,
        object_walk_tips: object_walk_tip_commits,
        objects_print_commit,
        object_segments,
        bitmap_object_format,
        reachable_commit_count: None,
        reachable_non_commit_object_count: None,
        tip_annotated_tag_by_commit,
    })
}

/// Parse a raw revision token into positive and negative specs.
///
/// Supports:
/// - `<a>..<b>` => negative `<a>`, positive `<b>`
/// - `^<rev>` => negative `<rev>`
/// - `<rev>` => positive `<rev>`
#[must_use]
pub fn split_revision_token(token: &str) -> (Vec<String>, Vec<String>) {
    if let Some((lhs, rhs)) = crate::rev_parse::split_double_dot_range(token) {
        let positive = if rhs.is_empty() {
            "HEAD".to_owned()
        } else {
            rhs.to_owned()
        };
        let negative = if lhs.is_empty() {
            "HEAD".to_owned()
        } else {
            lhs.to_owned()
        };
        return (vec![positive], vec![negative]);
    }
    if let Some(rest) = token.strip_prefix('^') {
        return (Vec::new(), vec![rest.to_owned()]);
    }
    (vec![token.to_owned()], Vec::new())
}

fn ansi_color_from_name(name: &str) -> String {
    match name {
        "red" => "\x1b[31m".to_owned(),
        "green" => "\x1b[32m".to_owned(),
        "yellow" => "\x1b[33m".to_owned(),
        "blue" => "\x1b[34m".to_owned(),
        "magenta" => "\x1b[35m".to_owned(),
        "cyan" => "\x1b[36m".to_owned(),
        "white" => "\x1b[37m".to_owned(),
        "bold" => "\x1b[1m".to_owned(),
        "dim" => "\x1b[2m".to_owned(),
        "ul" | "underline" => "\x1b[4m".to_owned(),
        "blink" => "\x1b[5m".to_owned(),
        "reverse" => "\x1b[7m".to_owned(),
        "reset" => "\x1b[m".to_owned(),
        _ => String::new(),
    }
}

fn color_name_to_code(name: &str) -> Option<u8> {
    match name {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "default" => Some(9),
        _ => None,
    }
}

fn ansi_color_from_spec(spec: &str) -> String {
    if spec == "reset" {
        return "\x1b[m".to_owned();
    }
    let mut attrs = Vec::new();
    let mut fg = None;
    let mut bg = None;
    for part in spec.split_whitespace() {
        match part {
            "bold" => attrs.push("1".to_owned()),
            "dim" => attrs.push("2".to_owned()),
            "italic" => attrs.push("3".to_owned()),
            "ul" | "underline" => attrs.push("4".to_owned()),
            "blink" => attrs.push("5".to_owned()),
            "reverse" => attrs.push("7".to_owned()),
            "strike" => attrs.push("9".to_owned()),
            "nobold" | "nodim" => attrs.push("22".to_owned()),
            "noitalic" => attrs.push("23".to_owned()),
            "noul" | "nounderline" => attrs.push("24".to_owned()),
            "noblink" => attrs.push("25".to_owned()),
            "noreverse" => attrs.push("27".to_owned()),
            "nostrike" => attrs.push("29".to_owned()),
            _ => {
                if let Some(code) = color_name_to_code(part) {
                    if fg.is_none() {
                        fg = Some(format!("{}", 30 + code));
                    } else {
                        bg = Some(format!("{}", 40 + code));
                    }
                }
            }
        }
    }
    let mut codes = attrs;
    if let Some(code) = fg {
        codes.push(code);
    }
    if let Some(code) = bg {
        codes.push(code);
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn format_relative_date(diff: i64) -> String {
    if diff < 0 {
        "in the future".to_owned()
    } else if diff < 60 {
        format!("{} seconds ago", diff)
    } else if diff < 3600 {
        let m = diff / 60;
        if m == 1 {
            "1 minute ago".to_owned()
        } else {
            format!("{m} minutes ago")
        }
    } else if diff < 86400 {
        let h = diff / 3600;
        if h == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{h} hours ago")
        }
    } else if diff < 86400 * 30 {
        let d = diff / 86400;
        if d == 1 {
            "1 day ago".to_owned()
        } else {
            format!("{d} days ago")
        }
    } else if diff < 86400 * 365 {
        let months = diff / (86400 * 30);
        if months == 1 {
            "1 month ago".to_owned()
        } else {
            format!("{months} months ago")
        }
    } else {
        let years = diff / (86400 * 365);
        if years == 1 {
            "1 year ago".to_owned()
        } else {
            format!("{years} years ago")
        }
    }
}

/// Render one commit according to the selected output mode.
///
/// # Errors
///
/// Returns object decode errors when commit metadata is required.
pub fn render_commit(
    repo: &Repository,
    oid: ObjectId,
    mode: &OutputMode,
    abbrev_len: usize,
) -> Result<String> {
    render_commit_with_color(repo, oid, mode, abbrev_len, false)
}

/// Render one commit, optionally with ANSI color for `%C` placeholders.
pub fn render_commit_with_color(
    repo: &Repository,
    oid: ObjectId,
    mode: &OutputMode,
    abbrev_len: usize,
    use_color: bool,
) -> Result<String> {
    match mode {
        OutputMode::OidOnly => Ok(format!("{oid}")),
        OutputMode::Parents => {
            let mut out = format!("{oid}");
            let commit = load_commit(repo, oid)?;
            for parent in commit.parents {
                out.push(' ');
                out.push_str(&parent.to_hex());
            }
            Ok(out)
        }
        OutputMode::Format(fmt) => {
            let commit = load_commit(repo, oid)?;
            let wall_now = repo.wall_clock_epoch();
            let subject = commit.message.lines().next().unwrap_or_default();
            let hex = oid.to_hex();

            // Handle named pretty formats
            match fmt.as_str() {
                "oneline" => {
                    return Ok(format!("{} {}", hex, subject));
                }
                "short" => {
                    fn fmt_ident(ident: &str) -> String {
                        let name = if let Some(bracket) = ident.find('<') {
                            ident[..bracket].trim()
                        } else {
                            ident.trim()
                        };
                        let email = if let Some(start) = ident.find('<') {
                            if let Some(end) = ident.find('>') {
                                &ident[start..=end]
                            } else {
                                ""
                            }
                        } else {
                            ""
                        };
                        format!("{} {}", name, email)
                    }
                    let mut out = String::new();
                    out.push_str(&format!("Author: {}\n", fmt_ident(&commit.author)));
                    out.push('\n');
                    out.push_str(&format!("    {}\n", subject));
                    out.push('\n');
                    return Ok(out);
                }
                "medium" => {
                    fn extract_ident_display(ident: &str) -> String {
                        let name = if let Some(bracket) = ident.find('<') {
                            ident[..bracket].trim()
                        } else {
                            ident.trim()
                        };
                        let email = if let Some(start) = ident.find('<') {
                            if let Some(end) = ident.find('>') {
                                &ident[start..=end]
                            } else {
                                ""
                            }
                        } else {
                            ""
                        };
                        format!("{} {}", name, email)
                    }
                    fn format_default_date(ident: &str) -> String {
                        let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
                        if parts.len() < 2 {
                            return String::new();
                        }
                        let ts_str = parts[1];
                        let offset_str = parts[0];
                        let ts: i64 = match ts_str.parse() {
                            Ok(v) => v,
                            Err(_) => return format!("{ts_str} {offset_str}"),
                        };
                        let tz_bytes = offset_str.as_bytes();
                        let tz_secs: i64 = if tz_bytes.len() >= 5 {
                            let sign = if tz_bytes[0] == b'-' { -1i64 } else { 1i64 };
                            let h: i64 = offset_str[1..3].parse().unwrap_or(0);
                            let m: i64 = offset_str[3..5].parse().unwrap_or(0);
                            sign * (h * 3600 + m * 60)
                        } else {
                            0
                        };
                        let adjusted = ts + tz_secs;
                        let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                        let weekday = match dt.weekday() {
                            time::Weekday::Monday => "Mon",
                            time::Weekday::Tuesday => "Tue",
                            time::Weekday::Wednesday => "Wed",
                            time::Weekday::Thursday => "Thu",
                            time::Weekday::Friday => "Fri",
                            time::Weekday::Saturday => "Sat",
                            time::Weekday::Sunday => "Sun",
                        };
                        let month = match dt.month() {
                            time::Month::January => "Jan",
                            time::Month::February => "Feb",
                            time::Month::March => "Mar",
                            time::Month::April => "Apr",
                            time::Month::May => "May",
                            time::Month::June => "Jun",
                            time::Month::July => "Jul",
                            time::Month::August => "Aug",
                            time::Month::September => "Sep",
                            time::Month::October => "Oct",
                            time::Month::November => "Nov",
                            time::Month::December => "Dec",
                        };
                        format!(
                            "{} {} {} {:02}:{:02}:{:02} {} {}",
                            weekday,
                            month,
                            dt.day(),
                            dt.hour(),
                            dt.minute(),
                            dt.second(),
                            dt.year(),
                            offset_str
                        )
                    }
                    let mut out = String::new();
                    out.push_str(&format!(
                        "Author: {}\n",
                        extract_ident_display(&commit.author)
                    ));
                    out.push_str(&format!(
                        "Date:   {}\n",
                        format_default_date(&commit.author)
                    ));
                    out.push('\n');
                    for line in commit.message.lines() {
                        out.push_str(&format!("    {}\n", line));
                    }
                    return Ok(out);
                }
                _ => {}
            }

            let raw_fmt = if let Some(t) = fmt.strip_prefix("format:") {
                t
            } else if let Some(t) = fmt.strip_prefix("tformat:") {
                t
            } else {
                fmt.as_str()
            };

            // Verify the commit signature only when a `%G` placeholder is present
            // in the format (matches git's lazy `%G?`/`%GS` evaluation).
            let signature: Option<crate::signing::SignatureCheck> = if raw_fmt.contains("%G") {
                repo.odb.read(&oid).ok().map(|obj| {
                    let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
                    match crate::signing::GpgConfig::from_config(&config) {
                        Ok(cfg) => crate::signing::verify_commit(&cfg, &obj.data)
                            .unwrap_or_else(|_| crate::signing::SignatureCheck::default_none()),
                        Err(_) => crate::signing::SignatureCheck::default_none(),
                    }
                })
            } else {
                None
            };
            // Body: everything after the first line (skip blank separator line)
            let body = {
                let mut lines = commit.message.lines();
                lines.next(); // skip subject
                              // Skip optional blank line after subject
                if let Some(blank) = lines.next() {
                    if blank.is_empty() {
                        lines.collect::<Vec<_>>().join("\n")
                    } else {
                        std::iter::once(blank)
                            .chain(lines)
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                } else {
                    String::new()
                }
            };
            let tree_hex = commit.tree.to_hex();
            let parent_hexes: Vec<String> = commit.parents.iter().map(|p| p.to_hex()).collect();
            let parent_abbrevs: Vec<String> = commit
                .parents
                .iter()
                .map(|p| {
                    let hex = p.to_hex();
                    let n = abbrev_len.clamp(4, 40).min(hex.len());
                    hex[..n].to_string()
                })
                .collect();

            // Extract name/email components from ident strings
            fn extract_name(ident: &str) -> &str {
                if let Some(bracket) = ident.find('<') {
                    ident[..bracket].trim()
                } else {
                    ident.trim()
                }
            }
            fn extract_email(ident: &str) -> &str {
                if let Some(start) = ident.find('<') {
                    if let Some(end) = ident.find('>') {
                        return &ident[start + 1..end];
                    }
                }
                ""
            }
            fn extract_timestamp(ident: &str) -> String {
                match parse_signature_times(ident) {
                    Some(p) => p.unix_seconds.to_string(),
                    None => String::new(),
                }
            }
            fn weekday_str(dt: &time::OffsetDateTime) -> &'static str {
                match dt.weekday() {
                    time::Weekday::Monday => "Mon",
                    time::Weekday::Tuesday => "Tue",
                    time::Weekday::Wednesday => "Wed",
                    time::Weekday::Thursday => "Thu",
                    time::Weekday::Friday => "Fri",
                    time::Weekday::Saturday => "Sat",
                    time::Weekday::Sunday => "Sun",
                }
            }
            fn month_str(dt: &time::OffsetDateTime) -> &'static str {
                match dt.month() {
                    time::Month::January => "Jan",
                    time::Month::February => "Feb",
                    time::Month::March => "Mar",
                    time::Month::April => "Apr",
                    time::Month::May => "May",
                    time::Month::June => "Jun",
                    time::Month::July => "Jul",
                    time::Month::August => "Aug",
                    time::Month::September => "Sep",
                    time::Month::October => "Oct",
                    time::Month::November => "Nov",
                    time::Month::December => "Dec",
                }
            }
            fn extract_email_local(ident: &str) -> &str {
                let email = extract_email(ident);
                if let Some(at) = email.find('@') {
                    &email[..at]
                } else {
                    email
                }
            }
            fn extract_date_default(ident: &str) -> String {
                let Some(p) = parse_signature_times(ident) else {
                    return String::new();
                };
                let offset_str = ident.get(p.tz_hhmm_range.clone()).unwrap_or("+0000");
                let adjusted = p.unix_seconds + p.tz_offset_secs;
                let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                format!(
                    "{} {} {} {:02}:{:02}:{:02} {} {}",
                    weekday_str(&dt),
                    month_str(&dt),
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    dt.year(),
                    offset_str
                )
            }
            fn extract_date_rfc2822(ident: &str) -> String {
                let Some(p) = parse_signature_times(ident) else {
                    return String::new();
                };
                let offset_str = ident.get(p.tz_hhmm_range.clone()).unwrap_or("+0000");
                let adjusted = p.unix_seconds + p.tz_offset_secs;
                let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                format!(
                    "{}, {} {} {} {:02}:{:02}:{:02} {}",
                    weekday_str(&dt),
                    dt.day(),
                    month_str(&dt),
                    dt.year(),
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    offset_str
                )
            }
            fn extract_date_short(ident: &str) -> String {
                let Some(p) = parse_signature_times(ident) else {
                    return String::new();
                };
                let adjusted = p.unix_seconds + p.tz_offset_secs;
                let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day())
            }
            fn extract_date_iso(ident: &str) -> String {
                let Some(p) = parse_signature_times(ident) else {
                    return String::new();
                };
                let offset_str = ident.get(p.tz_hhmm_range.clone()).unwrap_or("+0000");
                let adjusted = p.unix_seconds + p.tz_offset_secs;
                let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
                    dt.year(),
                    dt.month() as u8,
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    offset_str
                )
            }

            // Alignment/truncation state for %<(N), %>(N), %><(N) directives
            #[derive(Clone, Copy)]
            enum Align {
                Left,
                Right,
                Center,
            }
            #[derive(Clone, Copy)]
            enum ColTruncate {
                None,
                End,
                Start,
                Middle,
            }
            struct ColSpec {
                width: usize,
                align: Align,
                trunc: ColTruncate,
            }
            fn apply_col(spec: &ColSpec, s: &str) -> String {
                let char_len = s.chars().count();
                if char_len > spec.width {
                    match spec.trunc {
                        ColTruncate::None => s.to_owned(),
                        ColTruncate::End => {
                            let mut out: String =
                                s.chars().take(spec.width.saturating_sub(2)).collect();
                            out.push_str("..");
                            out
                        }
                        ColTruncate::Start => {
                            let skip = char_len - spec.width + 2;
                            let mut out = String::from("..");
                            out.extend(s.chars().skip(skip));
                            out
                        }
                        ColTruncate::Middle => {
                            let keep = spec.width.saturating_sub(2);
                            let left_half = keep / 2;
                            let right_half = keep - left_half;
                            let mut out: String = s.chars().take(left_half).collect();
                            out.push_str("..");
                            out.extend(s.chars().skip(char_len - right_half));
                            out
                        }
                    }
                } else {
                    let pad = spec.width - char_len;
                    match spec.align {
                        Align::Left => {
                            let mut out = s.to_owned();
                            for _ in 0..pad {
                                out.push(' ');
                            }
                            out
                        }
                        Align::Right => {
                            let mut out = String::new();
                            for _ in 0..pad {
                                out.push(' ');
                            }
                            out.push_str(s);
                            out
                        }
                        Align::Center => {
                            let left = pad / 2;
                            let right = pad - left;
                            let mut out = String::new();
                            for _ in 0..left {
                                out.push(' ');
                            }
                            out.push_str(s);
                            for _ in 0..right {
                                out.push(' ');
                            }
                            out
                        }
                    }
                }
            }
            fn parse_col_spec(
                chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
                align: Align,
            ) -> Option<ColSpec> {
                // Consume '('
                if chars.peek() != Some(&'(') {
                    return None;
                }
                chars.next();
                let mut num_str = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() {
                        num_str.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let width: usize = num_str.parse().ok()?;
                let trunc = if chars.peek() == Some(&',') {
                    chars.next(); // consume comma
                    let mut mode = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == ')' {
                            break;
                        }
                        mode.push(c);
                        chars.next();
                    }
                    match mode.as_str() {
                        "trunc" => ColTruncate::End,
                        "ltrunc" => ColTruncate::Start,
                        "mtrunc" => ColTruncate::Middle,
                        _ => ColTruncate::None,
                    }
                } else {
                    ColTruncate::None
                };
                // Consume ')'
                if chars.peek() == Some(&')') {
                    chars.next();
                }
                Some(ColSpec {
                    width,
                    align,
                    trunc,
                })
            }

            let mut pending_col: Option<ColSpec> = None;
            let mut rendered = String::new();
            let mut chars = raw_fmt.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    rendered.push(ch);
                    continue;
                }
                // Check for alignment directives: %<(...), %>(...), %><(...)
                if chars.peek() == Some(&'<') {
                    chars.next();
                    if let Some(spec) = parse_col_spec(&mut chars, Align::Left) {
                        pending_col = Some(spec);
                    }
                    continue;
                }
                if chars.peek() == Some(&'>') {
                    chars.next();
                    if chars.peek() == Some(&'<') {
                        chars.next(); // %><(...)
                        if let Some(spec) = parse_col_spec(&mut chars, Align::Center) {
                            pending_col = Some(spec);
                        }
                    } else if chars.peek() == Some(&'>') {
                        chars.next(); // %>>(...)
                        if let Some(spec) = parse_col_spec(&mut chars, Align::Right) {
                            pending_col = Some(spec);
                        }
                    } else if let Some(spec) = parse_col_spec(&mut chars, Align::Right) {
                        pending_col = Some(spec);
                    }
                    continue;
                }

                // Helper macro-like: expand the placeholder, then apply pending_col
                let mut expanded = String::new();
                let target = if pending_col.is_some() {
                    &mut expanded
                } else {
                    &mut rendered
                };
                match chars.peek() {
                    Some('%') => {
                        chars.next();
                        target.push('%');
                    }
                    Some('H') => {
                        chars.next();
                        target.push_str(&oid.to_hex());
                    }
                    Some('h') => {
                        chars.next();
                        let hex = oid.to_hex();
                        let n = abbrev_len.clamp(4, 40).min(hex.len());
                        target.push_str(&hex[..n]);
                    }
                    Some('T') => {
                        chars.next();
                        target.push_str(&tree_hex);
                    }
                    Some('t') => {
                        chars.next();
                        let n = abbrev_len.clamp(4, 40).min(tree_hex.len());
                        target.push_str(&tree_hex[..n]);
                    }
                    Some('P') => {
                        chars.next();
                        target.push_str(&parent_hexes.join(" "));
                    }
                    Some('p') => {
                        chars.next();
                        target.push_str(&parent_abbrevs.join(" "));
                    }
                    Some('n') => {
                        chars.next();
                        target.push('\n');
                    }
                    Some('s') => {
                        chars.next();
                        target.push_str(subject);
                    }
                    Some('b') => {
                        chars.next();
                        target.push_str(&body);
                        if !body.is_empty() {
                            target.push('\n');
                        }
                    }
                    Some('B') => {
                        chars.next();
                        target.push_str(&commit.message);
                    }
                    Some('a') => {
                        chars.next();
                        match chars.next() {
                            Some('n') => target.push_str(extract_name(&commit.author)),
                            Some('N') => target.push_str(extract_name(&commit.author)),
                            Some('e') => target.push_str(extract_email(&commit.author)),
                            Some('E') => target.push_str(extract_email(&commit.author)),
                            Some('l') => target.push_str(extract_email_local(&commit.author)),
                            Some('d') => target.push_str(&extract_date_default(&commit.author)),
                            Some('D') => target.push_str(&extract_date_rfc2822(&commit.author)),
                            Some('t') => target.push_str(&extract_timestamp(&commit.author)),
                            Some('s') => target.push_str(&extract_date_short(&commit.author)),
                            Some('i') => target.push_str(&extract_date_iso(&commit.author)),
                            Some('I') => {
                                let Some(p) = parse_signature_times(&commit.author) else {
                                    break;
                                };
                                let adjusted = p.unix_seconds + p.tz_offset_secs;
                                let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                                let sign_ch = if p.tz_offset_secs >= 0 { '+' } else { '-' };
                                let abs_off = p.tz_offset_secs.unsigned_abs();
                                target.push_str(&format!(
                                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
                                    dt.year(),
                                    dt.month() as u8,
                                    dt.day(),
                                    dt.hour(),
                                    dt.minute(),
                                    dt.second(),
                                    sign_ch,
                                    abs_off / 3600,
                                    (abs_off % 3600) / 60
                                ));
                            }
                            Some('r') => {
                                let Some(p) = parse_signature_times(&commit.author) else {
                                    break;
                                };
                                target.push_str(&format_relative_date(wall_now - p.unix_seconds));
                            }
                            Some(other) => {
                                target.push('%');
                                target.push('a');
                                target.push(other);
                            }
                            None => {
                                target.push('%');
                                target.push('a');
                            }
                        }
                    }
                    Some('c') => {
                        chars.next();
                        match chars.next() {
                            Some('n') => target.push_str(extract_name(&commit.committer)),
                            Some('N') => target.push_str(extract_name(&commit.committer)),
                            Some('e') => target.push_str(extract_email(&commit.committer)),
                            Some('E') => target.push_str(extract_email(&commit.committer)),
                            Some('l') => target.push_str(extract_email_local(&commit.committer)),
                            Some('d') => target.push_str(&extract_date_default(&commit.committer)),
                            Some('D') => target.push_str(&extract_date_rfc2822(&commit.committer)),
                            Some('t') => target.push_str(&extract_timestamp(&commit.committer)),
                            Some('s') => target.push_str(&extract_date_short(&commit.committer)),
                            Some('i') => target.push_str(&extract_date_iso(&commit.committer)),
                            Some('I') => {
                                let Some(p) = parse_signature_times(&commit.committer) else {
                                    break;
                                };
                                let adjusted = p.unix_seconds + p.tz_offset_secs;
                                let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                                let sign_ch = if p.tz_offset_secs >= 0 { '+' } else { '-' };
                                let abs_off = p.tz_offset_secs.unsigned_abs();
                                target.push_str(&format!(
                                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
                                    dt.year(),
                                    dt.month() as u8,
                                    dt.day(),
                                    dt.hour(),
                                    dt.minute(),
                                    dt.second(),
                                    sign_ch,
                                    abs_off / 3600,
                                    (abs_off % 3600) / 60
                                ));
                            }
                            Some('r') => {
                                let Some(p) = parse_signature_times(&commit.committer) else {
                                    break;
                                };
                                target.push_str(&format_relative_date(wall_now - p.unix_seconds));
                            }
                            Some(other) => {
                                target.push('%');
                                target.push('c');
                                target.push(other);
                            }
                            None => {
                                target.push('%');
                                target.push('c');
                            }
                        }
                    }
                    Some('x') => {
                        // Hex escape: %xNN
                        chars.next();
                        let mut hex_str = String::new();
                        if let Some(&c1) = chars.peek() {
                            if c1.is_ascii_hexdigit() {
                                hex_str.push(c1);
                                chars.next();
                            }
                        }
                        if let Some(&c2) = chars.peek() {
                            if c2.is_ascii_hexdigit() {
                                hex_str.push(c2);
                                chars.next();
                            }
                        }
                        if let Ok(byte) = u8::from_str_radix(&hex_str, 16) {
                            target.push(byte as char);
                        }
                    }
                    Some('C') => {
                        chars.next();
                        if chars.peek() == Some(&'(') {
                            chars.next();
                            let mut spec = String::new();
                            for c in chars.by_ref() {
                                if c == ')' {
                                    break;
                                }
                                spec.push(c);
                            }
                            let (force, color_spec) =
                                if let Some(rest) = spec.strip_prefix("always,") {
                                    (true, rest)
                                } else if let Some(rest) = spec.strip_prefix("auto,") {
                                    (false, rest)
                                } else if spec == "auto" {
                                    if use_color {
                                        target.push_str("\x1b[m");
                                    }
                                    continue;
                                } else {
                                    (false, spec.as_str())
                                };
                            if use_color || force {
                                target.push_str(&ansi_color_from_spec(color_spec));
                            }
                        } else {
                            // Named colors: %Cred, %Cgreen, %Cblue, %Creset, %Cbold
                            // Must match known names only, not consume trailing text
                            let remaining: String = chars.clone().collect();
                            let known = [
                                "reset", "red", "green", "blue", "yellow", "magenta", "cyan",
                                "white", "bold", "dim", "ul",
                            ];
                            let mut matched = false;
                            for name in &known {
                                if remaining.starts_with(name) {
                                    for _ in 0..name.len() {
                                        chars.next();
                                    }
                                    if use_color {
                                        target.push_str(&ansi_color_from_name(name));
                                    }
                                    matched = true;
                                    break;
                                }
                            }
                            if !matched {
                                // Unknown color name — consume alphanumerics
                                while let Some(&c) = chars.peek() {
                                    if c.is_alphanumeric() {
                                        chars.next();
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Some('w') => {
                        // %w(...) — wrapping directive, consume and ignore for now
                        chars.next();
                        if chars.peek() == Some(&'(') {
                            chars.next();
                            for c in chars.by_ref() {
                                if c == ')' {
                                    break;
                                }
                            }
                        }
                    }
                    Some('+') => {
                        // %+x — conditional newline: if next placeholder is non-empty, prepend newline
                        chars.next();
                        // Expand the following placeholder
                        if chars.peek() == Some(&'%') {
                            // The %+ applies to the NEXT expanded value
                            // For simplicity, treat %+x as: if %x is non-empty, emit '\n' + value
                            // This needs the *next* placeholder's value
                        }
                        // Simple: consume the next char as a format code; prepend \n if non-empty
                        let mut sub = String::new();
                        if let Some(&nc) = chars.peek() {
                            match nc {
                                'b' => {
                                    chars.next();
                                    sub.push_str(&body);
                                    if !body.is_empty() {
                                        sub.push('\n');
                                    }
                                }
                                's' => {
                                    chars.next();
                                    sub.push_str(subject);
                                }
                                _ => {
                                    chars.next();
                                    sub.push('%');
                                    sub.push('+');
                                    sub.push(nc);
                                }
                            }
                        }
                        if !sub.is_empty() {
                            target.push('\n');
                            target.push_str(&sub);
                        }
                    }
                    Some('-') => {
                        // %-x — conditional: suppress newline before placeholder if empty
                        chars.next();
                        // Consume the next format code
                        if let Some(&nc) = chars.peek() {
                            match nc {
                                'b' => {
                                    chars.next();
                                    if !body.is_empty() {
                                        target.push_str(&body);
                                        target.push('\n');
                                    }
                                }
                                's' => {
                                    chars.next();
                                    target.push_str(subject);
                                }
                                _ => {
                                    chars.next();
                                    target.push('%');
                                    target.push('-');
                                    target.push(nc);
                                }
                            }
                        }
                    }
                    Some('d') => {
                        // Decorations — output empty for now
                        chars.next();
                    }
                    Some('D') => {
                        // Decorations without parens — output empty for now
                        chars.next();
                    }
                    Some('e') => {
                        // Encoding
                        chars.next();
                        if let Some(encoding) = &commit.encoding {
                            target.push_str(encoding);
                        }
                    }
                    Some('g') => {
                        // Reflog placeholders: %gD, %gd, %gs, %gn, %ge, etc.
                        chars.next();
                        if let Some(&_nc) = chars.peek() {
                            chars.next(); // consume the sub-specifier
                                          // For non-reflog commits, these expand to empty
                        }
                    }
                    Some('G') => {
                        // Signature placeholders (%G?, %GS, %GK, %GF, %GP, %GT, %GG).
                        // Unknown `%G<x>` (and a bare trailing `%G`) pass through
                        // literally, mirroring git's pretty.c `return 0`.
                        use crate::signing::{SignatureCheck, TrustLevel};
                        let default_sig;
                        let sig: &SignatureCheck = match &signature {
                            Some(s) => s,
                            None => {
                                default_sig = SignatureCheck::default_none();
                                &default_sig
                            }
                        };
                        let mut lookahead = chars.clone();
                        lookahead.next(); // consume 'G'
                        let sub = lookahead.peek().copied();
                        let handled = match sub {
                            Some('?') => {
                                let ch = if sig.result == 'G'
                                    && matches!(
                                        sig.trust_level,
                                        TrustLevel::Undefined | TrustLevel::Never
                                    ) {
                                    'U'
                                } else {
                                    sig.result
                                };
                                target.push(ch);
                                true
                            }
                            Some('S') => {
                                target.push_str(sig.signer.as_deref().unwrap_or(""));
                                true
                            }
                            Some('K') => {
                                target.push_str(sig.key.as_deref().unwrap_or(""));
                                true
                            }
                            Some('F') => {
                                target.push_str(sig.fingerprint.as_deref().unwrap_or(""));
                                true
                            }
                            Some('P') => {
                                target
                                    .push_str(sig.primary_key_fingerprint.as_deref().unwrap_or(""));
                                true
                            }
                            Some('T') => {
                                target.push_str(sig.trust_level.display_key());
                                true
                            }
                            Some('G') => {
                                target.push_str(&sig.output);
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            chars.next(); // consume 'G'
                            chars.next(); // consume sub-char
                        } else {
                            target.push('%');
                        }
                    }
                    Some(&other) => {
                        chars.next();
                        target.push('%');
                        target.push(other);
                    }
                    None => target.push('%'),
                }
                // Apply pending column formatting
                if let Some(spec) = pending_col.take() {
                    let formatted = apply_col(&spec, &expanded);
                    rendered.push_str(&formatted);
                }
            }
            Ok(rendered)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpectedObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ExpectedObjectKind {
    fn from_object_kind(kind: ObjectKind) -> Option<Self> {
        match kind {
            ObjectKind::Commit => Some(Self::Commit),
            ObjectKind::Tree => Some(Self::Tree),
            ObjectKind::Blob => Some(Self::Blob),
            ObjectKind::Tag => Some(Self::Tag),
        }
    }

    fn from_tag_type(kind: &str) -> Option<Self> {
        match kind {
            "commit" => Some(Self::Commit),
            "tree" => Some(Self::Tree),
            "blob" => Some(Self::Blob),
            "tag" => Some(Self::Tag),
            _ => None,
        }
    }

    fn matches(self, kind: ObjectKind) -> bool {
        matches!(
            (self, kind),
            (Self::Commit, ObjectKind::Commit)
                | (Self::Tree, ObjectKind::Tree)
                | (Self::Blob, ObjectKind::Blob)
                | (Self::Tag, ObjectKind::Tag)
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }
}

/// Non-commit root from revision arguments (`tag`, `rev:path`, raw tree/blob OID), for object walks.
#[derive(Clone, Debug)]
pub struct ObjectWalkRoot {
    /// Object id of the peeled non-commit (tree or blob) or tag target.
    pub oid: ObjectId,
    pub input: String,
    /// Path within the tree for `rev:path` blob roots.
    pub root_path: Option<String>,
}

#[derive(Clone, Debug)]
struct RootObject {
    oid: ObjectId,
    input: String,
    expected_kind: Option<ExpectedObjectKind>,
    root_path: Option<String>,
    /// When the user named an annotated tag, the tag object to emit before walking [`Self::oid`].
    wrap_with_tag: Option<ObjectId>,
}

fn indexed_object_roots(repo: &Repository) -> Result<Vec<RootObject>> {
    let Some(_) = &repo.work_tree else {
        return Ok(Vec::new());
    };
    let index_path = repo.git_dir.join("index");
    if !index_path.is_file() {
        return Ok(Vec::new());
    }

    let idx = Index::load(&index_path)?;
    let mut roots = Vec::new();
    for e in &idx.entries {
        if e.stage() != 0 || e.mode == MODE_GITLINK || e.mode == MODE_TREE {
            continue;
        }
        let path_str = String::from_utf8_lossy(&e.path).into_owned();
        roots.push(RootObject {
            oid: e.oid,
            input: format!(":{path_str}"),
            expected_kind: Some(ExpectedObjectKind::Blob),
            root_path: Some(path_str),
            wrap_with_tag: None,
        });
    }

    if let Some(cache_tree) = &idx.cache_tree {
        push_index_cache_tree_child_roots(&mut roots, cache_tree, "");
    }

    Ok(roots)
}

fn push_index_cache_tree_child_roots(
    roots: &mut Vec<RootObject>,
    node: &CacheTreeNode,
    parent_path: &str,
) {
    for child in &node.children {
        let name = String::from_utf8_lossy(&child.name);
        let path = if parent_path.is_empty() {
            name.into_owned()
        } else {
            format!("{parent_path}/{name}")
        };

        if child.entry_count >= 0 {
            if let Some(oid) = child.oid {
                roots.push(RootObject {
                    oid,
                    input: format!(":{path}"),
                    expected_kind: Some(ExpectedObjectKind::Tree),
                    root_path: Some(path.clone()),
                    wrap_with_tag: None,
                });
            }
        }

        push_index_cache_tree_child_roots(roots, child, &path);
    }
}

fn object_walk_print_commit_line(
    filter_provided_objects: bool,
    filter: Option<&ObjectFilter>,
    user_given_tip: bool,
) -> bool {
    if !filter_provided_objects {
        return user_given_tip || filter_shows_commit_line_when_not_user_given(filter);
    }
    match filter {
        Some(ObjectFilter::ObjectType(FilterObjectKind::Commit)) => {
            user_given_tip || filter_shows_commit_line_when_not_user_given(filter)
        }
        Some(ObjectFilter::ObjectType(_)) => false,
        Some(ObjectFilter::Combine(parts)) => {
            if parts
                .iter()
                .all(|p| matches!(p, ObjectFilter::ObjectType(FilterObjectKind::Commit)))
            {
                user_given_tip || filter_shows_commit_line_when_not_user_given(filter)
            } else {
                false
            }
        }
        _ => user_given_tip || filter_shows_commit_line_when_not_user_given(filter),
    }
}

/// Whether `LOFS_COMMIT` would receive `LOFR_DO_SHOW` when the commit is `NOT_USER_GIVEN` (Git list-objects-filter).
fn filter_shows_commit_line_when_not_user_given(filter: Option<&ObjectFilter>) -> bool {
    match filter {
        None => true,
        Some(ObjectFilter::BlobNone)
        | Some(ObjectFilter::BlobLimit(_))
        | Some(ObjectFilter::TreeDepth(_))
        | Some(ObjectFilter::SparseOid(_)) => true,
        Some(ObjectFilter::ObjectType(FilterObjectKind::Commit)) => true,
        Some(ObjectFilter::ObjectType(
            FilterObjectKind::Blob | FilterObjectKind::Tree | FilterObjectKind::Tag,
        )) => false,
        Some(ObjectFilter::Combine(parts)) => parts
            .iter()
            .all(|p| filter_shows_commit_line_when_not_user_given(Some(p))),
    }
}

fn skip_tree_descent_for_object_type_filter(filter: Option<&ObjectFilter>) -> bool {
    match filter {
        Some(ObjectFilter::ObjectType(FilterObjectKind::Commit | FilterObjectKind::Tag)) => true,
        Some(ObjectFilter::Combine(parts)) => parts
            .iter()
            .any(|p| skip_tree_descent_for_object_type_filter(Some(p))),
        _ => false,
    }
}

fn sparse_filter_includes_path(
    repo: &Repository,
    path: &str,
    sparse_lines: Option<&[String]>,
) -> bool {
    sparse_lines
        .map(|lines| path_in_sparse_checkout(path, lines, repo.work_tree.as_deref()))
        .unwrap_or(true)
}

fn sparse_oid_lines_from_filter(
    repo: &Repository,
    filter: Option<&ObjectFilter>,
) -> Result<Option<Vec<String>>> {
    let Some(f) = filter else {
        return Ok(None);
    };
    match f {
        ObjectFilter::SparseOid(spec) => {
            // Resolve `<rev>:<path>` (or a raw blob OID) to a blob, matching Git's
            // `repo_get_oid_with_flags(..., GET_OID_BLOB)`. A name that does not resolve to an
            // accessible blob is a hard error (`unable to access sparse blob in '<name>'`).
            let blob_oid = if let Ok(oid) = spec.parse::<ObjectId>() {
                oid
            } else if let Some((treeish, path)) = split_treeish_spec(spec) {
                let treeish_oid = resolve_revision_for_range_end(repo, treeish)
                    .map_err(|_| sparse_blob_access_error(spec))?;
                resolve_treeish_path(repo, treeish_oid, path)
                    .map_err(|_| sparse_blob_access_error(spec))?
            } else {
                // A bare name with no `:<path>` (e.g. `main`): Git resolves it to whatever object
                // the revision names (commit/tree/tag/blob), then tries to parse that object as a
                // sparse blob. A non-blob therefore fails parsing, not access (t5616 expects
                // "unable to parse sparse filter data in <oid>" for `sparse:oid=main`).
                resolve_revision_for_range_end(repo, spec)
                    .map_err(|_| sparse_blob_access_error(spec))?
            };
            let obj = repo
                .odb
                .read(&blob_oid)
                .map_err(|_| sparse_blob_access_error(spec))?;
            // A resolved object that is not a parseable sparse blob (e.g. a tree) fails parsing:
            // `unable to parse sparse filter data in <oid>`.
            if obj.kind != ObjectKind::Blob {
                return Err(RevListError::SparseFilterUnparsable {
                    object_id: blob_oid.to_hex(),
                }
                .into());
            }
            let text = std::str::from_utf8(&obj.data).map_err(|_| {
                Error::from(RevListError::SparseFilterUnparsable {
                    object_id: blob_oid.to_hex(),
                })
            })?;
            Ok(Some(parse_sparse_patterns_from_blob(text)))
        }
        ObjectFilter::Combine(parts) => {
            for p in parts {
                if let Some(lines) = sparse_oid_lines_from_filter(repo, Some(p))? {
                    return Ok(Some(lines));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Git's `unable to access sparse blob in '<name>'` error for an unresolvable `sparse:oid` spec.
fn sparse_blob_access_error(spec: &str) -> Error {
    RevListError::SparseBlobUnreadable {
        spec: spec.to_owned(),
    }
    .into()
}

fn packed_object_set(repo: &Repository) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    if let Ok(primary) = repo.odb.primary() {
        let _ = primary.packed_objects().for_each_object(&mut |oid| {
            out.insert(*oid);
            std::ops::ControlFlow::Continue(())
        });
    }
    out
}

fn resolve_specs(repo: &Repository, specs: &[String]) -> Result<Vec<ObjectId>> {
    resolve_specs_with_options(repo, specs, false)
}

fn resolve_specs_with_options(
    repo: &Repository,
    specs: &[String],
    ignore_missing: bool,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let oid = match resolve_revision_for_range_end(repo, spec) {
            Ok(oid) => oid,
            Err(Error::ObjectNotFound(_) | Error::InvalidRef(_)) if ignore_missing => continue,
            Err(err) => return Err(err),
        };
        match peel_to_commit(repo, oid) {
            Ok(commit_oid) => out.push(commit_oid),
            // `git rev-list <obj>` accepts a positive tip that is (or peels to) a non-commit
            // object such as a blob or a tag-of-blob: it simply contributes no commits and exits
            // 0 (verified against git: `git rev-list tag-of-blob` prints nothing, exit 0). Mirror
            // that here so callers that feed all freshly-fetched ref tips into a commit walk
            // (e.g. submodule-change detection over a remote carrying a tag-of-blob, t5516
            // "refuse fetch to current branch of worktree") do not abort.
            Err(Error::CorruptObject(_)) if object_peels_to_non_commit(repo, oid) => {}
            Err(Error::ObjectNotFound(_) | Error::InvalidRef(_)) if ignore_missing => {}
            Err(err) => return Err(err),
        }
    }
    Ok(out)
}

/// Resolve revision strings to commit tips and non-commit roots (tags, trees, blobs, `rev:path`).
///
/// Used by `test-tool path-walk` and similar object walks that mirror `git` revision parsing.
/// Resolve revision strings to commit OIDs (for negative specs / exclusions).
pub fn resolve_revision_commits(repo: &Repository, specs: &[String]) -> Result<Vec<ObjectId>> {
    resolve_specs(repo, specs)
}

/// Resolve revision argument strings to commit OIDs for history walks (`log`, `rev-list`).
///
/// Each spec is interpreted like Git's `handle_revision_arg` for a plain commit tip: ranges
/// (`A..B`), exclusions (`^rev`), and revision expressions are supported via
/// [`split_revision_token`] and [`resolve_revision_for_range_end`].
pub fn resolve_revision_specs_to_commits(
    repo: &Repository,
    specs: &[String],
) -> Result<Vec<ObjectId>> {
    resolve_specs(repo, specs)
}

pub fn resolve_object_walk_roots(
    repo: &Repository,
    specs: &[String],
) -> Result<(Vec<ObjectId>, Vec<ObjectWalkRoot>)> {
    let (commits, roots, _tip_annotated_tag_by_commit) = resolve_specs_for_objects(repo, specs)?;
    Ok((
        commits,
        roots
            .into_iter()
            .map(|r| ObjectWalkRoot {
                oid: r.oid,
                input: r.input,
                root_path: r.root_path,
            })
            .collect(),
    ))
}

fn resolve_specs_for_objects(
    repo: &Repository,
    specs: &[String],
) -> Result<ResolvedSpecsWithTagMap> {
    resolve_specs_for_objects_with_options(repo, specs, false, MissingAction::Error)
}

fn resolve_specs_for_objects_with_options(
    repo: &Repository,
    specs: &[String],
    ignore_missing: bool,
    missing_action: MissingAction,
) -> Result<ResolvedSpecsWithTagMap> {
    let mut commits = Vec::new();
    let mut roots = Vec::new();
    let mut tip_annotated_tag_by_commit: HashMap<ObjectId, ObjectId> = HashMap::new();

    for spec in specs {
        if let Ok(raw_oid) = spec.parse::<ObjectId>() {
            let raw_object = match repo.odb.read(&raw_oid) {
                Ok(obj) => obj,
                Err(Error::ObjectNotFound(_)) if ignore_missing => continue,
                Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                    roots.push(RootObject {
                        oid: raw_oid,
                        input: spec.clone(),
                        expected_kind: None,
                        root_path: None,
                        wrap_with_tag: None,
                    });
                    continue;
                }
                Err(err) => return Err(err),
            };
            match raw_object.kind {
                ObjectKind::Commit => {
                    commits.push(raw_oid);
                }
                ObjectKind::Tag => {
                    let tag = parse_tag(&raw_object.data)?;
                    let expected_kind = ExpectedObjectKind::from_tag_type(&tag.object_type)
                        .ok_or_else(|| {
                            Error::CorruptObject(format!(
                                "object {spec} has unsupported tag type '{}'",
                                tag.object_type
                            ))
                        })?;
                    if expected_kind == ExpectedObjectKind::Commit {
                        // A tag whose target peels to a commit becomes a normal commit walk tip
                        // (printed as a bare OID commit line) with the tag emitted alongside it via
                        // `tip_annotated_tag_by_commit`, matching how a tag named by ref is handled.
                        // Routing it through a commit `RootObject` instead would print the commit with
                        // an empty object name (`<oid> \0`) rather than as a commit line (t5305).
                        tip_annotated_tag_by_commit.insert(tag.object, raw_oid);
                        commits.push(tag.object);
                    } else {
                        roots.push(RootObject {
                            oid: tag.object,
                            input: spec.clone(),
                            expected_kind: Some(expected_kind),
                            root_path: None,
                            wrap_with_tag: Some(raw_oid),
                        });
                    }
                }
                ObjectKind::Tree | ObjectKind::Blob => roots.push(RootObject {
                    oid: raw_oid,
                    input: spec.clone(),
                    expected_kind: None,
                    root_path: None,
                    wrap_with_tag: None,
                }),
            }
            continue;
        }

        if let Some((treeish, path)) = split_treeish_spec(spec) {
            if !path.is_empty() {
                let treeish_oid = match resolve_revision_for_range_end(repo, treeish) {
                    Ok(oid) => oid,
                    Err(Error::ObjectNotFound(_) | Error::InvalidRef(_)) if ignore_missing => {
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                let object_oid = match resolve_treeish_path(repo, treeish_oid, path) {
                    Ok(oid) => oid,
                    Err(Error::ObjectNotFound(_) | Error::InvalidRef(_)) if ignore_missing => {
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                let expected_kind = match repo.odb.read_info(&object_oid) {
                    Ok(info) => ExpectedObjectKind::from_object_kind(info.kind),
                    Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => None,
                    Err(Error::ObjectNotFound(_)) if ignore_missing => continue,
                    Err(err) => return Err(err),
                };
                roots.push(RootObject {
                    oid: object_oid,
                    input: spec.clone(),
                    expected_kind,
                    root_path: Some(path.to_owned()),
                    wrap_with_tag: None,
                });
                continue;
            }
        }

        let oid = match resolve_revision_for_range_end(repo, spec) {
            Ok(oid) => oid,
            Err(Error::ObjectNotFound(_) | Error::InvalidRef(_)) if ignore_missing => continue,
            Err(err) => return Err(err),
        };
        if let Ok(obj) = repo.odb.read(&oid) {
            if obj.kind == ObjectKind::Tag {
                if let Ok(commit_oid) = peel_to_commit(repo, oid) {
                    tip_annotated_tag_by_commit.insert(commit_oid, oid);
                }
            }
        }
        match peel_to_commit(repo, oid) {
            Ok(commit_oid) => commits.push(commit_oid),
            Err(Error::CorruptObject(_)) if ignore_missing => {}
            Err(Error::ObjectNotFound(_)) if ignore_missing => {}
            Err(Error::CorruptObject(_)) | Err(Error::ObjectNotFound(_)) => {
                roots.push(RootObject {
                    oid,
                    input: spec.clone(),
                    expected_kind: None,
                    root_path: None,
                    wrap_with_tag: None,
                })
            }
            Err(err) => return Err(err),
        }
    }

    Ok((commits, roots, tip_annotated_tag_by_commit))
}

/// Returns `true` when `oid` is a readable object that peels (through any tag chain) to a blob or
/// tree rather than a commit. Used to mirror `git rev-list`'s tolerance of a non-commit positive
/// tip: such a tip is silently ignored, whereas a genuinely missing/corrupt object is an error.
fn object_peels_to_non_commit(repo: &Repository, mut oid: ObjectId) -> bool {
    loop {
        let Ok(object) = repo.odb.read(&oid) else {
            return false;
        };
        match object.kind {
            ObjectKind::Commit => return false,
            ObjectKind::Blob | ObjectKind::Tree => return true,
            ObjectKind::Tag => match parse_tag(&object.data) {
                Ok(tag) => oid = tag.object,
                Err(_) => return false,
            },
        }
    }
}

/// Peel an object (possibly a tag) to the underlying commit.
fn peel_to_commit(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let object = repo.odb.read(&oid)?;
        match object.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => {
                let tag = parse_tag(&object.data)?;
                oid = tag.object;
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "object {oid} is a {other:?}, not a commit"
                )));
            }
        }
    }
}

fn reflog_commit_tips(repo: &Repository) -> Result<Vec<ObjectId>> {
    let z = zero_oid();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for refname in list_reflog_refs(&repo.git_dir)? {
        let entries = read_reflog(&repo.git_dir, &refname)?;
        for e in entries {
            for oid in [e.old_oid, e.new_oid] {
                if oid == z {
                    continue;
                }
                match peel_to_commit(repo, oid) {
                    Ok(c) if seen.insert(c) => out.push(c),
                    Err(_) => {}
                    _ => {}
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn walk_closure(
    graph: &mut CommitGraph<'_>,
    starts: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let (seen, _) = walk_closure_ordered(graph, starts)?;
    Ok(seen)
}

/// Like [`walk_closure`] but follows only the first parent from each start (Git
/// `--exclude-first-parent-only` for excluded tips).
fn walk_closure_first_parent_only(
    graph: &mut CommitGraph<'_>,
    starts: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    for &start in starts {
        queue.push_back(start);
    }
    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let parents = graph.parents_of(oid)?;
        if let Some(&p) = parents.first() {
            queue.push_back(p);
        }
    }
    Ok(seen)
}

/// BFS walk that returns both the set and the discovery order.
pub(crate) fn walk_closure_ordered(
    graph: &mut CommitGraph<'_>,
    starts: &[ObjectId],
) -> Result<(HashSet<ObjectId>, Vec<ObjectId>)> {
    walk_closure_ordered_excluding(graph, starts, &HashSet::new())
}

fn walk_closure_ordered_excluding(
    graph: &mut CommitGraph<'_>,
    starts: &[ObjectId],
    excluded: &HashSet<ObjectId>,
) -> Result<(HashSet<ObjectId>, Vec<ObjectId>)> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    let mut queue = VecDeque::new();
    for &start in starts {
        queue.push_back(start);
    }
    while let Some(oid) = queue.pop_front() {
        if excluded.contains(&oid) {
            continue;
        }
        if !seen.insert(oid) {
            continue;
        }
        order.push(oid);
        for parent in graph.parents_of(oid)? {
            queue.push_back(parent);
        }
    }
    Ok((seen, order))
}

fn walk_closure_ordered_with_missing(
    graph: &mut CommitGraph<'_>,
    starts: &[ObjectId],
    excluded: &HashSet<ObjectId>,
    missing_action: MissingAction,
    missing: &mut Vec<ObjectId>,
    missing_seen: &mut HashSet<ObjectId>,
) -> Result<(HashSet<ObjectId>, Vec<ObjectId>)> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    let mut queue = VecDeque::new();
    for &start in starts {
        queue.push_back(start);
    }
    while let Some(oid) = queue.pop_front() {
        if excluded.contains(&oid) || seen.contains(&oid) {
            continue;
        }
        let parents = match graph.parents_of(oid) {
            Ok(parents) => parents,
            Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                if missing_action.reports_missing() && missing_seen.insert(oid) {
                    missing.push(oid);
                }
                continue;
            }
            Err(err) => return Err(err),
        };
        seen.insert(oid);
        order.push(oid);
        for parent in parents {
            queue.push_back(parent);
        }
    }
    Ok((seen, order))
}

/// Date-priority walk used when a parent-count filter (`--no-merges`, `--max-parents`, …) is
/// active. Mirrors Git's `limit_list`: a pure committer-date priority queue seeded from the user
/// tips that traverses the *full* reachable set (`traversable`), stepping through dropped commits
/// (e.g. merges) without emitting them, and emits only commits in `emit`. Unlike [`date_order_walk`]
/// this imposes no topological gating, so a tip that is also an ancestor (and has a later date than
/// its descendants) is emitted in date order — matching `git rev-list --all --no-merges` (t6009).
fn date_order_walk_through_dropped(
    graph: &mut CommitGraph<'_>,
    tips: &[ObjectId],
    traversable: &HashSet<ObjectId>,
    emit: &HashSet<ObjectId>,
    author_dates: bool,
) -> Result<Vec<ObjectId>> {
    let mut heap = BinaryHeap::new();
    let mut queued: HashSet<ObjectId> = HashSet::new();
    let mut next_seq: u64 = 0;
    // Seed every user tip that is part of the reachable set, in their given order: an earlier tip
    // gets a smaller seq so equal-date tips are emitted in tip order (Git's insertion-order tiebreak).
    for &tip in tips {
        if traversable.contains(&tip) && queued.insert(tip) {
            heap.push(CommitDateKey {
                oid: tip,
                date: graph.sort_key(tip, author_dates),
                seq: next_seq,
            });
            next_seq += 1;
        }
    }

    let mut emitted = HashSet::new();
    let mut out = Vec::with_capacity(emit.len());
    while let Some(item) = heap.pop() {
        if emit.contains(&item.oid) && emitted.insert(item.oid) {
            out.push(item.oid);
        }
        for parent in graph.parents_of(item.oid)? {
            if !traversable.contains(&parent) {
                continue;
            }
            if queued.insert(parent) {
                heap.push(CommitDateKey {
                    oid: parent,
                    date: graph.sort_key(parent, author_dates),
                    seq: next_seq,
                });
                next_seq += 1;
            }
        }
    }

    Ok(out)
}

/// Git-style default ordering: among commits ready to print, pick the one with the
/// greatest committer timestamp; a parent becomes ready only after all of its
/// children that remain in the walk have been emitted.
///
/// This matches `git rev-list` behavior (and differs from sorting the whole set by
/// date, which can surface ancestors before descendants when dates are skewed).
pub(crate) fn date_order_walk(
    graph: &mut CommitGraph<'_>,
    selected: &HashSet<ObjectId>,
    tip_order: &[ObjectId],
    author_dates: bool,
) -> Result<Vec<ObjectId>> {
    let mut unfinished_children: HashMap<ObjectId, usize> =
        selected.iter().map(|&oid| (oid, 0usize)).collect();
    for &child in selected {
        for parent in graph.parents_of(child)? {
            if selected.contains(&parent) {
                if let Some(count) = unfinished_children.get_mut(&parent) {
                    *count += 1;
                }
            }
        }
    }

    let mut heap = BinaryHeap::new();
    // Equal-date ties break by *insertion order* to match Git's `prio_queue` FIFO tiebreak
    // (`prio-queue.c`): the tip the user (or pseudo-ref expansion like `--reflog`) listed first pops
    // first. `tip_order` carries that user/insertion order; each seeded tip below takes its index as
    // its `seq`, and parents discovered during the walk take a strictly larger `seq`.
    let ordered_tip_count = tip_order.len() as u64;
    // Git `limit_list` seeds `revs->commits` — every *explicitly given* tip — into the priority
    // queue up front, then pops by date and inserts unseen parents. It does NOT defer a parent until
    // its child is shown, so when the user lists an ancestor and its descendant as equal-date tips,
    // the one listed first is emitted first (`rev-list A B` with A an ancestor of B, same date → A,
    // then B). Replicate that: seed explicit tips immediately. When there are no explicit tips
    // (path-walk, where `selected` is the whole matched set and seeding everything would surface
    // ancestors before descendants), fall back to seeding only in-degree-zero commits so descendants
    // still precede ancestors. Parents discovered during the walk get a strictly larger seq.
    let mut seeded: HashSet<ObjectId> = HashSet::new();
    if tip_order.is_empty() {
        for &oid in selected {
            if unfinished_children.get(&oid).copied().unwrap_or(0) == 0 {
                heap.push(CommitDateKey {
                    oid,
                    date: graph.sort_key(oid, author_dates),
                    seq: 0,
                });
                seeded.insert(oid);
            }
        }
    } else {
        for (i, &oid) in tip_order.iter().enumerate() {
            if selected.contains(&oid) && seeded.insert(oid) {
                heap.push(CommitDateKey {
                    oid,
                    date: graph.sort_key(oid, author_dates),
                    seq: i as u64,
                });
            }
        }
    }

    let mut next_seq: u64 = ordered_tip_count + 1;
    let mut emitted = HashSet::new();
    let mut out = Vec::with_capacity(selected.len());
    while let Some(item) = heap.pop() {
        if !emitted.insert(item.oid) {
            continue;
        }
        out.push(item.oid);
        for parent in graph.parents_of(item.oid)? {
            if !selected.contains(&parent) {
                continue;
            }
            let Some(count) = unfinished_children.get_mut(&parent) else {
                continue;
            };
            *count = count.saturating_sub(1);
            // Already-seeded commits (explicit tips) are in the heap; the pop-time `emitted` guard
            // dedups them. Only enqueue a parent once its last selected child has been processed and
            // it has not already been seeded as a tip.
            if *count == 0 && !seeded.contains(&parent) {
                heap.push(CommitDateKey {
                    oid: parent,
                    date: graph.sort_key(parent, author_dates),
                    seq: next_seq,
                });
                next_seq += 1;
            }
        }
    }

    Ok(out)
}

fn topo_sort(
    graph: &mut CommitGraph<'_>,
    selected: &HashSet<ObjectId>,
    author_dates: bool,
) -> Result<Vec<ObjectId>> {
    let mut child_count: HashMap<ObjectId, usize> = selected.iter().map(|&oid| (oid, 0)).collect();

    for &oid in selected {
        for parent in graph.parents_of(oid)? {
            if !selected.contains(&parent) {
                continue;
            }
            if let Some(count) = child_count.get_mut(&parent) {
                *count += 1;
            }
        }
    }

    // Git's `--topo-order`: among commits whose children have all been emitted, take the one
    // with the smallest committer date first (Kahn + min-heap). A max-heap on `CommitDateKey`
    // inverts this and breaks `rev-list --reverse --topo-order` vs upstream (t3425).
    let mut ready: BinaryHeap<Reverse<CommitDateKey>> = BinaryHeap::new();
    for (&oid, &count) in &child_count {
        if count == 0 {
            ready.push(Reverse(CommitDateKey {
                oid,
                date: graph.sort_key(oid, author_dates),
                seq: 0,
            }));
        }
    }

    let mut out = Vec::with_capacity(selected.len());
    while let Some(Reverse(item)) = ready.pop() {
        let oid = item.oid;
        out.push(oid);
        for parent in graph.parents_of(oid)? {
            if !selected.contains(&parent) {
                continue;
            }
            if let Some(count) = child_count.get_mut(&parent) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.push(Reverse(CommitDateKey {
                        oid: parent,
                        date: graph.sort_key(parent, author_dates),
                        seq: 0,
                    }));
                }
            }
        }
    }

    reorder_adjacent_merge_parent_blocks(graph, &mut out)?;
    Ok(out)
}

fn graph_order_topo_sort(
    graph: &mut CommitGraph<'_>,
    selected: &HashSet<ObjectId>,
    discovery_order: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let mut child_count: HashMap<ObjectId, usize> = selected.iter().map(|&oid| (oid, 0)).collect();

    for &oid in selected {
        for parent in graph.parents_of(oid)? {
            if !selected.contains(&parent) {
                continue;
            }
            if let Some(count) = child_count.get_mut(&parent) {
                *count += 1;
            }
        }
    }

    let discovery_rank: HashMap<ObjectId, usize> = discovery_order
        .iter()
        .copied()
        .enumerate()
        .map(|(rank, oid)| (oid, rank))
        .collect();
    let mut ready: Vec<ObjectId> = child_count
        .iter()
        .filter_map(|(&oid, &count)| (count == 0).then_some(oid))
        .collect();

    ready.sort_by(|&a, &b| {
        graph
            .committer_time(a)
            .cmp(&graph.committer_time(b))
            .then_with(|| {
                discovery_rank
                    .get(&b)
                    .copied()
                    .unwrap_or(usize::MAX)
                    .cmp(&discovery_rank.get(&a).copied().unwrap_or(usize::MAX))
            })
            .then_with(|| a.cmp(&b))
    });

    let mut out = Vec::with_capacity(selected.len());
    while let Some(oid) = ready.pop() {
        out.push(oid);
        for parent in graph.parents_of(oid)? {
            if !selected.contains(&parent) {
                continue;
            }
            if let Some(count) = child_count.get_mut(&parent) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.push(parent);
                }
            }
        }
    }

    reorder_adjacent_merge_parent_blocks(graph, &mut out)?;
    Ok(out)
}

fn reorder_adjacent_merge_parent_blocks(
    graph: &mut CommitGraph<'_>,
    ordered: &mut [ObjectId],
) -> Result<()> {
    for index in 0..ordered.len() {
        let parents = graph.parents_of(ordered[index])?;
        if parents.len() <= 1 || index + parents.len() >= ordered.len() {
            continue;
        }
        let parent_set: HashSet<ObjectId> = parents.iter().copied().collect();
        let end = index + 1 + parents.len();
        if ordered[index + 1..end]
            .iter()
            .all(|oid| parent_set.contains(oid))
        {
            let rank: HashMap<ObjectId, usize> = parents
                .iter()
                .rev()
                .copied()
                .enumerate()
                .map(|(rank, oid)| (oid, rank))
                .collect();
            ordered[index + 1..end].sort_by_key(|oid| rank.get(oid).copied().unwrap_or(usize::MAX));
        }
    }
    Ok(())
}

fn reorder_symmetric_topo_right_first(
    graph: &mut CommitGraph<'_>,
    ordered: &mut Vec<ObjectId>,
    left_oid: ObjectId,
    right_oid: ObjectId,
) -> Result<()> {
    if ordered.len() < 2 {
        return Ok(());
    }

    let left_reach = walk_closure(graph, &[left_oid])?;
    let right_reach = walk_closure(graph, &[right_oid])?;
    let mut right_only = Vec::new();
    let mut rest = Vec::new();

    for oid in ordered.drain(..) {
        if right_reach.contains(&oid) && !left_reach.contains(&oid) {
            right_only.push(oid);
        } else {
            rest.push(oid);
        }
    }

    right_only.extend(rest);
    *ordered = right_only;
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn simplify_merges_commit_list(
    repo: &Repository,
    commits: &[ObjectId],
    paths: &[String],
    excluded: &HashSet<ObjectId>,
    ancestry_path_bottoms: &[ObjectId],
    ordering: OrderingMode,
    show_pulls: bool,
    preserve_graph_merges: bool,
) -> Result<Vec<ObjectId>> {
    let selected: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut out = Vec::new();
    let mut simplified_parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for oid in commits {
        let raw_parents = load_commit(repo, *oid)?.parents;
        let rewritten = visible_parents_for_graph_lib(repo, *oid, &selected, false)?;
        let path_survives =
            path_merge_survives_simplify(repo, *oid, paths, excluded, ancestry_path_bottoms)?;
        let pull_merge = show_pulls
            && raw_parents.len() > 1
            && path_merge_survives_simplify(repo, *oid, paths, excluded, &[])?;
        if raw_parents.len() > 1 && rewritten.len() <= 1 {
            if rewritten.is_empty() || pull_merge || path_survives {
                simplified_parents.insert(*oid, rewritten);
                out.push(*oid);
            }
            continue;
        }
        if rewritten.len() <= 1 {
            simplified_parents.insert(*oid, rewritten);
            out.push(*oid);
            continue;
        }
        let mut simplified = if preserve_graph_merges {
            let treesame_rewritten = path_treesame_parents(repo, *oid, paths, &rewritten)?;
            let all_rewritten_treesame =
                !rewritten.is_empty() && treesame_rewritten.len() == rewritten.len();
            if all_rewritten_treesame {
                vec![rewritten[0]]
            } else {
                graph_simplify_parent_list_lib(repo, &selected, &rewritten, &treesame_rewritten)?
            }
        } else {
            graph_simplify_parent_list_lib(repo, &selected, &rewritten, &HashSet::new())?
        };
        simplified.sort_unstable();
        simplified.dedup();
        if simplified.len() > 1 || pull_merge {
            simplified_parents.insert(*oid, simplified);
            out.push(*oid);
        }
    }
    if ordering == OrderingMode::AuthorDateTopo {
        return simplify_merges_author_date_order(repo, &out, &simplified_parents);
    }
    Ok(out)
}

fn simplify_merges_author_date_order(
    repo: &Repository,
    commits: &[ObjectId],
    simplified_parents: &HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    let selected: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut child_count: HashMap<ObjectId, usize> =
        commits.iter().copied().map(|oid| (oid, 0)).collect();
    for parents in simplified_parents.values() {
        for parent in parents {
            if selected.contains(parent) {
                if let Some(count) = child_count.get_mut(parent) {
                    *count += 1;
                }
            }
        }
    }

    let mut graph = CommitGraph::new(repo, false);
    let mut ready = BinaryHeap::new();
    for oid in commits {
        if child_count.get(oid).copied().unwrap_or_default() == 0 {
            ready.push(CommitDateKey {
                oid: *oid,
                date: graph.sort_key(*oid, true),
                seq: 0,
            });
        }
    }

    let mut out = Vec::with_capacity(commits.len());
    let mut emitted = HashSet::new();
    while let Some(item) = ready.pop() {
        if !emitted.insert(item.oid) {
            continue;
        }
        out.push(item.oid);
        if let Some(parents) = simplified_parents.get(&item.oid) {
            for parent in parents {
                if !selected.contains(parent) {
                    continue;
                }
                let Some(count) = child_count.get_mut(parent) else {
                    continue;
                };
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.push(CommitDateKey {
                        oid: *parent,
                        date: graph.sort_key(*parent, true),
                        seq: 0,
                    });
                }
            }
        }
    }
    Ok(out)
}

fn path_merge_survives_simplify(
    repo: &Repository,
    oid: ObjectId,
    paths: &[String],
    excluded: &HashSet<ObjectId>,
    ancestry_path_bottoms: &[ObjectId],
) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let commit = load_commit(repo, oid)?;
    if commit.parents.len() <= 1 {
        return Ok(false);
    }
    let commit_map: HashMap<String, (ObjectId, u32)> =
        flatten_tree_with_mode(repo, commit.tree, "")?
            .into_iter()
            .collect();
    let mut treesame_parents = 0usize;
    let mut first_parent_differs = false;
    let mut differing_parent_is_excluded = false;
    let mut differing_parent_oids = Vec::new();
    for (nth, parent_oid) in commit.parents.iter().enumerate() {
        let parent = load_commit(repo, *parent_oid)?;
        let parent_map: HashMap<String, (ObjectId, u32)> =
            flatten_tree_with_mode(repo, parent.tree, "")?
                .into_iter()
                .collect();
        let differs = path_differs_for_specs(&commit_map, &parent_map, paths);
        if differs {
            differing_parent_oids.push(*parent_oid);
            if nth == 0 {
                first_parent_differs = true;
            }
            if excluded.contains(parent_oid) {
                differing_parent_is_excluded = true;
            }
        } else {
            treesame_parents += 1;
        }
    }
    if treesame_parents != 1 {
        return Ok(false);
    }
    if first_parent_differs || differing_parent_is_excluded {
        return Ok(true);
    }
    if !ancestry_path_bottoms.is_empty() {
        let mut graph = CommitGraph::new(repo, false);
        return treesame_parent_is_on_ancestry_bottom_side(
            &mut graph,
            &differing_parent_oids,
            ancestry_path_bottoms,
        );
    }
    Ok(false)
}

fn graph_simplify_parent_list_lib(
    repo: &Repository,
    selected: &HashSet<ObjectId>,
    parents: &[ObjectId],
    protected_treesame: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    let protected_count = parents
        .iter()
        .filter(|parent| protected_treesame.contains(parent))
        .count();
    for parent in parents {
        if parent_reachable_via_others_lib(repo, selected, *parent, parents)? {
            if protected_count == 1 && protected_treesame.contains(parent) {
                out.push(*parent);
            }
            continue;
        }
        out.push(*parent);
    }
    Ok(out)
}

fn path_treesame_parents(
    repo: &Repository,
    oid: ObjectId,
    paths: &[String],
    parents: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let mut treesame = HashSet::new();
    if paths.is_empty() || parents.is_empty() {
        return Ok(treesame);
    }

    let commit = load_commit(repo, oid)?;
    let commit_map: HashMap<String, (ObjectId, u32)> =
        flatten_tree_with_mode(repo, commit.tree, "")?
            .into_iter()
            .collect();
    for parent_oid in parents {
        let parent = load_commit(repo, *parent_oid)?;
        let parent_map: HashMap<String, (ObjectId, u32)> =
            flatten_tree_with_mode(repo, parent.tree, "")?
                .into_iter()
                .collect();
        if !path_differs_for_specs(&commit_map, &parent_map, paths) {
            treesame.insert(*parent_oid);
        }
    }
    Ok(treesame)
}

fn parent_reachable_via_others_lib(
    repo: &Repository,
    selected: &HashSet<ObjectId>,
    target: ObjectId,
    parents: &[ObjectId],
) -> Result<bool> {
    for parent in parents {
        if *parent == target {
            continue;
        }
        if graph_reaches_lib(repo, selected, *parent, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn graph_reaches_lib(
    repo: &Repository,
    selected: &HashSet<ObjectId>,
    start: ObjectId,
    target: ObjectId,
) -> Result<bool> {
    let mut stack = vec![start];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if oid == target {
            return Ok(true);
        }
        let mut parents = load_commit(repo, oid)?.parents;
        parents.retain(|p| selected.contains(p));
        stack.extend(parents);
    }
    Ok(false)
}

fn load_raw_parents_lib(repo: &Repository, oid: ObjectId) -> Result<Vec<ObjectId>> {
    Ok(load_commit(repo, oid)?.parents)
}

fn load_navigation_parents_lib(
    repo: &Repository,
    oid: ObjectId,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    if let Some(grafted) = grafts.get(&oid) {
        return Ok(grafted.clone());
    }
    load_raw_parents_lib(repo, oid)
}

fn first_parent_of_commit_lib(
    repo: &Repository,
    oid: ObjectId,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Option<ObjectId>> {
    let parents = load_navigation_parents_lib(repo, oid, grafts)?;
    Ok(parents.first().copied())
}

fn first_parent_anchor_in_set_lib(
    repo: &Repository,
    start: ObjectId,
    anchors: &HashSet<ObjectId>,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Option<ObjectId>> {
    let mut seen = HashSet::new();
    let mut cursor = Some(start);
    while let Some(oid) = cursor {
        if !seen.insert(oid) {
            break;
        }
        if anchors.contains(&oid) {
            return Ok(Some(oid));
        }
        cursor = first_parent_of_commit_lib(repo, oid, grafts)?;
    }
    Ok(None)
}

#[expect(clippy::only_used_in_recursion)]
fn collect_visible_parent_for_graph_lib(
    repo: &Repository,
    candidate: ObjectId,
    included: &HashSet<ObjectId>,
    first_parent_only: bool,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen.insert(candidate) {
        return Ok(());
    }
    if included.contains(&candidate) {
        out.push(candidate);
        return Ok(());
    }
    let mut parents = load_navigation_parents_lib(repo, candidate, grafts)?;
    if parents.is_empty() {
        return Ok(());
    }
    if parents.len() > 1 {
        parents.truncate(1);
    }
    for parent in parents {
        collect_visible_parent_for_graph_lib(
            repo,
            parent,
            included,
            first_parent_only,
            grafts,
            seen,
            out,
        )?;
    }
    Ok(())
}

fn visible_parents_for_graph_lib(
    repo: &Repository,
    oid: ObjectId,
    included: &HashSet<ObjectId>,
    first_parent_only: bool,
) -> Result<Vec<ObjectId>> {
    let grafts = crate::rev_parse::load_graft_parents(&repo.git_dir);
    let mut direct = load_navigation_parents_lib(repo, oid, &grafts)?;
    if first_parent_only && direct.len() > 1 {
        direct.truncate(1);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for parent in direct {
        collect_visible_parent_for_graph_lib(
            repo,
            parent,
            included,
            first_parent_only,
            &grafts,
            &mut seen,
            &mut out,
        )?;
    }
    let mut dedup = HashSet::new();
    out.retain(|parent| dedup.insert(*parent));
    Ok(out)
}

fn reorder_path_limited_graph_commits(
    repo: &Repository,
    commits: &[ObjectId],
    first_parent_only: bool,
) -> Result<Vec<ObjectId>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }

    // For a default (non `--first-parent`) dense path-limited walk Git emits commits in
    // commit-date priority order: when it pops a merge it inserts *all* of its parents into
    // the date-sorted pending list, so a newer side branch is shown before an older
    // first-parent ancestor. The caller already produced this order via `date_order_walk`,
    // so reshuffling by first-parent topology here would be wrong (it would sink a newer
    // side commit below the older first-parent spine — t4013 `log --patch-with-stat main --
    // dir/`). Only the `--first-parent` walk wants the first-parent spine grouping below.
    if !first_parent_only {
        return Ok(commits.to_vec());
    }

    let included: HashSet<ObjectId> = commits.iter().copied().collect();
    let grafts = crate::rev_parse::load_graft_parents(&repo.git_dir);
    let mut chain = Vec::new();
    let mut chain_seen = HashSet::new();
    let mut cursor = Some(commits[0]);
    while let Some(oid) = cursor {
        if !included.contains(&oid) || !chain_seen.insert(oid) {
            break;
        }
        chain.push(oid);
        let visible = visible_parents_for_graph_lib(repo, oid, &included, first_parent_only)?;
        cursor = visible.first().copied();
    }

    let chain_set: HashSet<ObjectId> = chain.iter().copied().collect();
    let mut grouped: HashMap<Option<ObjectId>, Vec<ObjectId>> = HashMap::new();
    for oid in commits {
        if chain_set.contains(oid) {
            continue;
        }
        let anchor = first_parent_anchor_in_set_lib(repo, *oid, &chain_set, &grafts)?;
        grouped.entry(anchor).or_default().push(*oid);
    }

    let mut ordered = Vec::new();
    for chain_oid in chain {
        if let Some(group) = grouped.remove(&Some(chain_oid)) {
            ordered.extend(group);
        }
        ordered.push(chain_oid);
    }
    if let Some(group) = grouped.remove(&None) {
        ordered.extend(group);
    }
    for (_anchor, group) in grouped {
        ordered.extend(group);
    }
    Ok(ordered)
}

fn expand_sparse_path_limited_graph_history(
    repo: &Repository,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }

    let mut expanded = Vec::new();
    let mut seen = HashSet::new();
    let grafts = crate::rev_parse::load_graft_parents(&repo.git_dir);
    let mut push_unique = |oid: ObjectId, out: &mut Vec<ObjectId>| {
        if seen.insert(oid) {
            out.push(oid);
        }
    };

    for window in commits.windows(2) {
        let from = window[0];
        let to = window[1];
        push_unique(from, &mut expanded);

        let mut cursor = first_parent_of_commit_lib(repo, from, &grafts)?;
        let mut chain = Vec::new();
        let mut found_target = false;
        let mut local_seen = HashSet::new();
        while let Some(oid) = cursor {
            if !local_seen.insert(oid) {
                break;
            }
            if oid == to {
                found_target = true;
                break;
            }
            chain.push(oid);
            cursor = first_parent_of_commit_lib(repo, oid, &grafts)?;
        }
        if found_target {
            for oid in chain {
                push_unique(oid, &mut expanded);
            }
        }
    }

    if let Some(&last) = commits.last() {
        push_unique(last, &mut expanded);
        let mut cursor = first_parent_of_commit_lib(repo, last, &grafts)?;
        let mut tail_seen = HashSet::new();
        while let Some(oid) = cursor {
            if !tail_seen.insert(oid) {
                break;
            }
            push_unique(oid, &mut expanded);
            cursor = first_parent_of_commit_lib(repo, oid, &grafts)?;
        }
    }

    Ok(expanded)
}

fn limit_to_ancestry(
    graph: &mut CommitGraph<'_>,
    selected: &mut HashSet<ObjectId>,
    bottoms: &[ObjectId],
) -> Result<()> {
    let mut keep = HashSet::new();

    for &bottom in bottoms {
        let ancestors = walk_closure(graph, &[bottom])?;
        keep.extend(
            ancestors
                .iter()
                .copied()
                .filter(|oid| selected.contains(oid)),
        );
    }

    let mut descendants: HashSet<ObjectId> = bottoms.iter().copied().collect();
    loop {
        let mut made_progress = false;
        for &candidate in selected.iter() {
            if descendants.contains(&candidate) {
                continue;
            }

            let parents = graph.parents_of(candidate)?;
            if parents.iter().any(|parent| descendants.contains(parent)) {
                descendants.insert(candidate);
                made_progress = true;
            }
        }

        if !made_progress {
            break;
        }
    }

    keep.extend(
        descendants
            .iter()
            .copied()
            .filter(|oid| selected.contains(oid)),
    );
    selected.retain(|oid| keep.contains(oid));
    Ok(())
}

/// Check if a commit modifies any of the given paths compared to its parents.
///
/// `simplify_merges` / `show_pulls` mirror Git's path-limited `try_to_simplify_commit` behavior
/// for merge commits when `--full-history` is active (including the implicit full history from
/// `--simplify-merges` or `--ancestry-path`).
#[allow(clippy::too_many_arguments)]
fn commit_touches_paths(
    repo: &Repository,
    graph: &mut CommitGraph<'_>,
    oid: ObjectId,
    paths: &[String],
    full_history: bool,
    sparse: bool,
    simplify_merges: bool,
    preserve_simplify_merges_graph_merges: bool,
    show_pulls: bool,
    parent_rewrite: bool,
    ancestry_path: bool,
    ancestry_path_bottoms: &[ObjectId],
    excluded: &HashSet<ObjectId>,
    bloom_chain: Option<&CommitGraphChain>,
    read_changed_paths: bool,
    changed_paths_version: i32,
    bloom_stats: Option<&BloomWalkStatsHandle>,
    bloom_cwd: Option<&str>,
) -> Result<bool> {
    let commit = load_commit(repo, oid)?;
    let parents = graph.parents_of(oid)?;
    let commit_entries = flatten_tree_with_mode(repo, commit.tree, "")?;
    let commit_map: HashMap<String, (ObjectId, u32)> = commit_entries.into_iter().collect();

    // Root commit: include only when any requested pathspec exists.
    if parents.is_empty() {
        if sparse {
            return Ok(true);
        }
        let precompose = repo.pathspec_precompose_enabled();
        let ctx = crate::pathspec::PathspecMatchContext {
            is_directory: false,
            is_git_submodule: false,
            precompose_paths: precompose,
        };
        return Ok(commit_map
            .keys()
            .any(|path| crate::pathspec::matches_pathspec_list_with_context(path, paths, ctx)));
    }

    // Single-parent commit: include only when requested paths changed.
    if parents.len() == 1 {
        let mut bloom_ret = BloomPrecheck::Inapplicable;
        if let Some(chain) = bloom_chain {
            bloom_ret = chain.bloom_precheck_for_paths(
                &repo.odb,
                oid,
                paths,
                bloom_cwd,
                changed_paths_version,
                read_changed_paths,
            )?;
            if let Some(stats) = bloom_stats {
                if let Ok(mut g) = stats.lock() {
                    g.record_precheck(bloom_ret);
                }
            }
            if bloom_ret == BloomPrecheck::DefinitelyNot {
                return Ok(false);
            }
        }

        let parent = load_commit(repo, parents[0])?;
        let parent_map: HashMap<String, (ObjectId, u32)> =
            flatten_tree_with_mode(repo, parent.tree, "")?
                .into_iter()
                .collect();
        let differs = path_differs_for_specs(&commit_map, &parent_map, paths);
        if bloom_ret == BloomPrecheck::Maybe && !differs {
            if let Some(stats) = bloom_stats {
                if let Ok(mut g) = stats.lock() {
                    g.record_false_positive();
                }
            }
        }
        if differs {
            return Ok(true);
        }
        if sparse {
            return Ok(true);
        }
        return Ok(false);
    }

    // Merge commit: dense history omits the merge when exactly one parent is TREESAME.
    let mut treesame_parents = 0usize;
    let mut treesame_parent_oids = Vec::new();
    let mut differing_parent_oids = Vec::new();
    let mut differs_any = false;
    let mut first_parent_differs = false;
    for (nth, parent_oid) in parents.iter().enumerate() {
        // Git consults the changed-path Bloom filter only when comparing against the
        // first parent (`nth_parent == 0` in `rev_compare_tree`). This applies to merge
        // commits too: a `DefinitelyNot` result short-circuits the first-parent tree diff
        // and is recorded in the Bloom statistics counters just like for single-parent
        // commits.
        let mut bloom_ret = BloomPrecheck::Inapplicable;
        if nth == 0 {
            if let Some(chain) = bloom_chain {
                bloom_ret = chain.bloom_precheck_for_paths(
                    &repo.odb,
                    oid,
                    paths,
                    bloom_cwd,
                    changed_paths_version,
                    read_changed_paths,
                )?;
                if let Some(stats) = bloom_stats {
                    if let Ok(mut g) = stats.lock() {
                        g.record_precheck(bloom_ret);
                    }
                }
            }
        }

        let differs = if bloom_ret == BloomPrecheck::DefinitelyNot {
            false
        } else {
            let parent = load_commit(repo, *parent_oid)?;
            let parent_map: HashMap<String, (ObjectId, u32)> =
                flatten_tree_with_mode(repo, parent.tree, "")?
                    .into_iter()
                    .collect();
            path_differs_for_specs(&commit_map, &parent_map, paths)
        };
        if nth == 0 {
            first_parent_differs = differs;
            if bloom_ret == BloomPrecheck::Maybe && !differs {
                if let Some(stats) = bloom_stats {
                    if let Ok(mut g) = stats.lock() {
                        g.record_false_positive();
                    }
                }
            }
        }
        if differs {
            differs_any = true;
            differing_parent_oids.push(*parent_oid);
        } else {
            treesame_parents += 1;
            treesame_parent_oids.push(*parent_oid);
        }
    }

    if ancestry_path
        && !ancestry_path_bottoms.is_empty()
        && if parent_rewrite {
            treesame_parents != parents.len()
                && treesame_parent_is_ancestry_bottom_or_ancestor(
                    graph,
                    &treesame_parent_oids,
                    ancestry_path_bottoms,
                )?
        } else {
            treesame_parent_is_on_ancestry_bottom_side(
                graph,
                &treesame_parent_oids,
                ancestry_path_bottoms,
            )?
        }
    {
        return Ok(sparse);
    }

    // `--full-history` without parent rewriting still omits merges that are TREESAME to every
    // parent. With parent rewriting (`--parents`, `--children`, or `%P`/`%p`) Git keeps those
    // merges so their rewritten edges can explain the selected history.
    if full_history && !simplify_merges && parents.len() > 1 && treesame_parents == parents.len() {
        return Ok(parent_rewrite || sparse);
    }

    if show_pulls && first_parent_differs && treesame_parents > 0 {
        return Ok(true);
    }

    if !full_history && treesame_parents == 1 {
        return Ok(false);
    }

    if full_history && simplify_merges {
        if preserve_simplify_merges_graph_merges {
            return Ok(true);
        }
        if treesame_parents == parents.len() {
            return Ok(true);
        }
        if treesame_parents > 0 && !differs_any {
            return Ok(true);
        }
        if show_pulls && first_parent_differs && treesame_parents > 0 {
            return Ok(true);
        }
        if treesame_parents == 1 {
            if ancestry_path
                && !ancestry_path_bottoms.is_empty()
                && treesame_parent_is_on_ancestry_bottom_side(
                    graph,
                    &differing_parent_oids,
                    ancestry_path_bottoms,
                )?
            {
                return Ok(true);
            }
            return Ok(first_parent_differs
                || differing_parent_oids
                    .iter()
                    .any(|parent| excluded.contains(parent)));
        }
        return Ok(differs_any || sparse);
    }

    if differs_any {
        return Ok(true);
    }

    Ok(sparse)
}

fn treesame_parent_is_on_ancestry_bottom_side(
    graph: &mut CommitGraph<'_>,
    treesame_parents: &[ObjectId],
    bottoms: &[ObjectId],
) -> Result<bool> {
    if treesame_parents.is_empty() {
        return Ok(false);
    }
    let treesame: HashSet<ObjectId> = treesame_parents.iter().copied().collect();
    for bottom in bottoms {
        if treesame.contains(bottom) {
            return Ok(true);
        }
        for parent in treesame_parents {
            let parent_closure = walk_closure(graph, &[*parent])?;
            if parent_closure.contains(bottom) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn treesame_parent_is_ancestry_bottom_or_ancestor(
    graph: &mut CommitGraph<'_>,
    treesame_parents: &[ObjectId],
    bottoms: &[ObjectId],
) -> Result<bool> {
    if treesame_parents.is_empty() {
        return Ok(false);
    }
    let treesame: HashSet<ObjectId> = treesame_parents.iter().copied().collect();
    for bottom in bottoms {
        if treesame.contains(bottom) {
            return Ok(true);
        }
        let bottom_closure = walk_closure(graph, &[*bottom])?;
        if bottom_closure.iter().any(|oid| treesame.contains(oid)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Bloom-filter context threaded through the dense path-limited walk so that the
/// changed-path Bloom precheck (Git's `rev_compare_tree` first-parent shortcut) and its
/// `GIT_TRACE2_PERF` statistics are recorded during the single real walk, exactly as Git
/// does — rather than during a later re-check of an already-reduced set.
#[derive(Clone, Copy)]
struct DenseBloomCtx<'a> {
    chain: Option<&'a CommitGraphChain>,
    read_changed_paths: bool,
    changed_paths_version: i32,
    stats: Option<&'a BloomWalkStatsHandle>,
    cwd: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn walk_dense_path_limited_closure(
    repo: &Repository,
    graph: &mut CommitGraph<'_>,
    tips: &[ObjectId],
    excluded: &HashSet<ObjectId>,
    paths: &[String],
    sparse: bool,
    show_pulls: bool,
) -> Result<HashSet<ObjectId>> {
    let mut walked = HashSet::new();
    let mut selected = HashSet::new();
    let mut queue: VecDeque<ObjectId> = tips.iter().copied().collect();

    while let Some(oid) = queue.pop_front() {
        if excluded.contains(&oid) || !walked.insert(oid) {
            continue;
        }

        let action = dense_path_limited_action(repo, graph, oid, paths, sparse, show_pulls)?;
        if action.visible {
            selected.insert(oid);
        }

        for parent in action.parents_to_walk {
            if !excluded.contains(&parent) && !walked.contains(&parent) {
                queue.push_back(parent);
            }
        }
    }

    Ok(selected)
}

/// Consult the changed-path Bloom filter over the full reachable set, matching Git's walk.
///
/// Git consults the Bloom filter for the first-parent comparison of *every* commit it walks in
/// `limit_list` via `rev_compare_tree`/`check_maybe_different_in_bloom_filter` (even commits later
/// pruned by TREESAME merge simplification). This happens unconditionally — not just when
/// `GIT_TRACE2_PERF` statistics are being collected — so loading the filter for each commit is also
/// what surfaces the reader's integrity warnings (out-of-range / decreasing index offsets) for a
/// corrupt commit-graph. grit's dense path-limited selection walk prunes the traversal (it follows
/// only the TREESAME parent of a merge) and then filters an already-reduced set, so on its own it
/// would consult far fewer filters than Git and miss both the statistics and these warnings.
///
/// This pass therefore runs over `reachable` — the full reachable set before dense limiting — and
/// looks up each commit's first-parent Bloom filter exactly once, mirroring Git's single
/// `limit_list` pass. When a `BloomWalkStats` handle is present it records the
/// `filter_not_present` / `maybe` / `definitely_not` / `false_positive` counters; the pass runs even
/// without one so corrupt-graph warnings still fire. It never changes which commits are selected for
/// output (a `DefinitelyNot` only confirms what the tree diff would also conclude).
fn consult_dense_bloom_filters(
    repo: &Repository,
    graph: &mut CommitGraph<'_>,
    reachable: &HashSet<ObjectId>,
    paths: &[String],
    bloom: &DenseBloomCtx<'_>,
) -> Result<()> {
    let Some(chain) = bloom.chain else {
        return Ok(());
    };
    for oid in reachable {
        let parents = graph.parents_of(*oid)?;
        // Bloom is consulted only against the first parent (Git `rev_compare_tree`,
        // `nth_parent == 0`); root commits have no first-parent comparison.
        let Some(first_parent) = parents.first().copied() else {
            continue;
        };
        let pre = chain.bloom_precheck_for_paths(
            &repo.odb,
            *oid,
            paths,
            bloom.cwd,
            bloom.changed_paths_version,
            bloom.read_changed_paths,
        )?;
        if let Some(stats) = bloom.stats {
            if let Ok(mut g) = stats.lock() {
                g.record_precheck(pre);
            }
        }
        // A `Maybe` result that turns out not to touch the path (tree-same against the first
        // parent) is a Bloom false positive in Git's accounting. Only computed when stats are
        // being collected, since it requires an extra tree diff that is otherwise unnecessary.
        if pre == BloomPrecheck::Maybe {
            if let Some(stats) = bloom.stats {
                let commit = load_commit(repo, *oid)?;
                let commit_map: HashMap<String, (ObjectId, u32)> =
                    flatten_tree_with_mode(repo, commit.tree, "")?
                        .into_iter()
                        .collect();
                let parent = load_commit(repo, first_parent)?;
                let parent_map: HashMap<String, (ObjectId, u32)> =
                    flatten_tree_with_mode(repo, parent.tree, "")?
                        .into_iter()
                        .collect();
                if !path_differs_for_specs(&commit_map, &parent_map, paths) {
                    if let Ok(mut g) = stats.lock() {
                        g.record_false_positive();
                    }
                }
            }
        }
    }
    Ok(())
}

struct DensePathAction {
    visible: bool,
    parents_to_walk: Vec<ObjectId>,
}

fn dense_path_limited_action(
    repo: &Repository,
    graph: &mut CommitGraph<'_>,
    oid: ObjectId,
    paths: &[String],
    sparse: bool,
    show_pulls: bool,
) -> Result<DensePathAction> {
    let commit = load_commit(repo, oid)?;
    let parents = graph.parents_of(oid)?;
    let commit_map: HashMap<String, (ObjectId, u32)> =
        flatten_tree_with_mode(repo, commit.tree, "")?
            .into_iter()
            .collect();

    if parents.is_empty() {
        let precompose = repo.pathspec_precompose_enabled();
        let ctx = crate::pathspec::PathspecMatchContext {
            is_directory: false,
            is_git_submodule: false,
            precompose_paths: precompose,
        };
        let visible = sparse
            || commit_map
                .keys()
                .any(|path| crate::pathspec::matches_pathspec_list_with_context(path, paths, ctx));
        return Ok(DensePathAction {
            visible,
            parents_to_walk: Vec::new(),
        });
    }

    let mut treesame = Vec::new();
    let mut differs_any = false;
    let mut first_parent_differs = false;
    for (nth, parent_oid) in parents.iter().enumerate() {
        let parent = load_commit(repo, *parent_oid)?;
        let parent_map: HashMap<String, (ObjectId, u32)> =
            flatten_tree_with_mode(repo, parent.tree, "")?
                .into_iter()
                .collect();
        if path_differs_for_specs(&commit_map, &parent_map, paths) {
            if nth == 0 {
                first_parent_differs = true;
            }
            differs_any = true;
        } else {
            treesame.push(*parent_oid);
        }
    }

    let parents_to_walk = if parents.len() > 1 {
        match treesame.first().copied() {
            Some(parent) => vec![parent],
            None => parents.clone(),
        }
    } else {
        parents.clone()
    };

    let visible = if sparse {
        true
    } else if parents.len() > 1 {
        treesame.is_empty() || (show_pulls && first_parent_differs && !treesame.is_empty())
    } else {
        differs_any
    };

    Ok(DensePathAction {
        visible,
        parents_to_walk,
    })
}

/// Whether `oid` would be included in a dense path-limited history walk for `paths`.
///
/// Matches the non-Bloom parts of the internal `commit_touches_paths` helper with `full_history = false` and
/// `sparse = false`: single-parent commits require a tree change on `paths` vs their parent;
/// merge commits are omitted when exactly one parent is tree-same on `paths` (Git `TREESAME`
/// simplification). Used by `log -g -- <path>` to align with Git's reflog path filtering.
pub fn commit_visible_for_dense_pathspecs(
    repo: &Repository,
    oid: ObjectId,
    paths: &[String],
) -> Result<bool> {
    if paths.is_empty() {
        return Ok(true);
    }
    let commit = load_commit(repo, oid)?;
    let parents = commit.parents.clone();
    let commit_entries = flatten_tree_with_mode(repo, commit.tree, "")?;
    let commit_map: HashMap<String, (ObjectId, u32)> = commit_entries.into_iter().collect();

    if parents.is_empty() {
        let precompose = repo.pathspec_precompose_enabled();
        return Ok(commit_map.keys().any(|path| {
            paths.iter().any(|spec| {
                crate::pathspec::matches_pathspec_with_context(
                    spec,
                    path,
                    crate::pathspec::PathspecMatchContext {
                        is_directory: false,
                        is_git_submodule: false,
                        precompose_paths: precompose,
                    },
                )
            })
        }));
    }

    if parents.len() == 1 {
        let parent = load_commit(repo, parents[0])?;
        let parent_map: HashMap<String, (ObjectId, u32)> =
            flatten_tree_with_mode(repo, parent.tree, "")?
                .into_iter()
                .collect();
        return Ok(path_differs_for_specs(&commit_map, &parent_map, paths));
    }

    let mut treesame_parents = 0usize;
    let mut differs_any = false;
    for parent_oid in &parents {
        let parent = load_commit(repo, *parent_oid)?;
        let parent_map: HashMap<String, (ObjectId, u32)> =
            flatten_tree_with_mode(repo, parent.tree, "")?
                .into_iter()
                .collect();
        let differs = path_differs_for_specs(&commit_map, &parent_map, paths);
        if differs {
            differs_any = true;
        } else {
            treesame_parents += 1;
        }
    }

    if treesame_parents == 1 {
        return Ok(false);
    }
    if differs_any {
        return Ok(true);
    }
    Ok(false)
}

fn compute_cherry_patch_id(
    repo: &Repository,
    oid: &ObjectId,
    paths: &[String],
) -> Result<Option<ObjectId>> {
    let algo = repo.odb.hash_algo();
    if paths.is_empty() {
        compute_patch_id(algo, &repo.odb, oid)
    } else {
        compute_patch_id_for_paths(algo, &repo.odb, oid, paths)
    }
}

fn path_differs_for_specs(
    current: &HashMap<String, (ObjectId, u32)>,
    parent: &HashMap<String, (ObjectId, u32)>,
    specs: &[String],
) -> bool {
    let mut paths = std::collections::BTreeSet::new();
    paths.extend(current.keys().cloned());
    paths.extend(parent.keys().cloned());

    for path in &paths {
        if !crate::pathspec::matches_pathspec_list(path, specs) {
            continue;
        }
        if current.get(path) != parent.get(path) {
            return true;
        }
    }
    false
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<crate::objects::CommitData> {
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    repo.odb.hot_path_metrics().record_commit_parse();
    parse_commit(&object.data)
}

fn rev_list_result_from_commits(commits: Vec<ObjectId>) -> RevListResult {
    RevListResult {
        commits,
        objects: Vec::new(),
        omitted_objects: Vec::new(),
        missing_objects: Vec::new(),
        boundary_commits: Vec::new(),
        left_right_map: HashMap::new(),
        cherry_equivalent: HashSet::new(),
        per_commit_object_counts: Vec::new(),
        object_walk_tips: Vec::new(),
        objects_print_commit: Vec::new(),
        object_segments: Vec::new(),
        bitmap_object_format: false,
        reachable_commit_count: None,
        reachable_non_commit_object_count: None,
        tip_annotated_tag_by_commit: HashMap::new(),
    }
}

fn rev_list_acceleration_compatible(options: &RevListOptions) -> bool {
    !options.first_parent
        && !options.ancestry_path
        && options.ancestry_path_bottoms.is_empty()
        && !options.simplify_by_decoration
        && options.simplify_by_decoration_oids.is_empty()
        && options.paths.is_empty()
        && !options.full_history
        && !options.sparse
        && !options.simplify_merges
        && !options.show_pulls
        && !options.boundary
        && !options.left_right
        && !options.left_only
        && !options.right_only
        && !options.cherry_mark
        && !options.cherry_pick
        && options.min_parents.is_none()
        && options.max_parents.is_none()
        && options.symmetric_left.is_none()
        && options.symmetric_right.is_none()
        && options.until_cutoff.is_none()
        && options.since_cutoff.is_none()
        && !options.maximal_only
        && options.skip == 0
        && options.max_count.is_none()
        && options.ordering == OrderingMode::Default
        && !options.reverse
        && !options.filter_print_omitted
        && !options.in_commit_order
        && !options.no_kept_objects
        && !options.unpacked_only
        && !options.exclude_first_parent_only
        && !options.exclude_promisor_objects
        && options.missing_action == MissingAction::Error
        && !options.include_reflog_entries
        && !options.include_indexed_objects
        && !options.exclude_indexed_objects
        && options.output_mode == OutputMode::OidOnly
        && !options.no_object_names
}

fn wants_bitmap_rev_list(options: &RevListOptions) -> bool {
    options.use_bitmap_index || options.count
}

fn peeled_wants_from_rev_list_inputs(
    include: &[ObjectId],
    object_roots: &[RootObject],
) -> (PeeledTips, Vec<ObjectId>) {
    let mut peeled = PeeledTips {
        commits: include.to_vec(),
        tags: Vec::new(),
        extra_objects: Vec::new(),
    };
    let mut filter_want_tips = include.to_vec();
    for root in object_roots {
        filter_want_tips.push(root.oid);
        match root.expected_kind {
            Some(ExpectedObjectKind::Tag) => peeled.tags.push(root.oid),
            Some(ExpectedObjectKind::Tree) | Some(ExpectedObjectKind::Blob) => {
                peeled.extra_objects.push(root.oid);
            }
            Some(ExpectedObjectKind::Commit) => peeled.commits.push(root.oid),
            None => peeled.extra_objects.push(root.oid),
        }
    }
    peeled.commits.sort();
    peeled.commits.dedup();
    (peeled, filter_want_tips)
}

fn try_commit_graph_count_only(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
    options: &RevListOptions,
) -> Result<Option<RevListResult>> {
    if !options.count || options.objects || !options.use_commit_graph {
        return Ok(None);
    }
    if !crate::rev_parse::load_graft_parents(&repo.git_dir).is_empty() {
        return Ok(None);
    }
    let Some(chain) = CommitGraphChain::try_load_with_caches(
        &repo.git_dir.join("objects"),
        Some(repo.caches().as_ref()),
    )
    .ok()
    .flatten() else {
        return Ok(None);
    };
    let shallow = load_shallow_boundaries(&repo.git_dir);
    let Some(n) = chain.count_reachable_commits(include, exclude, false, &shallow) else {
        return Ok(None);
    };
    Ok(Some(RevListResult {
        commits: Vec::new(),
        objects: Vec::new(),
        omitted_objects: Vec::new(),
        missing_objects: Vec::new(),
        boundary_commits: Vec::new(),
        left_right_map: HashMap::new(),
        cherry_equivalent: HashSet::new(),
        per_commit_object_counts: Vec::new(),
        object_walk_tips: Vec::new(),
        objects_print_commit: Vec::new(),
        object_segments: Vec::new(),
        bitmap_object_format: false,
        reachable_commit_count: Some(n),
        reachable_non_commit_object_count: None,
        tip_annotated_tag_by_commit: HashMap::new(),
    }))
}

fn try_rev_list_accelerated(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
    object_roots: &[RootObject],
    negative_object_roots: &[RootObject],
    options: &RevListOptions,
) -> Result<Option<RevListResult>> {
    if !rev_list_acceleration_compatible(options) {
        return Ok(None);
    }

    if !wants_bitmap_rev_list(options) || !bitmap_filter_supported(options.filter.as_ref()) {
        return try_commit_graph_count_only(repo, include, exclude, options);
    }

    let index = match repo.odb.with_pack_read_context(|| BitmapIndex::open(repo)) {
        Ok(Some(index)) => index,
        Ok(None) => {
            return try_commit_graph_count_only(repo, include, exclude, options);
        }
        Err(e) => {
            return Err(Error::CorruptObject(format!("pack bitmap: {e}")));
        }
    };
    let (peeled_wants, filter_want_tips) = peeled_wants_from_rev_list_inputs(include, object_roots);
    let reachable = match index.reachability_with_peeled_wants(
        repo,
        peeled_wants,
        exclude,
        options.filter.as_ref(),
        options.missing_action,
        &filter_want_tips,
    ) {
        Ok(set) => set,
        Err(BitmapWalkError::Unsupported(_)) => {
            return try_commit_graph_count_only(repo, include, exclude, options);
        }
        Err(BitmapWalkError::Bitmap(e)) => {
            return Err(Error::CorruptObject(format!("pack bitmap: {e}")));
        }
        Err(BitmapWalkError::Repository(e)) => return Err(e),
    };

    let skip_trees = skip_tree_descent_for_object_type_filter(options.filter.as_ref());
    let excluded_object_ids = excluded_object_root_ids(
        repo,
        negative_object_roots,
        options.missing_action,
        None,
        skip_trees,
        true,
        false,
    )?;

    let (commit_count, non_commit_count) = if excluded_object_ids.is_empty() {
        let commit_count = reachable
            .count_by_kind(ObjectKind::Commit)
            .map_err(|e| Error::CorruptObject(format!("pack bitmap: {e}")))?;
        let total = reachable.count();
        (commit_count, total.saturating_sub(commit_count))
    } else {
        let commit_count = reachable
            .iter_by_kind(ObjectKind::Commit)
            .filter(|oid| !excluded_object_ids.contains(oid))
            .count();
        let non_commit_count = [ObjectKind::Tree, ObjectKind::Blob, ObjectKind::Tag]
            .into_iter()
            .flat_map(|kind| reachable.iter_by_kind(kind))
            .filter(|oid| !excluded_object_ids.contains(oid))
            .count();
        (commit_count, non_commit_count)
    };

    let (commits, objects) = if options.count {
        (Vec::new(), Vec::new())
    } else {
        let commits: Vec<ObjectId> = reachable
            .iter_by_kind(ObjectKind::Commit)
            .filter(|oid| !excluded_object_ids.contains(oid))
            .collect();
        let objects: Vec<(ObjectId, String)> =
            [ObjectKind::Tree, ObjectKind::Blob, ObjectKind::Tag]
                .into_iter()
                .flat_map(|kind| {
                    reachable
                        .iter_by_kind(kind)
                        .filter(|oid| !excluded_object_ids.contains(oid))
                        .map(|oid| (oid, String::new()))
                })
                .collect();
        (commits, objects)
    };

    Ok(Some(RevListResult {
        commits,
        objects,
        omitted_objects: Vec::new(),
        missing_objects: Vec::new(),
        boundary_commits: Vec::new(),
        left_right_map: HashMap::new(),
        cherry_equivalent: HashSet::new(),
        per_commit_object_counts: Vec::new(),
        object_walk_tips: if options.objects {
            filter_want_tips
        } else {
            Vec::new()
        },
        objects_print_commit: Vec::new(),
        object_segments: Vec::new(),
        bitmap_object_format: true,
        reachable_commit_count: options.count.then_some(commit_count),
        reachable_non_commit_object_count: (options.count && options.objects)
            .then_some(non_commit_count),
        tip_annotated_tag_by_commit: HashMap::new(),
    }))
}

fn can_use_bounded_limit_list(options: &RevListOptions) -> bool {
    (options.max_count.is_some() || options.skip > 0)
        && !options.all_refs
        && !options.first_parent
        && !options.objects
        && options.paths.is_empty()
        && !options.left_right
        && !options.left_only
        && !options.right_only
        && !options.cherry_mark
        && !options.cherry_pick
        && !options.ancestry_path
        && !options.simplify_by_decoration
        && options.simplify_by_decoration_oids.is_empty()
        && !options.simplify_merges
        && options.min_parents.is_none()
        && options.max_parents.is_none()
        && !options.maximal_only
        && options.symmetric_left.is_none()
        && options.symmetric_right.is_none()
        && options.until_cutoff.is_none()
        && options.since_cutoff.is_none()
        && !options.boundary
        && !options.no_kept_objects
        && !options.unpacked_only
        && !options.exclude_first_parent_only
        && !options.exclude_promisor_objects
        && options.missing_action == MissingAction::Error
        && matches!(
            options.ordering,
            OrderingMode::Default | OrderingMode::DateOrderWalk
        )
}

fn try_bounded_limit_list(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
    options: &RevListOptions,
) -> Result<Option<Vec<ObjectId>>> {
    if include.is_empty() || !can_use_bounded_limit_list(options) {
        return Ok(None);
    }

    let need = options
        .skip
        .saturating_add(options.max_count.unwrap_or(usize::MAX));
    let mut ordered = commits_reachable_excluding_ancestors_of(repo, include, exclude, Some(need))?;
    if options.skip > 0 {
        ordered = ordered.into_iter().skip(options.skip).collect();
    }
    if let Some(max) = options.max_count {
        ordered.truncate(max);
    }
    if options.reverse {
        ordered.reverse();
    }
    Ok(Some(ordered))
}

fn extend_split_token(
    token: &str,
    not_mode: bool,
    positive: &mut Vec<String>,
    negative: &mut Vec<String>,
) {
    let (pos, neg) = split_revision_token(token);
    if not_mode {
        positive.extend(neg);
        negative.extend(pos);
    } else {
        positive.extend(pos);
        negative.extend(neg);
    }
}

/// Git `parse_long_opt`-style parsing for a single argv vector (used for `--stdin` lines).
///
/// Returns `Some((consumed_arg_count, value))` when `line` is `--<opt>` or `--<opt>=...`.
/// Consumed count is 1 for stuck form, 2 for detached form (caller must supply next line as value).
fn parse_long_opt_value(opt: &str, argv0: &str, argv1: Option<&str>) -> Option<(usize, String)> {
    let rest = argv0.strip_prefix("--")?;
    let rest = rest.strip_prefix(opt)?;
    if let Some(stripped) = rest.strip_prefix('=') {
        return Some((1, stripped.to_owned()));
    }
    if !rest.is_empty() {
        return None;
    }
    let next = argv1?;
    Some((2, next.to_owned()))
}

fn stdin_die_requires_value(opt: &str) -> Error {
    RevListError::MissingOptionValue {
        option: opt.to_owned(),
    }
    .into()
}

fn apply_stdin_pseudo_opt(
    git_dir: &Path,
    line: &str,
    next_line: Option<&str>,
    not_mode: bool,
    positive: &mut Vec<String>,
    negative: &mut Vec<String>,
    stdin_all_refs: &mut bool,
) -> Result<Option<usize>> {
    if line == "--end-of-options" {
        return Ok(Some(1));
    }
    if line == "--all" {
        if not_mode {
            for (_, oid) in refs::list_refs(git_dir, "refs/")? {
                let s = oid.to_hex();
                negative.push(s);
            }
            if let Ok(head_oid) = refs::resolve_ref(git_dir, "HEAD") {
                negative.push(head_oid.to_hex());
            }
        } else {
            *stdin_all_refs = true;
        }
        return Ok(Some(1));
    }
    if line == "--not" {
        return Ok(Some(1));
    }
    if line == "--branches" {
        for (_, oid) in refs::list_refs(git_dir, "refs/heads/")? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(1));
    }
    if line == "--tags" {
        for (_, oid) in refs::list_refs(git_dir, "refs/tags/")? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(1));
    }
    if line == "--remotes" {
        for (_, oid) in refs::list_refs(git_dir, "refs/remotes/")? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(1));
    }
    if let Some((consumed, pattern)) = parse_long_opt_value("branches", line, next_line) {
        let full_pattern = format!("refs/heads/{pattern}");
        for (_, oid) in refs::list_refs_glob(git_dir, &full_pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(consumed));
    }
    if let Some((1, pattern)) = parse_long_opt_value("tags", line, None) {
        let full_pattern = format!("refs/tags/{pattern}");
        for (_, oid) in refs::list_refs_glob(git_dir, &full_pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(1));
    }
    if let Some((consumed, pattern)) = parse_long_opt_value("tags", line, next_line) {
        let full_pattern = format!("refs/tags/{pattern}");
        for (_, oid) in refs::list_refs_glob(git_dir, &full_pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(consumed));
    }
    if let Some((1, pattern)) = parse_long_opt_value("remotes", line, None) {
        let full_pattern = format!("refs/remotes/{pattern}");
        for (_, oid) in refs::list_refs_glob(git_dir, &full_pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(1));
    }
    if let Some((consumed, pattern)) = parse_long_opt_value("remotes", line, next_line) {
        let full_pattern = format!("refs/remotes/{pattern}");
        for (_, oid) in refs::list_refs_glob(git_dir, &full_pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(consumed));
    }
    if line == "--glob" {
        return Err(stdin_die_requires_value("--glob"));
    }
    if let Some((1, pattern)) = parse_long_opt_value("glob", line, None) {
        for (_, oid) in refs::list_refs_glob(git_dir, &pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(1));
    }
    if let Some((consumed, pattern)) = parse_long_opt_value("glob", line, next_line) {
        for (_, oid) in refs::list_refs_glob(git_dir, &pattern)? {
            let s = oid.to_hex();
            if not_mode {
                negative.push(s);
            } else {
                positive.push(s);
            }
        }
        return Ok(Some(consumed));
    }
    if line == "--no-walk" || line.starts_with("--no-walk=") {
        if let Some(rest) = line.strip_prefix("--no-walk=") {
            if rest == "sorted" || rest == "unsorted" {
                return Ok(Some(1));
            }
            return Err(RevListError::InvalidNoWalkArgument.into());
        }
        return Ok(Some(1));
    }
    if line.starts_with("--") {
        return Err(RevListError::InvalidStdinOption {
            line: line.to_owned(),
        }
        .into());
    }
    if line.starts_with('-') {
        return Err(RevListError::InvalidStdinOption {
            line: line.to_owned(),
        }
        .into());
    }
    Ok(None)
}

/// Read `--stdin` revision lines and pathspec tail (after `--`), matching Git `read_revisions_from_stdin`.
fn read_revisions_from_stdin_lines(git_dir: &Path) -> Result<StdinRevisionSpecs> {
    let stdin = std::io::read_to_string(std::io::stdin()).map_err(Error::Io)?;
    let lines: Vec<String> = stdin.lines().map(std::borrow::ToOwned::to_owned).collect();

    let mut positive = Vec::new();
    let mut negative = Vec::new();
    let mut stdin_all_refs = false;
    let mut stdin_not_mode = false;
    let mut seen_end_of_options = false;
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i].as_str();
        if line.is_empty() {
            break;
        }
        if line == "--" {
            i += 1;
            let paths: Vec<String> = lines[i..].to_vec();
            return Ok((positive, negative, stdin_all_refs, paths));
        }

        if !seen_end_of_options && line.starts_with('-') {
            if line == "--end-of-options" {
                seen_end_of_options = true;
                i += 1;
                continue;
            }
            let next = lines.get(i + 1).map(|s| s.as_str());
            match apply_stdin_pseudo_opt(
                git_dir,
                line,
                next,
                stdin_not_mode,
                &mut positive,
                &mut negative,
                &mut stdin_all_refs,
            )? {
                Some(consumed) => {
                    if line == "--not" && consumed == 1 {
                        stdin_not_mode = !stdin_not_mode;
                    }
                    i += consumed;
                }
                None => {
                    extend_split_token(line, stdin_not_mode, &mut positive, &mut negative);
                    i += 1;
                }
            }
            continue;
        }

        extend_split_token(line, stdin_not_mode, &mut positive, &mut negative);
        i += 1;
    }

    Ok((positive, negative, stdin_all_refs, Vec::new()))
}

/// Merge command-line revision strings with `--stdin` lines (and optional stdin pathspecs).
///
/// Returns `(positive_specs, negative_specs, stdin_saw_all, stdin_pathspecs)`.
///
/// Command-line `--not` is applied while building `args_specs` (see `rev-list`); stdin uses an
/// independent `--not` toggle, matching Git.
///
/// # Errors
///
/// Returns [`Error::Message`] for Git-compatible `fatal:` stderr lines from stdin mode.
pub fn collect_revision_specs_with_stdin(
    git_dir: &Path,
    args_specs: &[String],
    read_stdin: bool,
) -> Result<StdinRevisionSpecs> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();

    for spec in args_specs {
        let (pos, neg) = split_revision_token(spec);
        positive.extend(pos);
        negative.extend(neg);
    }

    if !read_stdin {
        return Ok((positive, negative, false, Vec::new()));
    }

    let (s_pos, s_neg, stdin_all_refs, stdin_paths) = read_revisions_from_stdin_lines(git_dir)?;
    positive.extend(s_pos);
    negative.extend(s_neg);

    Ok((positive, negative, stdin_all_refs, stdin_paths))
}

/// Resolve every local tag object ID.
pub fn tag_targets(git_dir: &Path) -> Result<HashSet<ObjectId>> {
    Ok(refs::list_refs(git_dir, "refs/tags/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect())
}

pub(crate) struct CommitGraph<'r> {
    repo: &'r Repository,
    first_parent_only: bool,
    graph_chain: Option<CommitGraphChain>,
    parents: HashMap<ObjectId, Vec<ObjectId>>,
    commit_trees: HashMap<ObjectId, ObjectId>,
    committer_time: HashMap<ObjectId, i64>,
    author_time: HashMap<ObjectId, i64>,
    shallow_boundaries: HashSet<ObjectId>,
    graft_parents: HashMap<ObjectId, Vec<ObjectId>>,
}

impl<'r> CommitGraph<'r> {
    pub(crate) fn new(repo: &'r Repository, first_parent_only: bool) -> Self {
        Self::with_commit_graph(repo, first_parent_only, false)
    }

    pub(crate) fn with_commit_graph(
        repo: &'r Repository,
        first_parent_only: bool,
        use_commit_graph: bool,
    ) -> Self {
        let shallow_boundaries = load_shallow_boundaries(&repo.git_dir);
        let graft_parents = crate::rev_parse::load_graft_parents(&repo.git_dir);
        let graph_chain = if use_commit_graph {
            CommitGraphChain::try_load_with_caches(
                &repo.git_dir.join("objects"),
                Some(repo.caches().as_ref()),
            )
            .ok()
            .flatten()
        } else {
            None
        };
        Self {
            repo,
            first_parent_only,
            graph_chain,
            parents: HashMap::new(),
            commit_trees: HashMap::new(),
            committer_time: HashMap::new(),
            author_time: HashMap::new(),
            shallow_boundaries,
            graft_parents,
        }
    }

    pub(crate) fn commit_tree(&mut self, oid: ObjectId) -> Result<ObjectId> {
        if let Some(tree) = self.commit_trees.get(&oid).copied() {
            return Ok(tree);
        }
        self.populate(oid)?;
        if let Some(tree) = self.commit_trees.get(&oid).copied() {
            return Ok(tree);
        }
        let commit = load_commit(self.repo, oid)?;
        self.commit_trees.insert(oid, commit.tree);
        Ok(commit.tree)
    }

    pub(crate) fn parent_commit_trees(&mut self, oid: ObjectId) -> Result<Vec<ObjectId>> {
        let parents = self.parents_of(oid)?;
        let mut trees = Vec::with_capacity(parents.len());
        for parent in parents {
            trees.push(self.commit_tree(parent)?);
        }
        Ok(trees)
    }

    pub(crate) fn parents_of(&mut self, oid: ObjectId) -> Result<Vec<ObjectId>> {
        self.populate(oid)?;
        Ok(self.parents.get(&oid).cloned().unwrap_or_default())
    }

    fn committer_time(&mut self, oid: ObjectId) -> i64 {
        if self.populate(oid).is_err() {
            return 0;
        }
        self.committer_time.get(&oid).copied().unwrap_or(0)
    }

    fn author_time(&mut self, oid: ObjectId) -> i64 {
        if self.populate(oid).is_err() {
            return 0;
        }
        if !self.author_time.contains_key(&oid) {
            self.fill_author_time_from_object(oid);
        }
        self.author_time.get(&oid).copied().unwrap_or(0)
    }

    fn fill_author_time_from_object(&mut self, oid: ObjectId) {
        if let Ok(commit) = load_commit(self.repo, oid) {
            self.author_time
                .insert(oid, committer_unix_seconds_for_ordering(&commit.author));
        }
    }

    fn sort_key(&mut self, oid: ObjectId, author: bool) -> i64 {
        if author {
            self.author_time(oid)
        } else {
            self.committer_time(oid)
        }
    }

    fn populate(&mut self, oid: ObjectId) -> Result<()> {
        if self.parents.contains_key(&oid) {
            return Ok(());
        }
        if let Some(chain) = &self.graph_chain {
            if let Some((mut parents, ctime)) = chain.graph_commit(&oid) {
                if self.shallow_boundaries.contains(&oid) {
                    parents.clear();
                }
                if let Some(graft_parents) = self.graft_parents.get(&oid) {
                    parents = graft_parents.clone();
                }
                if self.first_parent_only && parents.len() > 1 {
                    parents.truncate(1);
                }
                self.committer_time.insert(oid, ctime);
                self.parents.insert(oid, parents);
                return Ok(());
            }
        }
        let commit = load_commit(self.repo, oid)?;
        // Shallow boundaries: treat commit as having no parents
        let mut parents = if self.shallow_boundaries.contains(&oid) {
            Vec::new()
        } else {
            commit.parents
        };
        if let Some(graft_parents) = self.graft_parents.get(&oid) {
            parents = graft_parents.clone();
        }
        if self.first_parent_only && parents.len() > 1 {
            parents.truncate(1);
        }
        self.committer_time
            .insert(oid, committer_unix_seconds_for_ordering(&commit.committer));
        self.author_time
            .insert(oid, committer_unix_seconds_for_ordering(&commit.author));
        self.commit_trees.insert(oid, commit.tree);
        self.parents.insert(oid, parents);
        Ok(())
    }
}

/// Load shallow boundary commit OIDs from `.git/shallow`.
fn load_shallow_boundaries(git_dir: &Path) -> HashSet<ObjectId> {
    let shallow_path = git_dir.join("shallow");
    let mut set = HashSet::new();
    if let Ok(contents) = fs::read_to_string(&shallow_path) {
        for line in contents.lines() {
            let line = line.trim();
            if !line.is_empty() {
                if let Ok(oid) = line.parse::<ObjectId>() {
                    set.insert(oid);
                }
            }
        }
    }
    set
}

fn commit_tips_from_ref_pairs(
    repo: &Repository,
    pairs: &[(String, ObjectId)],
    exclusions: &RefExclusions,
) -> Result<Vec<ObjectId>> {
    let namespace_prefix = git_namespace_prefix();
    let mut raw = Vec::new();
    for (refname, oid) in pairs {
        if exclusions.ref_excluded(strip_git_namespace(refname, &namespace_prefix), refname) {
            continue;
        }
        raw.push(*oid);
    }
    peel_ref_oids_to_unique_commits(repo, raw)
}

fn peel_ref_oids_to_unique_commits(repo: &Repository, raw: Vec<ObjectId>) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for oid in raw {
        match peel_to_commit(repo, oid) {
            Ok(commit_oid) if seen.insert(commit_oid) => out.push(commit_oid),
            Err(_) => {}
            _ => {}
        }
    }
    out.sort();
    Ok(out)
}

/// Compute which commits survive `--simplify-by-decoration`, mirroring Git's history
/// simplification (`simplify_commit` / `rewrite_parents` in revision.c with the
/// decoration-based TREESAME oracle from `rev_compare_tree`).
///
/// A commit is "interesting" (TREE_DIFFERENT, never pruned) iff it is decorated. Undecorated
/// commits are TREESAME and are candidates for pruning; their parent links are rewritten so
/// that children point past them to the nearest interesting ancestors. A commit is kept iff:
///   * it is decorated, or
///   * it is a root (no parents in the reachable graph) — roots are always interesting, or
///   * it is a relevant merge: after rewriting and removing redundant parents (those reachable
///     via another parent), it still has >= 2 distinct relevant parents.
///
/// `ordered` must be in walk order with every commit listed before its parents (topo order),
/// so processing it in reverse computes each commit's rewritten parents bottom-up.
fn compute_simplify_by_decoration_keep_set(
    graph: &mut CommitGraph,
    ordered: &[ObjectId],
    decorated: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let in_walk: HashSet<ObjectId> = ordered.iter().copied().collect();
    // For each commit, its list of rewritten relevant parents (the representatives a child
    // sees when it points at this commit). For a kept commit this is itself; for a pruned
    // commit it is the flattened relevant parents.
    let mut rewritten: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut keep: HashSet<ObjectId> = HashSet::new();

    // Process parents before children: `ordered` lists children first, so iterate in reverse.
    for &oid in ordered.iter().rev() {
        let raw_parents: Vec<ObjectId> = graph
            .parents_of(oid)?
            .into_iter()
            .filter(|p| in_walk.contains(p))
            .collect();

        // Build the relevant-parent list: replace each pruned parent with its own
        // rewritten representatives (already computed since parents precede children here).
        let mut relevant: Vec<ObjectId> = Vec::new();
        let mut seen = HashSet::new();
        for parent in &raw_parents {
            if keep.contains(parent) {
                if seen.insert(*parent) {
                    relevant.push(*parent);
                }
            } else if let Some(reps) = rewritten.get(parent) {
                for &rep in reps {
                    if seen.insert(rep) {
                        relevant.push(rep);
                    }
                }
            }
        }

        // Remove redundant parents: a relevant parent reachable from another relevant parent
        // (through the kept subgraph) is dropped, matching Git's parent-list simplification.
        if relevant.len() > 1 {
            relevant = remove_redundant_relevant_parents(graph, &relevant, &keep)?;
        }

        let is_root = raw_parents.is_empty();
        let kept = decorated.contains(&oid) || is_root || relevant.len() >= 2;
        if kept {
            keep.insert(oid);
            // A kept commit represents itself to its children.
            rewritten.insert(oid, vec![oid]);
        } else {
            // A pruned commit forwards its relevant parents to its children.
            rewritten.insert(oid, relevant);
        }
    }

    Ok(keep)
}

/// Drop parents from `relevant` that are reachable from another parent through the kept
/// subgraph (so a merge whose second parent is an ancestor of its first parent collapses).
fn remove_redundant_relevant_parents(
    graph: &mut CommitGraph,
    relevant: &[ObjectId],
    keep: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    for (i, &candidate) in relevant.iter().enumerate() {
        let mut redundant = false;
        for (j, &other) in relevant.iter().enumerate() {
            if i == j {
                continue;
            }
            if relevant_parent_reaches(graph, other, candidate, keep)? {
                redundant = true;
                break;
            }
        }
        if !redundant {
            out.push(candidate);
        }
    }
    Ok(out)
}

/// Walk ancestors of `start` over the full graph to see if `target` is a proper ancestor.
/// Used to drop a relevant parent that is already reachable through another parent.
fn relevant_parent_reaches(
    graph: &mut CommitGraph,
    start: ObjectId,
    target: ObjectId,
    _keep: &HashSet<ObjectId>,
) -> Result<bool> {
    if start == target {
        // A parent cannot make itself redundant; the caller already deduplicated by OID.
        return Ok(false);
    }
    let mut stack = vec![start];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        for parent in graph.parents_of(oid)? {
            if parent == target {
                return Ok(true);
            }
            stack.push(parent);
        }
    }
    Ok(false)
}

fn ref_pairs_for_all(repo: &Repository) -> Result<Vec<(String, ObjectId)>> {
    let mut pairs = Vec::new();
    if let Ok(head) = refs::resolve_ref(&repo.git_dir, "HEAD") {
        pairs.push(("HEAD".to_owned(), head));
    }
    pairs.extend(refs::list_refs(&repo.git_dir, "refs/")?);
    Ok(pairs)
}

fn all_ref_tips(repo: &Repository, exclusions: &RefExclusions) -> Result<Vec<ObjectId>> {
    let pairs = ref_pairs_for_all(repo)?;
    commit_tips_from_ref_pairs(repo, &pairs, exclusions)
}

/// One pass over `--all` refs: unique commit tips plus object roots for `--objects`.
fn all_ref_commit_tips_and_object_roots(
    repo: &Repository,
    exclusions: &RefExclusions,
) -> Result<(Vec<ObjectId>, Vec<RootObject>)> {
    let namespace_prefix = git_namespace_prefix();
    let pairs = ref_pairs_for_all(repo)?;
    let mut commits = HashSet::new();
    let mut roots = Vec::new();
    let mut seen_roots = HashSet::new();
    for (refname, oid) in pairs {
        if exclusions.ref_excluded(strip_git_namespace(&refname, &namespace_prefix), &refname) {
            continue;
        }
        let Ok(object) = repo.odb.read(&oid) else {
            continue;
        };
        match object.kind {
            ObjectKind::Commit => {
                commits.insert(oid);
            }
            ObjectKind::Tree | ObjectKind::Blob => {
                if seen_roots.insert(oid) {
                    roots.push(RootObject {
                        oid,
                        input: refname,
                        expected_kind: ExpectedObjectKind::from_object_kind(object.kind),
                        root_path: None,
                        wrap_with_tag: None,
                    });
                }
            }
            ObjectKind::Tag => {
                if seen_roots.insert(oid) {
                    roots.push(RootObject {
                        oid,
                        input: refname.clone(),
                        expected_kind: Some(ExpectedObjectKind::Tag),
                        root_path: None,
                        wrap_with_tag: None,
                    });
                }
                let Ok(tag) = parse_tag(&object.data) else {
                    continue;
                };
                let Some(expected_kind) = ExpectedObjectKind::from_tag_type(&tag.object_type)
                else {
                    continue;
                };
                if expected_kind == ExpectedObjectKind::Commit {
                    commits.insert(tag.object);
                    continue;
                }
                if seen_roots.insert(tag.object) {
                    roots.push(RootObject {
                        oid: tag.object,
                        input: refname,
                        expected_kind: Some(expected_kind),
                        root_path: None,
                        wrap_with_tag: Some(oid),
                    });
                }
            }
        }
    }
    let mut tips: Vec<ObjectId> = commits.into_iter().collect();
    tips.sort();
    Ok((tips, roots))
}

/// Expand named refs to peeled unique commit tips, applying `--exclude` / `--exclude-hidden` rules.
pub fn commit_tips_from_named_refs(
    repo: &Repository,
    pairs: &[(String, ObjectId)],
    exclusions: &RefExclusions,
) -> Result<Vec<ObjectId>> {
    commit_tips_from_ref_pairs(repo, pairs, exclusions)
}

/// Commit OIDs listed in `.git/shallow` (shallow clone boundaries).
///
/// At each boundary commit, history is cut: parents are omitted from the object store and must
/// not be traversed for connectivity checks (`git fsck`, `pack-objects` reachability, etc.).
#[must_use]
pub fn shallow_boundary_oids(git_dir: &Path) -> HashSet<ObjectId> {
    crate::shallow::load_shallow_boundaries(git_dir)
}

/// Shallow boundary commits on paths from `wants` when the server repository is shallow.
///
/// Matches Git `get_shallow_commits` with infinite depth and no client-advertised shallows: walk
/// from wanted tips, stop parent traversal at `.git/shallow` entries, and return those boundary
/// commits (for protocol v2 `shallow-info` and `pack-objects --shallow`).
#[must_use]
pub fn shallow_borders_reachable_from_wants(
    repo: &Repository,
    wants: &[ObjectId],
) -> Vec<ObjectId> {
    let boundaries = shallow_boundary_oids(&repo.git_dir);
    if boundaries.is_empty() || wants.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<ObjectId> = Vec::new();
    let mut seen_out = HashSet::new();
    let mut visited = HashSet::new();
    let mut q: VecDeque<ObjectId> = wants.iter().copied().collect();
    while let Some(oid) = q.pop_front() {
        if !visited.insert(oid) {
            continue;
        }
        if boundaries.contains(&oid) {
            if seen_out.insert(oid) {
                out.push(oid);
            }
            continue;
        }
        for p in commit_parent_ids(repo, oid) {
            q.push_back(p);
        }
    }
    out.sort();
    out
}

/// Compute new shallow boundary commits for `upload-pack` when the client sends `deepen <n>`.
///
/// This approximates Git's `get_shallow_commits` / `get_shallows_or_depth` for the common case:
/// non-`deepen-relative` fetches where the client lists its shallow commits and requests more
/// history. Returns commit OIDs that should be sent as `shallow` lines and registered as grafts
/// for `pack-objects --shallow`.
///
/// # Parameters
///
/// - `wants` — wanted commit OIDs (usually remote `HEAD` / ref tips).
/// - `client_shallow` — OIDs the client advertised with `shallow <oid>` before `deepen`.
/// - `deepen` — positive integer from the `deepen` pkt-line (`deepen 2` → `2`).
#[must_use]
pub fn shallow_grafts_for_upload_pack_deepen(
    repo: &Repository,
    wants: &[ObjectId],
    client_shallow: &[ObjectId],
    deepen: usize,
) -> Vec<ObjectId> {
    if deepen == 0 || wants.is_empty() {
        return Vec::new();
    }

    let server_shallow = shallow_boundary_oids(&repo.git_dir);
    let client_set: HashSet<ObjectId> = client_shallow.iter().copied().collect();

    // For a fresh (non-shallow) client, an absolute `deepen N` keeps exactly N commits counting
    // from the wanted tips, so the boundary sits at the N-th commit and the base offset is 0.
    // Using 1 here over-counts by one: with `deepen 2` on a 3-commit history the border lands on
    // the root commit (which has no parents) and no `shallow` line is advertised at all, leaving
    // the client believing it received full history and failing connectivity/fsck for the cut-off
    // parents (t5537 "shallow fetches check connectivity before writing shallow file"). When the
    // client already has a shallow boundary, `min_client_shallow_distance` supplies the real base.
    let min_hit = min_client_shallow_distance(repo, wants, &client_set, &server_shallow);
    let base = min_hit.unwrap_or(0);
    let target_depth = base.saturating_add(deepen);

    let included = commits_within_parent_depth(repo, wants, target_depth, &server_shallow);
    border_commits_not_in_client_shallow(repo, &included, &client_set)
}

fn commit_parent_ids(repo: &Repository, oid: ObjectId) -> Vec<ObjectId> {
    let Ok(obj) = repo.odb.read(&oid) else {
        return Vec::new();
    };
    if obj.kind != ObjectKind::Commit {
        return Vec::new();
    }
    parse_commit(&obj.data)
        .map(|c| c.parents)
        .unwrap_or_default()
}

fn min_client_shallow_distance(
    repo: &Repository,
    wants: &[ObjectId],
    client_shallow: &HashSet<ObjectId>,
    server_shallow: &HashSet<ObjectId>,
) -> Option<usize> {
    if client_shallow.is_empty() {
        return None;
    }
    let mut best: Option<usize> = None;
    let mut dist: HashMap<ObjectId, usize> = HashMap::new();
    let mut q: VecDeque<(ObjectId, usize)> = VecDeque::new();
    for &w in wants {
        dist.insert(w, 0);
        q.push_back((w, 0));
    }
    while let Some((oid, d)) = q.pop_front() {
        if client_shallow.contains(&oid) {
            best = Some(best.map(|b| b.min(d)).unwrap_or(d));
        }
        if server_shallow.contains(&oid) {
            continue;
        }
        for p in commit_parent_ids(repo, oid) {
            let nd = d.saturating_add(1);
            let prev = dist.get(&p).copied().unwrap_or(usize::MAX);
            if nd < prev {
                dist.insert(p, nd);
                q.push_back((p, nd));
            }
        }
    }
    best
}

fn commits_within_parent_depth(
    repo: &Repository,
    wants: &[ObjectId],
    max_depth: usize,
    server_shallow: &HashSet<ObjectId>,
) -> HashSet<ObjectId> {
    let mut best_depth: HashMap<ObjectId, usize> = HashMap::new();
    let mut q: VecDeque<(ObjectId, usize)> = VecDeque::new();
    for &w in wants {
        best_depth.insert(w, 1);
        q.push_back((w, 1));
    }
    while let Some((oid, depth)) = q.pop_front() {
        if depth > max_depth {
            continue;
        }
        if best_depth.get(&oid).copied() != Some(depth) {
            continue;
        }
        if depth == max_depth {
            continue;
        }
        if server_shallow.contains(&oid) {
            continue;
        }
        for p in commit_parent_ids(repo, oid) {
            let nd = depth.saturating_add(1);
            if nd > max_depth {
                continue;
            }
            let prev = best_depth.get(&p).copied().unwrap_or(usize::MAX);
            if nd < prev {
                best_depth.insert(p, nd);
                q.push_back((p, nd));
            }
        }
    }
    best_depth.into_keys().collect()
}

fn border_commits_not_in_client_shallow(
    repo: &Repository,
    included: &HashSet<ObjectId>,
    client_shallow: &HashSet<ObjectId>,
) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut seen_out = HashSet::new();
    for &c in included {
        if client_shallow.contains(&c) {
            continue;
        }
        let parents = commit_parent_ids(repo, c);
        let is_border = parents.iter().any(|p| !included.contains(p));
        if is_border && seen_out.insert(c) {
            out.push(c);
        }
    }
    out.sort();
    out
}

/// Shallow boundary commits for `upload-pack` when the client uses `deepen-since` and/or
/// `deepen-not` (Git runs `rev-list --max-age=…` / `--not` and derives border commits).
///
/// Returns OIDs to advertise as `shallow` lines and pass to `pack-objects --shallow`.
pub fn shallow_grafts_for_upload_pack_rev_list(
    repo: &Repository,
    wants: &[ObjectId],
    client_shallow: &[ObjectId],
    deepen_since: Option<i64>,
    deepen_not: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    if wants.is_empty() || (deepen_since.is_none() && deepen_not.is_empty()) {
        return Ok(Vec::new());
    }

    let positive: Vec<String> = wants.iter().map(|o| o.to_hex()).collect();
    let negative: Vec<String> = deepen_not
        .iter()
        .map(|o| format!("^{}", o.to_hex()))
        .collect();

    let options = RevListOptions {
        since_cutoff: deepen_since,
        missing_action: MissingAction::Allow,
        ..Default::default()
    };

    let res = rev_list(repo, &positive, &negative, &options)?;
    let included: HashSet<ObjectId> = res.commits.iter().copied().collect();
    let client_set: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    Ok(border_commits_not_in_client_shallow(
        repo,
        &included,
        &client_set,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommitDateKey {
    oid: ObjectId,
    date: i64,
    /// Insertion-order tiebreaker for equal dates, mirroring Git's `prio_queue` `ctr`
    /// (prio-queue.c): among commits with the same committer date, the one inserted *earlier*
    /// is emitted first. In this max-heap that means an earlier insertion must compare *greater*,
    /// so we order by the negated sequence. Walks that do not care about insertion order leave
    /// this at 0, falling through to the OID tiebreak (preserving prior behavior).
    seq: u64,
}

impl Ord for CommitDateKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.date.cmp(&other.date) {
            // Earlier insertion (smaller seq) should pop first => compare as "greater".
            Ordering::Equal => match other.seq.cmp(&self.seq) {
                Ordering::Equal => self.oid.cmp(&other.oid),
                ord => ord,
            },
            ord => ord,
        }
    }
}

impl PartialOrd for CommitDateKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Read every line from a newline-delimited file.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read.
pub fn read_lines(path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines().map(|line| line.to_owned()).collect())
}

/// Check if a token uses the symmetric diff `...` notation.
#[must_use]
pub fn is_symmetric_diff(token: &str) -> bool {
    token.contains("...") && !token.contains("....")
}

/// Split a symmetric diff token into (lhs, rhs).
#[must_use]
pub fn split_symmetric_diff(token: &str) -> Option<(String, String)> {
    token
        .split_once("...")
        .map(|(l, r)| (l.to_owned(), r.to_owned()))
}

fn commit_parent_trees(
    repo: &Repository,
    parents: &[ObjectId],
    missing_action: MissingAction,
    missing: &mut Vec<ObjectId>,
    missing_seen: &mut HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::with_capacity(parents.len());
    for &parent in parents {
        let commit = match load_commit(repo, parent) {
            Ok(commit) => commit,
            Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                if missing_action.reports_missing() && missing_seen.insert(parent) {
                    missing.push(parent);
                }
                continue;
            }
            Err(err) => return Err(err),
        };
        out.push(commit.tree);
    }
    Ok(out)
}

fn tree_oid_at_path(
    repo: &Repository,
    tree_state: &mut TreeWalkState,
    root_tree: ObjectId,
    path_prefix: &str,
) -> Result<Option<ObjectId>> {
    if path_prefix.is_empty() {
        return Ok(Some(root_tree));
    }
    let mut current = root_tree;
    for component in path_prefix.split('/').filter(|part| !part.is_empty()) {
        let entries = tree_state.tree_entries(repo, current)?;
        let Some(entry) = entries.iter().find(|entry| {
            entry.mode == MODE_TREE && String::from_utf8_lossy(&entry.name) == component
        }) else {
            return Ok(None);
        };
        current = entry.oid;
    }
    Ok(Some(current))
}

fn entry_oid_at_path(
    repo: &Repository,
    tree_state: &mut TreeWalkState,
    root_tree: ObjectId,
    path_prefix: &str,
    name: &str,
) -> Result<Option<ObjectId>> {
    let dir_tree = if path_prefix.is_empty() {
        root_tree
    } else {
        let Some(tree) = tree_oid_at_path(repo, tree_state, root_tree, path_prefix)? else {
            return Ok(None);
        };
        tree
    };
    let entries = tree_state.tree_entries(repo, dir_tree)?;
    Ok(entries
        .iter()
        .find_map(|entry| (String::from_utf8_lossy(&entry.name) == name).then_some(entry.oid)))
}

fn subtree_unchanged_in_all_parents(
    repo: &Repository,
    tree_state: &mut TreeWalkState,
    parent_roots: &[ObjectId],
    path_prefix: &str,
    tree_oid: ObjectId,
) -> Result<bool> {
    if parent_roots.is_empty() {
        return Ok(false);
    }
    for &root in parent_roots {
        let Some(parent_tree) = tree_oid_at_path(repo, tree_state, root, path_prefix)? else {
            return Ok(false);
        };
        if parent_tree != tree_oid {
            return Ok(false);
        }
    }
    Ok(true)
}

fn entry_unchanged_in_all_parents(
    repo: &Repository,
    tree_state: &mut TreeWalkState,
    parent_roots: &[ObjectId],
    path_prefix: &str,
    name: &str,
    oid: ObjectId,
) -> Result<bool> {
    if parent_roots.is_empty() {
        return Ok(false);
    }
    for &root in parent_roots {
        let Some(parent_oid) = entry_oid_at_path(repo, tree_state, root, path_prefix, name)? else {
            return Ok(false);
        };
        if parent_oid != oid {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Maps each tree OID to the minimum traversal depth it was entered at (Git `list-objects` /
/// `tree:<n>` semantics: the same tree may be revisited from a shallower path).
#[derive(Debug, Default)]
struct TreeWalkState {
    seen_at_depth: HashMap<ObjectId, u64>,
    parsed_trees: HashMap<ObjectId, Arc<Vec<crate::objects::TreeEntry>>>,
}

impl TreeWalkState {
    fn new() -> Self {
        Self::default()
    }

    fn tree_entries(
        &mut self,
        repo: &Repository,
        tree_oid: ObjectId,
    ) -> Result<Arc<Vec<crate::objects::TreeEntry>>> {
        if let Some(entries) = self.parsed_trees.get(&tree_oid) {
            return Ok(Arc::clone(entries));
        }
        let object = repo.odb.read(&tree_oid)?;
        if object.kind != ObjectKind::Tree {
            return Err(Error::CorruptObject(format!(
                "object {tree_oid} is not a tree"
            )));
        }
        let entries = Arc::new(parse_tree(&object.data)?);
        self.parsed_trees.insert(tree_oid, Arc::clone(&entries));
        Ok(entries)
    }

    /// Returns `true` if this tree at `depth` should be skipped (already entered at same or
    /// shallower depth).
    fn should_skip_tree(&mut self, oid: ObjectId, depth: u64) -> bool {
        match self.seen_at_depth.get(&oid).copied() {
            None => {
                self.seen_at_depth.insert(oid, depth);
                false
            }
            Some(prev) if depth >= prev => true,
            Some(_) => {
                self.seen_at_depth.insert(oid, depth);
                false
            }
        }
    }
}

fn object_walk_needs_child_read_info(
    filter: Option<&ObjectFilter>,
    sparse_lines: Option<&[String]>,
    missing_action: MissingAction,
) -> bool {
    filter.is_some() || sparse_lines.is_some() || missing_action != MissingAction::Error
}

fn excluded_object_root_ids(
    repo: &Repository,
    object_roots: &[RootObject],
    missing_action: MissingAction,
    sparse_lines: Option<&[String]>,
    skip_trees_for_type_filter: bool,
    omit_object_paths: bool,
    collect_tree_omits: bool,
) -> Result<HashSet<ObjectId>> {
    if object_roots.is_empty() {
        return Ok(HashSet::new());
    }

    let mut tree_state = TreeWalkState::new();
    let mut emitted = HashSet::new();
    let mut objects = Vec::new();
    let mut omitted = Vec::new();
    let mut missing = Vec::new();
    let mut missing_seen = HashSet::new();
    let mut top_tree_omit = None;
    let mut combine_states = None;

    for root in object_roots {
        collect_root_object(
            repo,
            root,
            &mut tree_state,
            &mut emitted,
            &mut objects,
            &mut omitted,
            &mut missing,
            &mut missing_seen,
            None,
            false,
            missing_action,
            sparse_lines,
            skip_trees_for_type_filter,
            omit_object_paths,
            None,
            collect_tree_omits,
            &mut top_tree_omit,
            &mut combine_states,
        )?;
    }

    Ok(objects.into_iter().map(|(oid, _)| oid).collect())
}

fn retain_objects_matching_pathspecs(objects: &mut Vec<(ObjectId, String)>, pathspecs: &[String]) {
    objects.retain(|(_, path)| {
        path.is_empty()
            || crate::pathspec::matches_pathspec_list(path, pathspecs)
            || pathspecs
                .iter()
                .any(|spec| crate::pathspec::pathspec_wants_descent_into_tree(spec, path))
    });
}

fn collect_pathspec_matching_tree_objects(
    repo: &Repository,
    commits: &[ObjectId],
    pathspecs: &[String],
) -> Result<Vec<(ObjectId, String)>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for commit_oid in commits {
        let commit = load_commit(repo, *commit_oid)?;
        collect_pathspec_matching_tree_objects_inner(
            repo,
            commit.tree,
            "",
            pathspecs,
            &mut seen,
            &mut out,
        )?;
    }
    Ok(out)
}

fn collect_pathspec_matching_tree_objects_inner(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    pathspecs: &[String],
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<(ObjectId, String)>,
) -> Result<()> {
    let object = repo.odb.read(&tree_oid)?;
    if object.kind != ObjectKind::Tree {
        return Err(Error::CorruptObject(format!(
            "object {tree_oid} is not a tree"
        )));
    }
    let entries = parse_tree(&object.data)?;
    for entry in entries {
        if entry.mode == 0o160000 {
            continue;
        }
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let is_tree = entry.mode == 0o040000;
        let matches = crate::pathspec::matches_pathspec_list(&path, pathspecs);
        let wants_descent = pathspecs
            .iter()
            .any(|spec| crate::pathspec::pathspec_wants_descent_into_tree(spec, &path));
        if (matches || (is_tree && wants_descent)) && seen.insert(entry.oid) {
            out.push((entry.oid, path.clone()));
        }
        if is_tree && wants_descent {
            collect_pathspec_matching_tree_objects_inner(
                repo, entry.oid, &path, pathspecs, seen, out,
            )?;
        }
    }
    Ok(())
}

/// Like [`collect_reachable_objects`], but also returns objects newly discovered per commit walk
/// plus one trailing segment for `object_roots`.
///
/// Matches Git `traverse_commit_list_filtered`: each commit's tree is processed before moving to
/// the next commit, with global de-duplication of emitted object OIDs across the full walk.
#[expect(clippy::too_many_arguments)]
fn collect_reachable_objects_segmented(
    repo: &Repository,
    graph: &mut CommitGraph<'_>,
    commits: &[ObjectId],
    object_roots: &[RootObject],
    tip_annotated_tags: &HashMap<ObjectId, ObjectId>,
    filter: Option<&ObjectFilter>,
    filter_provided: bool,
    missing_action: MissingAction,
    sparse_lines: Option<&[String]>,
    skip_trees_for_type_filter: bool,
    omit_object_paths: bool,
    packed_set: Option<&HashSet<ObjectId>>,
    collect_tree_omits: bool,
) -> Result<ReachableObjectsSegmented> {
    let mut emitted = HashSet::new();
    let mut result = Vec::new();
    let mut omitted = Vec::new();
    let mut missing = Vec::new();
    let mut missing_seen = HashSet::new();
    let mut segments: Vec<Vec<(ObjectId, String)>> = Vec::with_capacity(commits.len() + 1);
    let mut tree_state = TreeWalkState::new();
    let mut top_tree_omit =
        walk_needs_top_tree_omit_set(filter, collect_tree_omits).then(HashSet::<ObjectId>::new);
    let mut combine_states = CombineSubState::prepare_sub_states(filter, collect_tree_omits);

    for &commit_oid in commits {
        let start = result.len();
        let commit_tree = match graph.commit_tree(commit_oid) {
            Ok(tree) => tree,
            Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                if missing_action.reports_missing() && missing_seen.insert(commit_oid) {
                    missing.push(commit_oid);
                }
                segments.push(Vec::new());
                continue;
            }
            Err(err) => return Err(err),
        };
        let parent_trees_slice = if filter.is_some() {
            None
        } else {
            Some(graph.parent_commit_trees(commit_oid)?)
        };
        if parent_trees_slice
            .as_ref()
            .is_some_and(|parents| !parents.is_empty() && parents.iter().all(|&t| t == commit_tree))
        {
            segments.push(Vec::new());
            continue;
        }
        if let Some(&tag_oid) = tip_annotated_tags.get(&commit_oid) {
            if emitted.insert(tag_oid) {
                result.push((tag_oid, "tag".to_owned()));
            }
        }
        // The parent_union short-circuit dedups objects already reachable from a parent commit.
        // With an active filter this changes which trees are *visited* per commit, which breaks
        // Git's `tree:`/`combine:` skip-tree semantics (and the matching GIT_TRACE output): Git
        // visits every reachable tree per commit and relies on each filter's own seen state to
        // detect re-visits. Dedup of shown/omitted objects is already handled by `emitted` and the
        // final omitted set, so skip the union optimization whenever a filter is present.
        collect_tree_objects_filtered(
            repo,
            commit_tree,
            "",
            0,
            false,
            None,
            parent_trees_slice.as_deref(),
            &mut tree_state,
            &mut emitted,
            &mut result,
            &mut omitted,
            &mut missing,
            &mut missing_seen,
            filter,
            filter_provided,
            missing_action,
            sparse_lines,
            skip_trees_for_type_filter,
            omit_object_paths,
            packed_set,
            collect_tree_omits,
            &mut top_tree_omit,
            &mut combine_states,
        )?;
        segments.push(result[start..].to_vec());
    }

    let roots_start = result.len();
    for root in object_roots {
        collect_root_object(
            repo,
            root,
            &mut tree_state,
            &mut emitted,
            &mut result,
            &mut omitted,
            &mut missing,
            &mut missing_seen,
            filter,
            filter_provided,
            missing_action,
            sparse_lines,
            skip_trees_for_type_filter,
            omit_object_paths,
            packed_set,
            collect_tree_omits,
            &mut top_tree_omit,
            &mut combine_states,
        )?;
    }
    segments.push(result[roots_start..].to_vec());

    Ok((result, omitted, missing, segments))
}

#[derive(Clone, Copy, Debug)]
struct ListFilterBits {
    mark_seen: bool,
    do_show: bool,
    skip_tree: bool,
}

impl ListFilterBits {
    fn merge_combine(subs: &[ListFilterBits], sub_skipping: &[bool]) -> Self {
        let mut out = ListFilterBits {
            mark_seen: true,
            do_show: true,
            skip_tree: true,
        };
        for (sub, skipping) in subs.iter().zip(sub_skipping.iter()) {
            if !sub.do_show {
                out.do_show = false;
            }
            if !sub.mark_seen {
                out.mark_seen = false;
            }
            if !skipping {
                out.skip_tree = false;
            }
        }
        out
    }
}

fn walk_needs_top_tree_omit_set(filter: Option<&ObjectFilter>, collect_omits: bool) -> bool {
    collect_omits && matches!(filter, Some(ObjectFilter::TreeDepth(_)))
}

fn trace_skip_tree_contents(prefix: &str) {
    let Some(trace_val) = crate::environment::Environment::empty().var("GIT_TRACE") else {
        return;
    };
    if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
        return;
    }
    let path = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let line = format!("Skipping contents of tree {path}...\n");
    match trace_val.as_str() {
        "1" | "true" | "2" => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        path_dest => {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path_dest)
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

fn tree_depth_begin_tree(
    tree_oid: ObjectId,
    depth: u64,
    exclude_depth: u64,
    tree_state: &mut TreeWalkState,
    tree_omit_set: &mut Option<HashSet<ObjectId>>,
    collect_omits: bool,
) -> ListFilterBits {
    let include_it = depth < exclude_depth;
    let already_seen = tree_state.should_skip_tree(tree_oid, depth);
    if already_seen {
        return ListFilterBits {
            mark_seen: false,
            do_show: false,
            skip_tree: true,
        };
    }

    let been_omitted = if collect_omits {
        if let Some(omits) = tree_omit_set.as_mut() {
            if include_it {
                omits.remove(&tree_oid);
                false
            } else {
                !omits.insert(tree_oid)
            }
        } else {
            false
        }
    } else {
        false
    };

    let skip_tree = if include_it {
        false
    } else {
        !(collect_omits && !been_omitted)
    };

    let do_show = include_it;
    ListFilterBits {
        mark_seen: true,
        do_show,
        skip_tree,
    }
}

fn tree_depth_blob(
    oid: ObjectId,
    _size: u64,
    parent_depth: u64,
    exclude_depth: u64,
    tree_omit_set: &mut Option<HashSet<ObjectId>>,
    collect_omits: bool,
) -> ListFilterBits {
    let include_it = parent_depth.saturating_add(1) < exclude_depth;
    if collect_omits {
        if let Some(omits) = tree_omit_set.as_mut() {
            if include_it {
                omits.remove(&oid);
            } else {
                omits.insert(oid);
            }
        }
    }
    ListFilterBits {
        // Omitted blobs stay not-SEEN so the same OID can be revisited from a shallower path (t6112).
        mark_seen: include_it,
        do_show: include_it,
        skip_tree: false,
    }
}

fn filter_object_bits_tree_begin(
    f: &ObjectFilter,
    tree_oid: ObjectId,
    depth: u64,
    tree_state: &mut TreeWalkState,
    tree_omit_set: &mut Option<HashSet<ObjectId>>,
    collect_omits: bool,
    sub_states: Option<&mut [CombineSubState]>,
) -> ListFilterBits {
    match f {
        ObjectFilter::BlobNone | ObjectFilter::BlobLimit(_) => ListFilterBits {
            mark_seen: true,
            do_show: true,
            skip_tree: false,
        },
        ObjectFilter::TreeDepth(excl) => tree_depth_begin_tree(
            tree_oid,
            depth,
            *excl,
            tree_state,
            tree_omit_set,
            collect_omits,
        ),
        ObjectFilter::SparseOid(_) => ListFilterBits {
            mark_seen: true,
            do_show: true,
            skip_tree: false,
        },
        ObjectFilter::ObjectType(k) => {
            // Match Git `filter_object_type` LOFS_BEGIN_TREE: only commit/tag filters skip recursion;
            // blob filters must walk trees to reach blobs.
            let show = *k == FilterObjectKind::Tree;
            let skip_tree = matches!(k, FilterObjectKind::Commit | FilterObjectKind::Tag);
            ListFilterBits {
                mark_seen: true,
                do_show: show,
                skip_tree,
            }
        }
        ObjectFilter::Combine(parts) => {
            // `sub_states` is always `Some` for a `Combine` filter; fall back to the
            // neutral merge if the invariant is ever violated rather than panicking.
            let Some(states) = sub_states else {
                return ListFilterBits::merge_combine(&[], &[]);
            };
            debug_assert_eq!(states.len(), parts.len());
            let mut bits = Vec::with_capacity(parts.len());
            let mut skipping = Vec::with_capacity(parts.len());
            for (i, p) in parts.iter().enumerate() {
                let b = filter_object_bits_tree_begin(
                    p,
                    tree_oid,
                    depth,
                    &mut states[i].tree_state,
                    &mut states[i].tree_omit_set,
                    collect_omits,
                    None,
                );
                if b.skip_tree {
                    states[i].is_skipping_tree = true;
                    states[i].skip_tree_oid = Some(tree_oid);
                } else {
                    states[i].is_skipping_tree = false;
                    states[i].skip_tree_oid = None;
                }
                bits.push(b);
                skipping.push(states[i].is_skipping_tree);
            }
            ListFilterBits::merge_combine(&bits, &skipping)
        }
    }
}

fn filter_object_bits_blob(
    f: &ObjectFilter,
    oid: ObjectId,
    size: u64,
    parent_depth: u64,
    tree_omit_set: &mut Option<HashSet<ObjectId>>,
    collect_omits: bool,
    sub_states: Option<&mut [CombineSubState]>,
) -> ListFilterBits {
    match f {
        ObjectFilter::BlobNone => ListFilterBits {
            mark_seen: true,
            do_show: false,
            skip_tree: false,
        },
        ObjectFilter::BlobLimit(limit) => {
            let include = size < *limit;
            ListFilterBits {
                mark_seen: true,
                do_show: include,
                skip_tree: false,
            }
        }
        ObjectFilter::TreeDepth(excl) => {
            tree_depth_blob(oid, size, parent_depth, *excl, tree_omit_set, collect_omits)
        }
        ObjectFilter::SparseOid(_) => ListFilterBits {
            mark_seen: true,
            do_show: true,
            skip_tree: false,
        },
        ObjectFilter::ObjectType(k) => {
            let show = *k == FilterObjectKind::Blob;
            ListFilterBits {
                mark_seen: true,
                do_show: show,
                skip_tree: false,
            }
        }
        ObjectFilter::Combine(parts) => {
            // `sub_states` is always `Some` for a `Combine` filter; fall back to the
            // neutral merge if the invariant is ever violated rather than panicking.
            let Some(states) = sub_states else {
                return ListFilterBits::merge_combine(&[], &[]);
            };
            let mut bits = Vec::with_capacity(parts.len());
            let mut skipping = Vec::with_capacity(parts.len());
            for (i, p) in parts.iter().enumerate() {
                let b = filter_object_bits_blob(
                    p,
                    oid,
                    size,
                    parent_depth,
                    &mut states[i].tree_omit_set,
                    collect_omits,
                    None,
                );
                bits.push(b);
                skipping.push(states[i].is_skipping_tree);
            }
            ListFilterBits::merge_combine(&bits, &skipping)
        }
    }
}

#[derive(Debug)]
struct CombineSubState {
    tree_state: TreeWalkState,
    tree_omit_set: Option<HashSet<ObjectId>>,
    is_skipping_tree: bool,
    skip_tree_oid: Option<ObjectId>,
}

impl CombineSubState {
    fn new(parts_len: usize, collect_omits: bool) -> Vec<Self> {
        (0..parts_len)
            .map(|_| Self {
                tree_state: TreeWalkState::new(),
                tree_omit_set: collect_omits.then(HashSet::new),
                is_skipping_tree: false,
                skip_tree_oid: None,
            })
            .collect()
    }

    fn prepare_sub_states(
        filter: Option<&ObjectFilter>,
        collect_omits: bool,
    ) -> Option<Vec<CombineSubState>> {
        match filter {
            Some(ObjectFilter::Combine(parts)) => {
                Some(CombineSubState::new(parts.len(), collect_omits))
            }
            _ => None,
        }
    }
}

#[expect(clippy::too_many_arguments)]
#[allow(dead_code)]
fn collect_tree_objects_filtered(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    depth: u64,
    explicit_root: bool,
    parent_union: Option<&HashSet<ObjectId>>,
    parent_commit_trees: Option<&[ObjectId]>,
    tree_state: &mut TreeWalkState,
    emitted: &mut HashSet<ObjectId>,
    result: &mut Vec<(ObjectId, String)>,
    omitted: &mut Vec<ObjectId>,
    missing: &mut Vec<ObjectId>,
    missing_seen: &mut HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    filter_provided: bool,
    missing_action: MissingAction,
    sparse_lines: Option<&[String]>,
    skip_trees_for_type_filter: bool,
    omit_object_paths: bool,
    packed_set: Option<&HashSet<ObjectId>>,
    collect_tree_omits: bool,
    tree_omit_set: &mut Option<HashSet<ObjectId>>,
    combine_states: &mut Option<Vec<CombineSubState>>,
) -> Result<()> {
    if !explicit_root {
        if let Some(parents) = parent_commit_trees {
            if subtree_unchanged_in_all_parents(repo, tree_state, parents, prefix, tree_oid)? {
                return Ok(());
            }
        } else if let Some(pu) = parent_union {
            if pu.contains(&tree_oid) {
                return Ok(());
            }
        }
    }
    let entries = match tree_state.tree_entries(repo, tree_oid) {
        Ok(entries) => entries,
        Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
            if missing_action.reports_missing() && missing_seen.insert(tree_oid) {
                missing.push(tree_oid);
            }
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    let bits = match filter {
        None => ListFilterBits {
            mark_seen: true,
            do_show: true,
            skip_tree: false,
        },
        Some(f) => {
            if explicit_root && matches!(f, ObjectFilter::TreeDepth(0)) {
                ListFilterBits {
                    mark_seen: true,
                    do_show: true,
                    skip_tree: true,
                }
            } else if explicit_root && !filter_provided {
                ListFilterBits {
                    mark_seen: true,
                    do_show: true,
                    skip_tree: false,
                }
            } else {
                match f {
                    ObjectFilter::Combine(_) => filter_object_bits_tree_begin(
                        f,
                        tree_oid,
                        depth,
                        tree_state,
                        tree_omit_set,
                        collect_tree_omits,
                        combine_states.as_mut().map(Vec::as_mut_slice),
                    ),
                    _ => filter_object_bits_tree_begin(
                        f,
                        tree_oid,
                        depth,
                        tree_state,
                        tree_omit_set,
                        collect_tree_omits,
                        None,
                    ),
                }
            }
        }
    };

    // Git `filter_sparse` always shows tree objects; sparse patterns gate blobs (and inherited
    // default_match), not whether the tree OID is listed.
    let tree_included = bits.do_show;
    if tree_included {
        if !packed_set.is_some_and(|p| p.contains(&tree_oid)) && emitted.insert(tree_oid) {
            let out_path = if omit_object_paths {
                String::new()
            } else {
                prefix.to_owned()
            };
            result.push((tree_oid, out_path));
        }
    } else if bits.mark_seen {
        omitted.push(tree_oid);
    }

    if bits.skip_tree {
        trace_skip_tree_contents(prefix);
    }

    if skip_trees_for_type_filter && depth == 0 && !explicit_root {
        return Ok(());
    }

    if bits.skip_tree {
        return Ok(());
    }

    let needs_child_read_info =
        object_walk_needs_child_read_info(filter, sparse_lines, missing_action);
    for entry in entries.iter() {
        if entry.mode == 0o160000 {
            continue;
        }
        let name = String::from_utf8_lossy(&entry.name).to_string();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            if needs_child_read_info {
                let child_info = match repo.odb.read_info(&entry.oid) {
                    Ok(info) => info,
                    Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                        if missing_action.reports_missing() && missing_seen.insert(entry.oid) {
                            missing.push(entry.oid);
                        }
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                if child_info.kind != ObjectKind::Tree {
                    return Err(Error::CorruptObject(format!(
                        "object {} is not a tree",
                        entry.oid
                    )));
                }
            }
            if let Some(parents) = parent_commit_trees {
                if entry_unchanged_in_all_parents(
                    repo, tree_state, parents, prefix, &name, entry.oid,
                )? {
                    continue;
                }
            } else if let Some(pu) = parent_union {
                if pu.contains(&entry.oid) {
                    continue;
                }
            }
            let child_tree_depth = depth + 1;
            collect_tree_objects_filtered(
                repo,
                entry.oid,
                &path,
                child_tree_depth,
                false,
                parent_union,
                parent_commit_trees,
                tree_state,
                emitted,
                result,
                omitted,
                missing,
                missing_seen,
                filter,
                filter_provided,
                missing_action,
                sparse_lines,
                skip_trees_for_type_filter,
                omit_object_paths,
                packed_set,
                collect_tree_omits,
                tree_omit_set,
                combine_states,
            )?;
        } else {
            if let Some(parents) = parent_commit_trees {
                if entry_unchanged_in_all_parents(
                    repo, tree_state, parents, prefix, &name, entry.oid,
                )? {
                    continue;
                }
            } else if let Some(pu) = parent_union {
                if pu.contains(&entry.oid) {
                    continue;
                }
            }
            if needs_child_read_info {
                let child_info = match repo.odb.read_info(&entry.oid) {
                    Ok(info) => info,
                    Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                        if missing_action.reports_missing() && missing_seen.insert(entry.oid) {
                            missing.push(entry.oid);
                        }
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                if child_info.kind == ObjectKind::Blob {
                    let sparse_blob = sparse_filter_includes_path(repo, &path, sparse_lines);
                    let blob_bits = match filter {
                        None => ListFilterBits {
                            mark_seen: true,
                            do_show: true,
                            skip_tree: false,
                        },
                        Some(f) => {
                            if explicit_root && !filter_provided {
                                ListFilterBits {
                                    mark_seen: true,
                                    do_show: true,
                                    skip_tree: false,
                                }
                            } else {
                                match f {
                                    ObjectFilter::Combine(_) => filter_object_bits_blob(
                                        f,
                                        entry.oid,
                                        child_info.size,
                                        depth,
                                        tree_omit_set,
                                        collect_tree_omits,
                                        combine_states.as_mut().map(Vec::as_mut_slice),
                                    ),
                                    _ => filter_object_bits_blob(
                                        f,
                                        entry.oid,
                                        child_info.size,
                                        depth,
                                        tree_omit_set,
                                        collect_tree_omits,
                                        None,
                                    ),
                                }
                            }
                        }
                    };
                    let blob_included = blob_bits.do_show && sparse_blob;
                    if !blob_included {
                        omitted.push(entry.oid);
                    } else if blob_bits.mark_seen
                        && emitted.insert(entry.oid)
                        && !packed_set.is_some_and(|p| p.contains(&entry.oid))
                    {
                        let out_path = if omit_object_paths {
                            String::new()
                        } else {
                            path.clone()
                        };
                        result.push((entry.oid, out_path));
                    }
                } else {
                    if emitted.contains(&entry.oid) {
                        return Err(Error::CorruptObject(format!(
                            "object {} is not a blob",
                            entry.oid
                        )));
                    }
                    if emitted.insert(entry.oid) {
                        result.push((entry.oid, path));
                    }
                }
            } else if emitted.insert(entry.oid)
                && !packed_set.is_some_and(|p| p.contains(&entry.oid))
            {
                let out_path = if omit_object_paths {
                    String::new()
                } else {
                    path.clone()
                };
                result.push((entry.oid, out_path));
            }
        }
    }

    if let (Some(ObjectFilter::Combine(_)), Some(states)) = (filter, combine_states.as_mut()) {
        for st in states.iter_mut() {
            if st.is_skipping_tree && st.skip_tree_oid == Some(tree_oid) {
                st.is_skipping_tree = false;
                st.skip_tree_oid = None;
            }
        }
    }

    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn collect_root_object(
    repo: &Repository,
    root: &RootObject,
    tree_state: &mut TreeWalkState,
    emitted: &mut HashSet<ObjectId>,
    result: &mut Vec<(ObjectId, String)>,
    omitted: &mut Vec<ObjectId>,
    missing: &mut Vec<ObjectId>,
    missing_seen: &mut HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    filter_provided: bool,
    missing_action: MissingAction,
    sparse_lines: Option<&[String]>,
    skip_trees_for_type_filter: bool,
    omit_object_paths: bool,
    packed_set: Option<&HashSet<ObjectId>>,
    collect_tree_omits: bool,
    tree_omit_set: &mut Option<HashSet<ObjectId>>,
    combine_states: &mut Option<Vec<CombineSubState>>,
) -> Result<()> {
    if let Some(tag_oid) = root.wrap_with_tag {
        if root.expected_kind != Some(ExpectedObjectKind::Commit) {
            let show_tag = match filter {
                None => true,
                Some(f) => f.includes_commit_or_tag_object(ObjectKind::Tag),
            };
            if show_tag && emitted.insert(tag_oid) {
                result.push((tag_oid, "tag".to_owned()));
            }
        }
    }

    let object = match repo.odb.read(&root.oid) {
        Ok(object) => object,
        Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
            if missing_action.reports_missing() && missing_seen.insert(root.oid) {
                missing.push(root.oid);
            }
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    if let Some(expected) = root.expected_kind {
        if !expected.matches(object.kind) {
            return Err(Error::CorruptObject(format!(
                "object {} is not a {}",
                root.input,
                expected.as_str()
            )));
        }
    }

    match object.kind {
        ObjectKind::Commit => {
            if emitted.insert(root.oid) {
                result.push((root.oid, "\0".to_owned()));
            }
            if let Some(tag_oid) = root.wrap_with_tag {
                let show_tag = match filter {
                    None => true,
                    Some(f) => f.includes_commit_or_tag_object(ObjectKind::Tag),
                };
                if show_tag && emitted.insert(tag_oid) {
                    result.push((tag_oid, "tag".to_owned()));
                }
            }
            let commit = parse_commit(&object.data)?;
            // See the note in `collect_objects_segmented`: the parent_union short-circuit must be
            // disabled when a filter is active so that trees are visited per commit exactly like
            // Git, preserving `tree:`/`combine:` skip-tree semantics and trace output.
            let parent_trees = if filter.is_some() {
                None
            } else {
                Some(commit_parent_trees(
                    repo,
                    &commit.parents,
                    missing_action,
                    missing,
                    missing_seen,
                )?)
            };
            collect_tree_objects_filtered(
                repo,
                commit.tree,
                "",
                0,
                false,
                None,
                parent_trees.as_deref(),
                tree_state,
                emitted,
                result,
                omitted,
                missing,
                missing_seen,
                filter,
                filter_provided,
                missing_action,
                sparse_lines,
                skip_trees_for_type_filter,
                omit_object_paths,
                packed_set,
                collect_tree_omits,
                tree_omit_set,
                combine_states,
            )?;
        }
        ObjectKind::Tree => {
            let root_path = root.root_path.as_deref().unwrap_or("");
            collect_tree_objects_filtered(
                repo,
                root.oid,
                root_path,
                0,
                true,
                None,
                None,
                tree_state,
                emitted,
                result,
                omitted,
                missing,
                missing_seen,
                filter,
                filter_provided,
                missing_action,
                sparse_lines,
                skip_trees_for_type_filter,
                omit_object_paths,
                packed_set,
                collect_tree_omits,
                tree_omit_set,
                combine_states,
            )?;
        }
        ObjectKind::Blob => {
            let path_for_sparse = root.root_path.as_deref().unwrap_or("");
            let sparse_blob = sparse_filter_includes_path(repo, path_for_sparse, sparse_lines);
            let blob_bits = match filter {
                None => ListFilterBits {
                    mark_seen: true,
                    do_show: true,
                    skip_tree: false,
                },
                Some(f) => {
                    if !filter_provided {
                        ListFilterBits {
                            mark_seen: true,
                            do_show: true,
                            skip_tree: false,
                        }
                    } else {
                        match f {
                            ObjectFilter::Combine(_) => filter_object_bits_blob(
                                f,
                                root.oid,
                                object.data.len() as u64,
                                0,
                                tree_omit_set,
                                collect_tree_omits,
                                combine_states.as_mut().map(Vec::as_mut_slice),
                            ),
                            _ => filter_object_bits_blob(
                                f,
                                root.oid,
                                object.data.len() as u64,
                                0,
                                tree_omit_set,
                                collect_tree_omits,
                                None,
                            ),
                        }
                    }
                }
            };
            let blob_included = blob_bits.do_show && sparse_blob;
            if !blob_included {
                if blob_bits.mark_seen {
                    omitted.push(root.oid);
                }
                return Ok(());
            }
            if packed_set.is_some_and(|p| p.contains(&root.oid)) {
                return Ok(());
            }
            if blob_bits.mark_seen && !emitted.insert(root.oid) {
                return Ok(());
            }
            let out_path = if omit_object_paths {
                String::new()
            } else {
                path_for_sparse.to_owned()
            };
            result.push((root.oid, out_path));
        }
        ObjectKind::Tag => {
            let tag = parse_tag(&object.data)?;
            let expected_kind =
                ExpectedObjectKind::from_tag_type(&tag.object_type).ok_or_else(|| {
                    Error::CorruptObject(format!(
                        "object {} has unsupported tag type '{}'",
                        root.input, tag.object_type
                    ))
                })?;
            let nested = RootObject {
                oid: tag.object,
                input: root.input.clone(),
                expected_kind: Some(expected_kind),
                root_path: None,
                wrap_with_tag: None,
            };
            collect_root_object(
                repo,
                &nested,
                tree_state,
                emitted,
                result,
                omitted,
                missing,
                missing_seen,
                filter,
                filter_provided,
                missing_action,
                sparse_lines,
                skip_trees_for_type_filter,
                omit_object_paths,
                packed_set,
                collect_tree_omits,
                tree_omit_set,
                combine_states,
            )?;
        }
    }

    Ok(())
}

/// Collect reachable objects in commit order: objects for each commit are emitted
/// right after that commit, rather than all objects after all commits.
/// Returns (objects, omitted, per_commit_counts).
#[expect(clippy::too_many_arguments)]
fn collect_reachable_objects_in_commit_order(
    repo: &Repository,
    _graph: &mut CommitGraph<'_>,
    commits: &[ObjectId],
    object_roots: &[RootObject],
    tip_annotated_tags: &HashMap<ObjectId, ObjectId>,
    filter: Option<&ObjectFilter>,
    filter_provided: bool,
    missing_action: MissingAction,
    sparse_lines: Option<&[String]>,
    skip_trees_for_type_filter: bool,
    omit_object_paths: bool,
    packed_set: Option<&HashSet<ObjectId>>,
    collect_tree_omits: bool,
) -> Result<ReachableObjectsInOrder> {
    let mut tree_state = TreeWalkState::new();
    let mut top_tree_omit =
        walk_needs_top_tree_omit_set(filter, collect_tree_omits).then(HashSet::<ObjectId>::new);
    let mut combine_states = CombineSubState::prepare_sub_states(filter, collect_tree_omits);
    let mut emitted = HashSet::new();
    let mut result = Vec::new();
    let mut omitted = Vec::new();
    let mut missing = Vec::new();
    let mut missing_seen = HashSet::new();
    let mut counts = Vec::with_capacity(commits.len());
    for &commit_oid in commits {
        let commit = match load_commit(repo, commit_oid) {
            Ok(commit) => commit,
            Err(Error::ObjectNotFound(_)) if missing_action != MissingAction::Error => {
                if missing_action.reports_missing() && missing_seen.insert(commit_oid) {
                    missing.push(commit_oid);
                }
                counts.push(0);
                continue;
            }
            Err(err) => return Err(err),
        };
        let before = result.len();
        if let Some(&tag_oid) = tip_annotated_tags.get(&commit_oid) {
            if emitted.insert(tag_oid) {
                result.push((tag_oid, "tag".to_owned()));
            }
        }
        // Match Git `rev-list --objects`: walk each commit's tree in full traversal order and rely
        // on `emitted` for OID de-duplication. Do not subtract parent reachability here — that would
        // skip blobs that still belong after this commit's tree line (t6100-rev-list-in-order).
        collect_tree_objects_filtered(
            repo,
            commit.tree,
            "",
            0,
            false,
            None,
            None,
            &mut tree_state,
            &mut emitted,
            &mut result,
            &mut omitted,
            &mut missing,
            &mut missing_seen,
            filter,
            filter_provided,
            missing_action,
            sparse_lines,
            skip_trees_for_type_filter,
            omit_object_paths,
            packed_set,
            collect_tree_omits,
            &mut top_tree_omit,
            &mut combine_states,
        )?;
        counts.push(result.len() - before);
    }

    for root in object_roots {
        collect_root_object(
            repo,
            root,
            &mut tree_state,
            &mut emitted,
            &mut result,
            &mut omitted,
            &mut missing,
            &mut missing_seen,
            filter,
            filter_provided,
            missing_action,
            sparse_lines,
            skip_trees_for_type_filter,
            omit_object_paths,
            packed_set,
            collect_tree_omits,
            &mut top_tree_omit,
            &mut combine_states,
        )?;
    }

    Ok((result, omitted, missing, counts))
}

/// Collect OIDs of all objects in packs that have a `.keep` file.
fn kept_object_ids(repo: &Repository) -> Result<HashSet<ObjectId>> {
    repo.odb.primary()?.packed_objects().kept_pack_object_ids()
}

/// Like [`flatten_tree`] but also carries each blob's file mode, so callers can detect
/// mode-only changes (e.g. an executable-bit flip) when deciding whether a path changed.
/// Git's tree diff treats a path as modified when *either* the object id or the (canonical)
/// mode differs; the OID-only [`flatten_tree`] cannot see a mode-only change.
fn flatten_tree_with_mode(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
) -> Result<Vec<(String, (ObjectId, u32))>> {
    let mut result = Vec::new();
    let object = match repo.odb.read(&tree_oid) {
        Ok(o) => o,
        Err(_) => return Ok(result),
    };
    if object.kind != ObjectKind::Tree {
        return Ok(result);
    }
    let entries = parse_tree(&object.data)?;
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name).to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let child_info = match repo.odb.read_info(&entry.oid) {
            Ok(info) => info,
            Err(Error::ObjectNotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        if child_info.kind == ObjectKind::Tree {
            result.extend(flatten_tree_with_mode(repo, entry.oid, &path)?);
        } else {
            result.push((path, (entry.oid, canon_tree_mode(entry.mode))));
        }
    }
    Ok(result)
}

/// Canonicalize a tree entry mode for path-change comparison, mirroring Git's `canon_mode`:
/// regular files collapse to `0100644`/`0100755`, symlinks to `0120000`, gitlinks to
/// `0160000`. This keeps mode comparison stable regardless of incidental low-bit noise.
fn canon_tree_mode(mode: u32) -> u32 {
    const S_IFMT: u32 = 0o170000;
    const S_IFREG: u32 = 0o100000;
    const S_IFLNK: u32 = 0o120000;
    const S_IFGITLINK: u32 = 0o160000;
    match mode & S_IFMT {
        S_IFREG => {
            if mode & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            }
        }
        S_IFLNK => 0o120000,
        S_IFGITLINK => 0o160000,
        _ => mode,
    }
}

/// Compute merge bases between two commits.
pub fn merge_bases(
    repo: &Repository,
    a: ObjectId,
    b: ObjectId,
    first_parent_only: bool,
) -> Result<Vec<ObjectId>> {
    let mut graph = CommitGraph::new(repo, first_parent_only);
    let ancestors_a = walk_closure(&mut graph, &[a])?;
    let ancestors_b = walk_closure(&mut graph, &[b])?;
    let common: HashSet<ObjectId> = ancestors_a.intersection(&ancestors_b).copied().collect();
    if common.is_empty() {
        return Ok(Vec::new());
    }
    // Merge bases: common ancestors not dominated by other common ancestors
    let mut bases = Vec::new();
    for &c in &common {
        let is_dominated = common.iter().any(|&other| {
            if other == c {
                return false;
            }
            let other_anc = walk_closure(&mut graph, &[other]).unwrap_or_default();
            other_anc.contains(&c)
        });
        if !is_dominated {
            bases.push(c);
        }
    }
    if bases.is_empty() {
        let mut sorted: Vec<_> = common.into_iter().collect();
        sorted.sort_by_key(|b| std::cmp::Reverse(graph.committer_time(*b)));
        bases.push(sorted[0]);
    }
    Ok(bases)
}
