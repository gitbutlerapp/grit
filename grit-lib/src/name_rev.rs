//! Name-rev: name commits relative to refs.
//!
//! This module implements the algorithm behind `git name-rev`: given a set of
//! refs it walks backwards through the commit graph and assigns each reachable
//! commit a human-readable name derived from the nearest ref tip.
//!
//! # Name format
//!
//! - Directly pointed to by a ref: `<refname>` (e.g. `tags/v1.0`, `main`)
//! - Tag object pointing at the commit: `<refname>^0`
//! - N first-parent hops from a tip: `<refname>~N`
//! - Second or later parent of a merge: `<refname>^2`, `<refname>~3^2`, etc.

use crate::error::{Error, Result};
use crate::ident::committer_unix_seconds_for_ordering;
use crate::objects::{parse_commit, parse_tag, CommitData, ObjectId, ObjectKind};
use crate::refs;
use crate::repo::Repository;
use std::collections::{HashMap, VecDeque};

/// How many first-parent generations are preferred over a single merge hop.
const MERGE_TRAVERSAL_WEIGHT: u32 = 65_535;

/// Internal per-commit naming record.
#[derive(Clone, Debug)]
struct RevName {
    /// The ref-derived name for the tip of this naming chain.
    ///
    /// This is stored verbatim (e.g. `"main"` or `"tags/v1.0^0"`); the final
    /// `~N` generation suffix is appended only at display time.
    tip_name: String,
    /// Tagger / committer date of the originating ref tip (Unix timestamp).
    taggerdate: i64,
    /// Number of first-parent hops from the naming tip to this commit.
    generation: u32,
    /// Weighted hop count used for name-quality comparisons.
    distance: u32,
    /// Whether the originating ref is under `refs/tags/`.
    from_tag: bool,
}

/// Options that control which refs participate in naming.
#[derive(Debug, Default, Clone)]
pub struct NameRevOptions {
    /// When true, only `refs/tags/` refs are used.
    pub tags_only: bool,
    /// When true, tag ref names are shortened to bare names (strips `tags/`).
    ///
    /// In Git this is enabled by `--tags --name-only` together.  Pass this as
    /// `tags_only && name_only` from the CLI layer.
    pub shorten_tags: bool,
    /// Glob patterns — only refs whose path (or any sub-path) matches at least
    /// one pattern are used.  Empty means "all refs pass".
    pub ref_filters: Vec<String>,
    /// Glob patterns — refs whose path (or any sub-path) matches are excluded.
    pub exclude_filters: Vec<String>,
}

/// Build the complete `ObjectId → display-name` mapping for all commits
/// reachable from the applicable refs in `repo`.
///
/// The returned map contains every commit that could be named given the
/// supplied `options`.  Commits not reachable from any passing ref are absent
/// from the map.
///
/// # Errors
///
/// Returns [`Error::Io`] for filesystem problems and [`Error::CorruptObject`]
/// for malformed objects.
pub fn build_name_map(
    repo: &Repository,
    options: &NameRevOptions,
) -> Result<HashMap<ObjectId, String>> {
    let tips = collect_tips(repo, options)?;
    let mut names: HashMap<ObjectId, RevName> = HashMap::new();

    let mut commit_cache: HashMap<ObjectId, CommitData> = HashMap::new();

    for tip in &tips {
        let Some(commit_oid) = tip.commit_oid else {
            continue;
        };
        if tip.deref {
            names.insert(
                tip.object_oid,
                RevName {
                    tip_name: tip.display_name.clone(),
                    taggerdate: tip.taggerdate,
                    generation: 0,
                    distance: 0,
                    from_tag: tip.from_tag,
                },
            );
        }
        name_from_tip(
            repo,
            &mut names,
            &mut commit_cache,
            commit_oid,
            &tip.display_name,
            tip.taggerdate,
            tip.from_tag,
            tip.deref,
        )?;
    }

    Ok(names
        .into_iter()
        .map(|(oid, name)| (oid, format_name(&name)))
        .collect())
}

/// Return the display name for a named commit.
///
/// - `generation == 0`: returns `tip_name` as-is.
/// - `generation > 0`: returns `<tip_name>~<generation>` with any trailing
///   `"^0"` stripped from `tip_name` first.
fn format_name(name: &RevName) -> String {
    if name.generation == 0 {
        return name.tip_name.clone();
    }
    let base = name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name);
    format!("{}~{}", base, name.generation)
}

