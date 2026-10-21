//! `git tag` listing/filtering core.
//!
//! The `grit tag` command parses argv, resolves identity, and formats output;
//! what lives here is presentation-free tag logic: creating lightweight and
//! annotated tags ([`create_tag`]), listing metadata ([`tag_ref_metadata`]),
//! filter/sort predicates, and `-m`/`-F` message cleanup ([`strip_comments`]).
//! This module performs object and ref writes for create paths and pure
//! computations for list filters.
//!
//! # What this module owns
//!
//! - Tag creation ([`create_tag`], [`CreateTagRequest`], [`TagKind`]) and list
//!   classification ([`tag_ref_metadata`]).
//! - The `--contains` / `--no-contains` / `--points-at` filter predicates
//!   ([`tag_contains`], [`tag_points_at`]).
//! - The `--sort` comparators: numeric `version:refname` ordering
//!   ([`compare_version`] / [`version_segments`]) and the `creatordate` /
//!   `taggerdate` epoch extraction ([`creator_date`] / [`parse_epoch_from_ident`]).
//! - The `-l` glob matcher ([`glob_matches`]) and the `-n<N>` annotation
//!   extraction ([`get_tag_annotation`]).
//! - The `-m`/`-F` message cleanup ([`strip_comments`]).

use std::collections::{HashSet, VecDeque};

use crate::check_ref_format::validate_tag_short_name;
use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, serialize_tag, ObjectId, ObjectKind, TagData};
use crate::refs;
use crate::repo::Repository;

/// Whether a `refs/tags/*` ref is lightweight or an annotated tag object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    /// The ref points directly at a commit (or other object).
    Lightweight,
    /// The ref points at a tag object with message and tagger.
    Annotated,
}

/// Inputs for [`create_tag`].
#[derive(Debug, Clone)]
pub struct CreateTagRequest {
    /// Short tag name (without `refs/tags/`).
    pub name: String,
    /// Object the tag should point at (usually a commit).
    pub target: ObjectId,
    /// When set, build an annotated tag object with this message (after [`strip_comments`]).
    pub message: Option<String>,
    /// Tagger identity line (`Name <email> <epoch> <tz>`); required for annotated tags.
    pub tagger: Option<String>,
}

/// Result of [`create_tag`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTagOutcome {
    /// Short tag name (without `refs/tags/`).
    pub name: String,
    /// Object id stored at `refs/tags/<name>` (commit oid or tag object).
    pub oid: ObjectId,
    /// Whether the tag is lightweight or annotated.
    pub kind: TagKind,
    /// Commit oid when the tag ultimately references a commit.
    pub peeled: Option<ObjectId>,
}

/// Create a lightweight or annotated tag ref.
///
/// # Errors
///
/// Returns [`Error::Message`] when the name is invalid, the tag exists, or a message
/// was given without a tagger line.
pub fn create_tag(repo: &Repository, req: &CreateTagRequest) -> Result<CreateTagOutcome> {
    validate_tag_short_name(&req.name).map_err(|e| Error::Message(e.to_string()))?;
    let tag_ref = format!("refs/tags/{}", req.name);
    if refs::resolve_ref(repo.git_dir(), &tag_ref).is_ok() {
        return Err(Error::Message(format!("tag '{}' already exists", req.name)));
    }

    let target_kind = repo.odb().read_info(&req.target)?.kind;
    let peeled = repo.odb().peel_to_commit(req.target).ok().flatten();

    let wants_annotated = req.message.is_some();
    let (ref_oid, kind) = if wants_annotated {
        let tagger = req
            .tagger
            .as_ref()
            .ok_or_else(|| Error::Message("annotated tag requires a tagger identity".into()))?;
        let message = req
            .message
            .as_deref()
            .map(strip_comments)
            .unwrap_or_default();
        let tag = TagData {
            object: req.target,
            object_type: target_kind.as_str().to_owned(),
            tag: req.name.clone(),
            tagger: Some(tagger.clone()),
            extra_headers: Vec::new(),
            message,
        };
        let bytes = serialize_tag(&tag);
        let tag_oid = repo.odb().write(ObjectKind::Tag, &bytes)?;
        (tag_oid, TagKind::Annotated)
    } else {
        (req.target, TagKind::Lightweight)
    };

    refs::write_ref(repo.git_dir(), &tag_ref, &ref_oid)?;

    Ok(CreateTagOutcome {
        name: req.name.clone(),
        oid: ref_oid,
        kind,
        peeled,
    })
}

/// Classify a tag ref target and peel to a commit when possible.
#[must_use]
pub fn tag_ref_metadata(repo: &Repository, ref_oid: &ObjectId) -> (TagKind, Option<ObjectId>) {
    let Ok(info) = repo.odb().read_info(ref_oid) else {
        return (TagKind::Lightweight, None);
    };
    match info.kind {
        ObjectKind::Commit => (TagKind::Lightweight, Some(*ref_oid)),
        ObjectKind::Tag => (
            TagKind::Annotated,
            repo.odb().peel_to_commit(*ref_oid).ok().flatten(),
        ),
        ObjectKind::Tree | ObjectKind::Blob => (TagKind::Lightweight, None),
    }
}

/// Get annotation text for a tag (up to `n` lines).
///
/// Returns `None` if the tag has no annotation (lightweight) or `n == 0`.
#[must_use]
pub fn get_tag_annotation(repo: &Repository, oid: &ObjectId, n: u32) -> Option<String> {
    if n == 0 {
        return None;
    }
    let obj = repo.odb().read(oid).ok()?;
    let tag = parse_tag(&obj.data).ok()?;
    if tag.message.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = tag
        .message
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(n as usize)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines.join(" "))
}