/// Compute the effective comparison distance for a (distance, generation) pair.
///
/// First-parent chains add [`MERGE_TRAVERSAL_WEIGHT`] to make merge-traversal
/// paths look "closer" (lower distance) than deep first-parent chains when
/// generation > 0.
fn effective_distance(distance: u32, generation: u32) -> u32 {
    distance.saturating_add(if generation > 0 {
        MERGE_TRAVERSAL_WEIGHT
    } else {
        0
    })
}

/// Return `true` when the proposed name is strictly better than the existing one.
///
/// Preference order:
/// 1. Tags over non-tags.
/// 2. Smaller effective distance.
/// 3. Older tagger/committer date (more stable refs first).
fn is_better_name(
    existing: &RevName,
    taggerdate: i64,
    generation: u32,
    distance: u32,
    from_tag: bool,
) -> bool {
    let existing_eff = effective_distance(existing.distance, existing.generation);
    let new_eff = effective_distance(distance, generation);

    // Tags beat non-tags.
    if from_tag && existing.from_tag {
        return existing_eff > new_eff;
    }
    if existing.from_tag != from_tag {
        return from_tag;
    }

    // Both non-tags: prefer smaller effective distance.
    if existing_eff != new_eff {
        return existing_eff > new_eff;
    }

    // Tiebreak by older date (more stable / earlier tag).
    if existing.taggerdate != taggerdate {
        return existing.taggerdate > taggerdate;
    }

    false
}

/// Derive the `tip_name` for a non-first parent.
///
/// Uses the *current* commit's name (not the parent's) so that e.g. the second
/// parent of `main~3` is named `main~3^2`.
fn get_parent_name(current: &RevName, parent_number: u32) -> String {
    let base = current
        .tip_name
        .strip_suffix("^0")
        .unwrap_or(&current.tip_name);
    if current.generation > 0 {
        format!("{}~{}^{}", base, current.generation, parent_number)
    } else {
        format!("{}^{}", base, parent_number)
    }
}