/// Check if a tag contains (has reachable ancestry from) a commit.
///
/// Peels the tag ref to a commit, then walks ancestors.
#[must_use]
pub fn tag_contains(repo: &Repository, tag_oid: &ObjectId, target: &ObjectId) -> bool {
    let Ok(Some(commit_oid)) = repo.odb().peel_to_commit(*tag_oid) else {
        return false;
    };

    if &commit_oid == target {
        return true;
    }

    // BFS/DFS walk
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(commit_oid);

    while let Some(oid) = queue.pop_front() {
        if !visited.insert(oid) {
            continue;
        }
        if &oid == target {
            return true;
        }
        if let Ok(obj) = repo.odb().read(&oid) {
            if obj.kind == ObjectKind::Commit {
                if let Ok(commit) = parse_commit(&obj.data) {
                    for parent in commit.parents {
                        if !visited.contains(&parent) {
                            queue.push_back(parent);
                        }
                    }
                }
            }
        }
    }

    false
}

/// Check if a tag points at (or peels to) a given object.
#[must_use]
pub fn tag_points_at(repo: &Repository, tag_oid: &ObjectId, target: &ObjectId) -> bool {
    if tag_oid == target {
        return true;
    }
    // Peel through tag objects
    let mut current = *tag_oid;
    for _ in 0..10 {
        let obj = match repo.odb().read(&current) {
            Ok(o) => o,
            Err(_) => return false,
        };
        match obj.kind {
            ObjectKind::Tag => {
                let tag = match parse_tag(&obj.data) {
                    Ok(t) => t,
                    Err(_) => return false,
                };
                if &tag.object == target {
                    return true;
                }
                current = tag.object;
            }
            _ => return false,
        }
    }
    false
}

/// Extract the "creator date" for a tag object.
///
/// For annotated tags, this is the tagger date.  For lightweight tags
/// (which point directly at a commit), this is the committer date.
/// Returns 0 if the date cannot be determined.
#[must_use]
pub fn creator_date(repo: &Repository, oid: &ObjectId) -> i64 {
    let obj = match repo.odb().read(oid) {
        Ok(o) => o,
        Err(_) => return 0,
    };
    match obj.kind {
        ObjectKind::Tag => {
            // Parse tagger line for epoch
            if let Ok(tag) = parse_tag(&obj.data) {
                if let Some(ref tagger) = tag.tagger {
                    return parse_epoch_from_ident(tagger);
                }
            }
            0
        }
        ObjectKind::Commit => {
            if let Ok(commit) = parse_commit(&obj.data) {
                parse_epoch_from_ident(&commit.committer)
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// Extract the epoch timestamp from a Git identity string.
///
/// Format: `Name <email> <epoch> <offset>`
#[must_use]
pub fn parse_epoch_from_ident(ident: &str) -> i64 {
    // The epoch is the second-to-last token
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse().unwrap_or(0)
    } else {
        0
    }
}

/// Compare two tag names as version strings (for `version:refname`).
///
/// Splits each name on `.` and `-` boundaries, comparing numeric segments
/// numerically and non-numeric segments lexicographically.  This matches
/// the behaviour of `git tag --sort=version:refname` (strverscmp-like).
#[must_use]
pub fn compare_version(a: &str, b: &str) -> std::cmp::Ordering {
    let seg_a = version_segments(a);
    let seg_b = version_segments(b);
    for (sa, sb) in seg_a.iter().zip(seg_b.iter()) {
        let ord = match (sa.parse::<u64>(), sb.parse::<u64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => sa.cmp(sb),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    seg_a.len().cmp(&seg_b.len())
}

/// Split a version string into segments at `.` and `-` boundaries.
#[must_use]
pub fn version_segments(s: &str) -> Vec<&str> {
    // Split on `.` and `-` keeping non-empty pieces
    s.split(['.', '-']).filter(|seg| !seg.is_empty()).collect()
}

/// Strip `#comment` lines from a message and normalize whitespace.
///
/// Also: strip trailing whitespace from each line, collapse multiple blank lines
/// to one, and drop leading/trailing blank lines. This is the `whitespace`
/// cleanup `git tag` applies to `-m`/`-F` messages (anything but `--cleanup=verbatim`).
#[must_use]
pub fn strip_comments(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        if line.starts_with('#') {
            continue;
        }
        lines.push(line.trim_end().to_string());
    }
    // Remove leading blank lines
    while lines.first().map(|l| l.is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    // Remove trailing blank lines
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    // Collapse multiple consecutive blank lines to at most one
    let mut result = Vec::new();
    let mut last_blank = false;
    for line in &lines {
        let is_blank = line.is_empty();
        if is_blank && last_blank {
            continue; // skip extra blank lines
        }
        result.push(line.clone());
        last_blank = is_blank;
    }
    result.join("\n") + "\n"
}

/// Simple glob pattern matching for tag names.
///
/// Supports `*` (matches any sequence) and `?` (matches any single character).
#[must_use]
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), name.as_bytes())
}

/// Recursive glob matcher.
fn glob_match_bytes(pat: &[u8], text: &[u8]) -> bool {
    match (pat.first(), text.first()) {
        (None, None) => true,
        (Some(&b'*'), _) => {
            // Skip consecutive stars
            let pat_rest = pat
                .iter()
                .position(|&b| b != b'*')
                .map_or(&pat[pat.len()..], |i| &pat[i..]);
            if pat_rest.is_empty() {
                return true;
            }
            for i in 0..=text.len() {
                if glob_match_bytes(pat_rest, &text[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(&b'?'), Some(_)) => glob_match_bytes(&pat[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => glob_match_bytes(&pat[1..], &text[1..]),
        _ => false,
    }
}