/// Walk backwards from `start_oid`, assigning names to all reachable ancestors.
///
/// Uses an explicit stack to avoid deep recursion on long histories.
#[expect(clippy::too_many_arguments)]
fn name_from_tip(
    repo: &Repository,
    names: &mut HashMap<ObjectId, RevName>,
    commit_cache: &mut HashMap<ObjectId, CommitData>,
    start_oid: ObjectId,
    tip_name: &str,
    taggerdate: i64,
    from_tag: bool,
    deref: bool,
) -> Result<()> {
    let actual_tip_name = if deref {
        format!("{}^0", tip_name)
    } else {
        tip_name.to_owned()
    };

    // Seed the starting commit.
    let should_start = match names.get(&start_oid) {
        None => true,
        Some(existing) => is_better_name(existing, taggerdate, 0, 0, from_tag),
    };
    if !should_start {
        return Ok(());
    }
    names.insert(
        start_oid,
        RevName {
            tip_name: actual_tip_name,
            taggerdate,
            generation: 0,
            distance: 0,
            from_tag,
        },
    );

    // Stack-based DFS; first parent is processed first.
    let mut stack: Vec<ObjectId> = vec![start_oid];

    while let Some(oid) = stack.pop() {
        let current = match names.get(&oid) {
            Some(n) => n.clone(),
            None => continue,
        };

        let commit = match load_commit_cached(repo, commit_cache, oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Clone parents out so we can release the borrow.
        let parents = commit.parents.clone();

        let mut to_push: Vec<ObjectId> = Vec::new();

        for (idx, parent_oid) in parents.iter().enumerate() {
            let parent_number = (idx + 1) as u32;

            let (parent_gen, parent_dist) = if parent_number > 1 {
                (
                    0u32,
                    current.distance.saturating_add(MERGE_TRAVERSAL_WEIGHT),
                )
            } else {
                (
                    current.generation.saturating_add(1),
                    current.distance.saturating_add(1),
                )
            };

            let should_update = match names.get(parent_oid) {
                None => true,
                Some(existing) => {
                    is_better_name(existing, taggerdate, parent_gen, parent_dist, from_tag)
                }
            };

            if should_update {
                let parent_tip_name = if parent_number > 1 {
                    get_parent_name(&current, parent_number)
                } else {
                    current.tip_name.clone()
                };

                names.insert(
                    *parent_oid,
                    RevName {
                        tip_name: parent_tip_name,
                        taggerdate,
                        generation: parent_gen,
                        distance: parent_dist,
                        from_tag,
                    },
                );
                to_push.push(*parent_oid);
            }
        }

        for parent in to_push.into_iter().rev() {
            stack.push(parent);
        }
    }

    Ok(())
}

/// A ref tip to be used as a naming source.
struct TipEntry {
    /// The object ID stored directly in the ref.
    object_oid: ObjectId,
    /// Short display name for output (e.g. `"main"`, `"tags/v1.0"`).
    display_name: String,
    /// Peeled commit OID, if the ref ultimately resolves to a commit.
    commit_oid: Option<ObjectId>,
    /// Tagger date (for annotated tags) or committer date.
    taggerdate: i64,
    /// True when the ref is under `refs/tags/`.
    from_tag: bool,
    /// True when the object was a tag and we peeled it to reach the commit.
    deref: bool,
}

/// Collect all ref tips from the repository, applying option filters.
fn collect_tips(repo: &Repository, options: &NameRevOptions) -> Result<Vec<TipEntry>> {
    let all_refs = refs::list_refs(&repo.git_dir, "refs/")?;
    let mut tips: Vec<TipEntry> = Vec::new();

    for (refname, oid) in all_refs {
        if options.tags_only && !refname.starts_with("refs/tags/") {
            continue;
        }

        // Exclude filters.
        if options
            .exclude_filters
            .iter()
            .any(|pat| subpath_matches(&refname, pat))
        {
            continue;
        }

        // Include filters (if specified, at least one must match).
        let can_abbreviate = if !options.ref_filters.is_empty() {
            let mut matched = false;
            let mut subpath_match = false;
            for pat in &options.ref_filters {
                match subpath_match_kind(&refname, pat) {
                    SubpathMatch::Full => matched = true,
                    SubpathMatch::Sub => {
                        matched = true;
                        subpath_match = true;
                    }
                    SubpathMatch::None => {}
                }
            }
            if !matched {
                continue;
            }
            subpath_match
        } else {
            // No filter: can abbreviate if tags_only (and caller also requests name_only,
            // but we handle that at the CLI layer).
            false
        };

        let from_tag = refname.starts_with("refs/tags/");
        let display_name = shorten_refname(&refname, can_abbreviate || options.shorten_tags);

        // Peel the object: follow tag chains until we reach a commit.
        let (commit_oid, taggerdate, deref) = peel_to_commit(repo, oid)?;

        tips.push(TipEntry {
            object_oid: oid,
            display_name,
            commit_oid,
            taggerdate,
            from_tag,
            deref,
        });
    }

    // Sort: tags first (from_tag=true sorts before false), then older taggerdate.
    tips.sort_by(|a, b| {
        let tag_cmp = b.from_tag.cmp(&a.from_tag); // true > false, so b first = tags first
        if tag_cmp != std::cmp::Ordering::Equal {
            return tag_cmp;
        }
        a.taggerdate.cmp(&b.taggerdate)
    });

    Ok(tips)
}

/// Peel an object ID through tag layers to find the underlying commit.
///
/// Returns `(commit_oid, taggerdate, was_dereffed)`.  `taggerdate` is the
/// tagger timestamp from the first (outermost) tag encountered, or the
/// committer timestamp if the object is a plain commit.  `was_dereffed` is
/// true if at least one tag layer was peeled.
fn peel_to_commit(repo: &Repository, mut oid: ObjectId) -> Result<(Option<ObjectId>, i64, bool)> {
    let mut deref = false;
    let mut taggerdate: Option<i64> = None;

    loop {
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => return Ok((None, taggerdate.unwrap_or(0), deref)),
        };

        match obj.kind {
            ObjectKind::Commit => {
                let ts = if let Ok(c) = parse_commit(&obj.data) {
                    parse_signature_time(&c.committer)
                } else {
                    0
                };
                let date = taggerdate.unwrap_or(ts);
                return Ok((Some(oid), date, deref));
            }
            ObjectKind::Tag => {
                let tag = match parse_tag(&obj.data) {
                    Ok(t) => t,
                    Err(_) => return Ok((None, taggerdate.unwrap_or(0), deref)),
                };
                // Record the outermost tag's date.
                if taggerdate.is_none() {
                    taggerdate = Some(tag.tagger.as_deref().map(parse_signature_time).unwrap_or(0));
                }
                oid = tag.object;
                deref = true;
            }
            _ => return Ok((None, taggerdate.unwrap_or(0), deref)),
        }
    }
}

/// Walk all commits reachable from every ref and return their OIDs.
///
/// Used by `--all` mode to enumerate commits that should be named and printed.
///
/// # Errors
///
/// Returns object or I/O errors on failure.
pub fn all_reachable_commits(repo: &Repository) -> Result<Vec<ObjectId>> {
    let all_refs = refs::list_refs(&repo.git_dir, "refs/")?;
    let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    for (_, oid) in all_refs {
        // Peel to commit if needed.
        let (commit_oid, _, _) = peel_to_commit(repo, oid)?;
        if let Some(c) = commit_oid {
            if seen.insert(c) {
                queue.push_back(c);
            }
        }
    }

    while let Some(oid) = queue.pop_front() {
        let commit = match load_commit(repo, oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for parent in commit.parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }

    let mut result: Vec<ObjectId> = seen.into_iter().collect();
    result.sort();
    Ok(result)
}

/// Shorten a fully-qualified ref name to its display form.
///
/// - `can_abbreviate = true`: strip `refs/heads/`, `refs/tags/`, or `refs/`
///   (mimics Git's unambiguous-ref shortening for `--tags --name-only`).
/// - `can_abbreviate = false`: strip only `refs/heads/`; other prefixes lose
///   only the leading `refs/` component, preserving `tags/`, `remotes/`, etc.
fn shorten_refname(refname: &str, can_abbreviate: bool) -> String {
    if can_abbreviate {
        if let Some(rest) = refname.strip_prefix("refs/heads/") {
            return rest.to_owned();
        }
        if let Some(rest) = refname.strip_prefix("refs/tags/") {
            return rest.to_owned();
        }
        if let Some(rest) = refname.strip_prefix("refs/") {
            return rest.to_owned();
        }
        return refname.to_owned();
    }
    // Default: strip refs/heads/ only; everything else keeps its sub-namespace.
    if let Some(rest) = refname.strip_prefix("refs/heads/") {
        return rest.to_owned();
    }
    if let Some(rest) = refname.strip_prefix("refs/") {
        return rest.to_owned();
    }
    refname.to_owned()
}

/// Possible outcomes of matching a ref path against a glob pattern.
#[derive(PartialEq, Eq)]
enum SubpathMatch {
    /// The pattern matched the full path starting at position 0.
    Full,
    /// The pattern matched a sub-path (after a `/`).
    Sub,
    /// No match.
    None,
}

/// Test whether `pattern` matches `path` or any slash-separated suffix of it.
///
/// Returns the kind of match found (full, sub-path, or none), matching
/// Git's `subpath_matches` semantics.
fn subpath_match_kind(path: &str, pattern: &str) -> SubpathMatch {
    // Try the full path first.
    if glob_matches(pattern, path) {
        return SubpathMatch::Full;
    }
    // Try every sub-path (after each '/').
    let mut rest = path;
    while let Some(pos) = rest.find('/') {
        rest = &rest[pos + 1..];
        if glob_matches(pattern, rest) {
            return SubpathMatch::Sub;
        }
    }
    SubpathMatch::None
}

/// Return `true` when the path (or any sub-path) matches the pattern.
fn subpath_matches(path: &str, pattern: &str) -> bool {
    subpath_match_kind(path, pattern) != SubpathMatch::None
}

/// Minimal glob matcher supporting `*` (any sequence) and `?` (any single char).
///
/// All other characters match literally.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &txt)
}

fn glob_match_inner(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some('*'), _) => {
            // '*' matches zero or more characters.
            glob_match_inner(&pat[1..], txt)
                || (!txt.is_empty() && glob_match_inner(pat, &txt[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pat[1..], &txt[1..]),
        (Some('?'), None) => false,
        (Some(p), Some(t)) => p == t && glob_match_inner(&pat[1..], &txt[1..]),
        (Some(_), None) => false,
    }
}

/// Parse the Unix timestamp from a Git signature string.
///
/// A signature has the form `"Name <email> <timestamp> <tz>"`.  This returns
/// the second-to-last whitespace-delimited token parsed as an `i64`, or `0`
/// on parse failure.
pub(crate) fn parse_signature_time(sig: &str) -> i64 {
    committer_unix_seconds_for_ordering(sig)
}

/// Load and parse a commit object, using a cache to avoid re-reading.
fn load_commit_cached<'c>(
    repo: &Repository,
    cache: &'c mut HashMap<ObjectId, CommitData>,
    oid: ObjectId,
) -> Result<&'c CommitData> {
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(oid) {
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "object {oid} is not a commit"
            )));
        }
        let commit = parse_commit(&obj.data)?;
        e.insert(commit);
    }
    cache
        .get(&oid)
        .ok_or_else(|| Error::CorruptObject(format!("commit {oid} missing from cache")))
}

/// Load and parse a commit object from the object database.
fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData> {
    let obj = repo.odb.read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    parse_commit(&obj.data)
}

/// Resolve a full 40-hex OID string, an abbreviated OID, or a ref name to an
/// [`ObjectId`].
///
/// Used when the caller passes raw OID strings on the command line.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] when the spec cannot be resolved.
pub fn resolve_oid(repo: &Repository, spec: &str) -> Result<ObjectId> {
    crate::rev_parse::resolve_revision(repo, spec)
}

/// Look up the best name for `oid` in the given map, peeling tags first.
///
/// For non-commit objects (e.g. a tag object), the map may contain an entry
/// under the *underlying* commit OID.  This function tries `oid` directly, and
/// if that misses (and the object is a tag), it peels and retries.
///
/// Returns `None` when no name is available.
///
/// # Errors
///
/// Returns object read errors.
pub fn lookup_name<'m>(
    repo: &Repository,
    name_map: &'m HashMap<ObjectId, String>,
    oid: ObjectId,
) -> Result<Option<&'m String>> {
    // Fast path: direct hit (works for commits and exact-ref matches).
    if let Some(name) = name_map.get(&oid) {
        return Ok(Some(name));
    }

    // For tag objects: peel to commit and try again.
    let obj = match repo.odb.read(&oid) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    if obj.kind == ObjectKind::Tag {
        let (commit_oid, _, _) = peel_to_commit(repo, oid)?;
        if let Some(c) = commit_oid {
            return Ok(name_map.get(&c));
        }
    }
    Ok(None)
}

/// Annotate a line of text: wherever a 40-hex sequence appears whose OID is in
/// `name_map`, append ` (<name>)` after the hex string (or replace the hex
/// string with the name when `name_only` is true).
///
/// Returns the annotated line (including trailing newline if the original had
/// one).
pub fn annotate_line(
    repo: &Repository,
    name_map: &HashMap<ObjectId, String>,
    line: &str,
    name_only: bool,
) -> Result<String> {
    let mut out = String::with_capacity(line.len() + 32);
    let chars: Vec<char> = line.chars().collect();
    let hex_len = 40usize;
    let mut i = 0usize;
    let mut flush_start = 0usize;

    while i + hex_len <= chars.len() {
        // Check if chars[i..i+hex_len] is all hex and the char after is not hex.
        let slice: String = chars[i..i + hex_len].iter().collect();
        let after_is_hex = chars
            .get(i + hex_len)
            .map(|c| c.is_ascii_hexdigit())
            .unwrap_or(false);
        if !after_is_hex && slice.chars().all(|c| c.is_ascii_hexdigit()) {
            // Try to resolve this as an OID.
            if let Ok(oid) = slice.parse::<ObjectId>() {
                if let Ok(Some(name)) = lookup_name(repo, name_map, oid) {
                    // Flush everything before this match.
                    let prefix: String = chars[flush_start..i].iter().collect();
                    out.push_str(&prefix);

                    if name_only {
                        out.push_str(name);
                    } else {
                        out.push_str(&slice);
                        out.push_str(" (");
                        out.push_str(name);
                        out.push(')');
                    }
                    flush_start = i + hex_len;
                    i += hex_len;
                    continue;
                }
            }
        }
        i += 1;
    }

    // Flush remainder.
    let tail: String = chars[flush_start..].iter().collect();
    out.push_str(&tail);
    Ok(out)
}

/// Return a short abbreviated hex string for `oid` (first `len` hex chars).
#[must_use]
pub fn abbrev_oid(oid: ObjectId, len: usize) -> String {
    let hex = oid.to_hex();
    let n = len.clamp(4, 40).min(hex.len());
    hex[..n].to_owned()
}

/// Return all commits reachable from `refs/` sorted by OID (for `--all` mode).
///
/// This is an alias for [`all_reachable_commits`] exposed for the CLI.
pub use self::all_reachable_commits as walk_all_commits;

/// Check if a path exists in the object database (either OID).
pub fn object_exists(repo: &Repository, oid: ObjectId) -> bool {
    repo.odb.exists(&oid)
}
