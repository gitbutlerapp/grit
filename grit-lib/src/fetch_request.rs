//! Resolve explicit fetch refspecs (Git `get_ref_map` / `get_remote_ref` semantics).
//!
//! Command-line refspecs replace configured [`crate::remote::Remote::fetch_refspecs`] for
//! stored destinations, while configured refspecs still opportunistically update matching
//! remote-tracking refs (without duplicating [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) lines).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use thiserror::Error;

use crate::config::ConfigSet;
use crate::objects::ObjectId;
use crate::refspec::{parse_fetch_refspec, RefspecError, RefspecItem};
use crate::remote::{Remote, RemoteError};
use crate::repo_handles::{configured_remote_names, has_remote_section};
use crate::transfer::{match_positive, ref_excluded, MatchedRef};

/// How an explicit fetch refspec source side was resolved against advertised refs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchRefspecSourceKind {
    /// Source was already a full remote ref name.
    ExactRemoteRef,
    /// Expanded via `refs/heads/<short>`.
    DwimHeads,
    /// Expanded via `refs/tags/<short>`.
    DwimTags,
    /// Expanded via `refs/<short>` (e.g. `pull/123/head`).
    DwimRefsPrefix,
    /// Matched the source string literally on the remote.
    DwimLiteral,
    /// Empty source (`HEAD` / `@`).
    Head,
    /// Source is a 40-character object id.
    ObjectId,
}

/// Whether a fetched ref is written to [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchHeadPolicy {
    /// Normal command-line refspec: for-merge line.
    ForMerge,
    /// Opportunistic remote-tracking update: omit from FETCH_HEAD (Git `FETCH_HEAD_IGNORE`).
    Ignore,
}

/// One ref to fetch after resolving explicit refspecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFetchRef {
    /// Remote-side ref name (e.g. `refs/heads/main`, `HEAD`, or a hex object id).
    pub remote_ref: String,
    /// Object id when the source is `HEAD` or a raw object id (not looked up in advertisements).
    pub oid: Option<ObjectId>,
    /// Local ref to update, or `None` when the refspec destination is empty.
    pub local_ref: Option<String>,
    /// Whether the refspec carried a `+` force flag.
    pub force: bool,
    /// Whether this ref appears in [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion).
    pub fetch_head: FetchHeadPolicy,
    /// How the source side was resolved against advertised refs.
    pub source_kind: FetchRefspecSourceKind,
}

/// Outcome of [`FetchRefspecPlan::resolve`].
#[derive(Debug, Clone)]
pub enum FetchRefspecPlan {
    /// Wildcard / configured refspecs only: match positive refspec items over all remote refs.
    Pattern {
        /// Positive refspec items to match against every advertised ref.
        positive: Vec<RefspecItem>,
        /// Negative refspec items excluding advertised refs.
        negatives: Vec<RefspecItem>,
    },
    /// Command-line refspecs include at least one explicit (non-wildcard) source.
    Mixed {
        /// Resolved explicit fetch/update pairs in fetch order.
        explicit: Vec<ResolvedFetchRef>,
        /// Wildcard / matching refspecs from the same command line.
        pattern_positive: Vec<RefspecItem>,
        /// Negative refspec items excluding advertised refs.
        negatives: Vec<RefspecItem>,
    },
}

/// Errors while resolving fetch refspecs before transfer.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FetchRefspecError {
    /// A refspec string failed [`parse_fetch_refspec`].
    #[error("invalid refspec '{spec}': {message}")]
    InvalidRefspec {
        /// Original refspec string.
        spec: String,
        /// Parse failure detail.
        message: String,
    },
    /// No advertised remote ref matches the source side of an explicit refspec.
    #[error("remote has no ref matching fetch source '{ref_name}'")]
    SourceNotFound {
        /// User-facing source fragment (e.g. `missing-branch`).
        ref_name: String,
        /// Original refspec string.
        spec: String,
    },
    /// More than one advertised ref matches an ambiguous short source.
    #[error("fetch source '{ref_name}' is ambiguous on the remote")]
    AmbiguousSource {
        /// User-facing source fragment that matched more than one remote ref.
        ref_name: String,
        /// Original refspec string.
        spec: String,
    },
}

impl From<RefspecError> for FetchRefspecError {
    fn from(e: RefspecError) -> Self {
        match e {
            RefspecError::Invalid(s) => Self::InvalidRefspec {
                spec: s.clone(),
                message: s,
            },
        }
    }
}

/// Advertised remote state used while resolving fetch refspecs.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchRemoteAdvertisement<'a> {
    /// Advertised `(name, oid)` pairs (no `HEAD`, no peeled `^{}`).
    pub refs: &'a [(String, ObjectId)],
    /// Tip oid of the remote `HEAD` ref when advertised (even if omitted from `refs`).
    pub head_oid: Option<ObjectId>,
}

/// Remote name and explicit refspec strings after parsing `grit fetch` positionals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchInvocation {
    /// Configured remote name, URL, or filesystem path to fetch from.
    pub remote: String,
    /// Command-line refspecs (`main`, `refs/heads/x:refs/heads/y`, …). Empty uses configured refspecs.
    pub user_refspecs: Vec<String>,
}

/// Resolve the repository argument of fetch, pull, or a ref listing into a [`Remote`].
///
/// `remote_or_url` is already known to name the repository (it is not a refspec), so a
/// configured remote name wins, and anything that looks like a URL, an scp-style
/// `host:path`, or an existing filesystem path becomes an anonymous [`Remote::from_url`].
///
/// # Errors
///
/// - [`RemoteError::NoUrl`] when the remote has a config section but no `url`.
/// - [`RemoteError::UnknownRemote`] when `remote_or_url` is neither configured nor URL-like.
/// - URL parse errors from [`Remote::from_config`] / [`Remote::from_url`].
pub fn resolve_remote_argument(
    config: &ConfigSet,
    remote_or_url: &str,
) -> Result<Remote, RemoteError> {
    if has_remote_section(config, remote_or_url) {
        return Remote::from_config(config, remote_or_url);
    }
    if looks_like_path_or_url(remote_or_url) || remote_or_url.contains(':') {
        return Remote::from_url(remote_or_url);
    }
    Err(RemoteError::UnknownRemote(remote_or_url.to_owned()))
}

/// Classify `grit fetch [<remote>] [<refspec>…]` positionals (no refspec parsing).
///
/// When the CLI supplies a first positional (`remote`), it is always the
/// repository (configured name, URL, scp-style `host:path`, or path) — matching
/// Git. Remaining positionals are refspecs. With no first positional, an initial
/// token that looks like a repository is consumed as the remote; otherwise the
/// default remote is used and every token is a refspec.
#[must_use]
pub fn resolve_fetch_invocation(
    config: &ConfigSet,
    remote: Option<&str>,
    refspecs: &[String],
    default_remote: &str,
) -> FetchInvocation {
    if let Some(name) = remote.filter(|s| !s.is_empty()) {
        return FetchInvocation {
            remote: name.to_owned(),
            user_refspecs: refspecs.to_vec(),
        };
    }

    if refspecs.is_empty() {
        return FetchInvocation {
            remote: default_remote.to_owned(),
            user_refspecs: Vec::new(),
        };
    }

    if is_fetch_remote_token(config, &refspecs[0]) {
        FetchInvocation {
            remote: refspecs[0].clone(),
            user_refspecs: refspecs[1..].to_vec(),
        }
    } else {
        FetchInvocation {
            remote: default_remote.to_owned(),
            user_refspecs: refspecs.to_vec(),
        }
    }
}

fn is_fetch_remote_token(config: &ConfigSet, token: &str) -> bool {
    if configured_remote_names(config).iter().any(|n| n == token) {
        return true;
    }
    if looks_like_path_or_url(token) {
        return true;
    }
    if token.contains(':') && !token.contains(' ') {
        return !looks_like_fetch_refspec(token);
    }
    false
}

/// A URL with a scheme, a repository path, or an existing filesystem path.
///
/// Colon-only forms (`host:path`) are left to the caller: they are a URL in the remote
/// position but may be a `src:dst` refspec in a positional list.
fn looks_like_path_or_url(token: &str) -> bool {
    token.contains("://")
        || token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.ends_with(".git")
        || token.ends_with(".bundle")
        || Path::new(token).exists()
}

/// True when `token` is an explicit `src:dst` fetch refspec, not an scp-style remote URL.
fn looks_like_fetch_refspec(token: &str) -> bool {
    if token.contains('@') {
        return false;
    }
    let Ok(item) = parse_fetch_refspec(token) else {
        return false;
    };
    if item.negative || item.pattern || item.matching {
        return false;
    }
    if !token.contains(':') || item.src.is_none() {
        return false;
    }
    let src = item.src.as_deref().unwrap_or("");
    let dst = item.dst.as_deref().unwrap_or("");
    if src.contains('.') && !src.starts_with("refs/") {
        return false;
    }
    if dst.contains('/') && !dst.starts_with("refs/") {
        return false;
    }
    true
}

/// Inputs for [`FetchRefspecPlan::resolve`].
#[derive(Debug, Clone, Default)]
pub struct FetchRefspecInput {
    /// Refspecs requested for this fetch (command line or caller override).
    pub user_refspecs: Vec<String>,
    /// `remote.<name>.fetch` from config (used for opportunistic tracking updates).
    pub configured_refspecs: Vec<String>,
    /// Short remote name when fetching a configured remote (`origin`).
    pub remote_name: Option<String>,
    /// When true, explicit refspec sources missing from the advertisement are
    /// kept on the plan so a later prune pass can delete stale tracking refs.
    pub allow_missing_explicit_for_prune: bool,
}

fn refspec_item_is_explicit(item: &RefspecItem) -> bool {
    !item.pattern && !item.matching && !item.negative
}

impl FetchRefspecPlan {
    /// Classify refspecs and resolve explicit sources against `remote`.
    ///
    /// # Errors
    ///
    /// Returns [`FetchRefspecError`] when an explicit source cannot be resolved uniquely.
    pub fn resolve(
        input: &FetchRefspecInput,
        remote: FetchRemoteAdvertisement<'_>,
    ) -> Result<Self, FetchRefspecError> {
        let effective = if input.user_refspecs.is_empty() {
            input.configured_refspecs.clone()
        } else {
            input.user_refspecs.clone()
        };

        let mut negatives = Vec::new();
        let mut positive_strings = Vec::new();
        for spec in &effective {
            let item =
                parse_fetch_refspec(spec).map_err(|e| FetchRefspecError::InvalidRefspec {
                    spec: spec.clone(),
                    message: e.to_string(),
                })?;
            if item.negative {
                negatives.push(item);
            } else {
                positive_strings.push(spec.clone());
            }
        }

        let has_explicit = positive_strings.iter().any(|s| {
            parse_fetch_refspec(s)
                .map(|i| refspec_item_is_explicit(&i))
                .unwrap_or(false)
        });

        if !has_explicit {
            let positive = positive_strings
                .iter()
                .map(|s| parse_fetch_refspec(s))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Self::Pattern {
                positive,
                negatives,
            });
        }

        let remote_index: HashMap<&str, ObjectId> =
            remote.refs.iter().map(|(n, o)| (n.as_str(), *o)).collect();

        let mut explicit = Vec::new();
        let mut pattern_positive = Vec::new();

        for spec in &positive_strings {
            let item =
                parse_fetch_refspec(spec).map_err(|e| FetchRefspecError::InvalidRefspec {
                    spec: spec.clone(),
                    message: e.to_string(),
                })?;
            if item.pattern || item.matching {
                pattern_positive.push(item);
                continue;
            }
            let src_raw = item.src.as_deref().unwrap_or("");
            if item.exact_sha1 {
                if !ObjectId::is_full_hex(src_raw) {
                    return Err(FetchRefspecError::InvalidRefspec {
                        spec: spec.clone(),
                        message: "invalid object id".into(),
                    });
                }
                let oid =
                    ObjectId::from_hex(src_raw).map_err(|_| FetchRefspecError::InvalidRefspec {
                        spec: spec.clone(),
                        message: "invalid object id".into(),
                    })?;
                let local_ref = resolve_explicit_destination(item.dst.as_deref(), src_raw);
                explicit.push(ResolvedFetchRef {
                    remote_ref: src_raw.to_owned(),
                    oid: Some(oid),
                    local_ref,
                    force: item.force,
                    fetch_head: FetchHeadPolicy::ForMerge,
                    source_kind: FetchRefspecSourceKind::ObjectId,
                });
                continue;
            }
            let (remote_ref, kind, oid) =
                match resolve_explicit_source(src_raw, &remote_index, remote.head_oid, spec) {
                    Ok(resolved) => resolved,
                    Err(FetchRefspecError::SourceNotFound { .. })
                        if input.allow_missing_explicit_for_prune =>
                    {
                        let remote_ref = explicit_remote_ref_for_prune(src_raw);
                        (remote_ref, FetchRefspecSourceKind::ExactRemoteRef, None)
                    }
                    Err(e) => return Err(e),
                };
            let local_ref = resolve_explicit_destination(item.dst.as_deref(), &remote_ref);
            explicit.push(ResolvedFetchRef {
                remote_ref,
                oid,
                local_ref,
                force: item.force,
                fetch_head: FetchHeadPolicy::ForMerge,
                source_kind: kind,
            });
        }

        append_opportunistic_tracking(
            &mut explicit,
            &input.configured_refspecs,
            input.remote_name.as_deref(),
        );

        Ok(Self::Mixed {
            explicit,
            pattern_positive,
            negatives,
        })
    }

    /// Whether this fetch used explicit (non-wildcard) command-line refspecs.
    #[must_use]
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Mixed { .. })
    }
}

/// Build [`MatchedRef`] rows from a resolved plan and advertised remote refs,
/// using a caller-supplied negative list (for [`FetchOptions::negative_refspecs`](crate::remote::transfer::FetchOptions::negative_refspecs))
/// and optionally skipping explicit sources missing on the remote when
/// `prune_missing_explicit` is set (vanished-ref `--prune` fetch).
pub(crate) fn matched_refs_with_negatives(
    plan: &FetchRefspecPlan,
    remote: FetchRemoteAdvertisement<'_>,
    negatives: &[RefspecItem],
    prune_missing_explicit: bool,
) -> Result<Vec<MatchedRef>, FetchRefspecError> {
    match plan {
        FetchRefspecPlan::Pattern { positive, .. } => {
            pattern_matched_refs(remote.refs, positive, negatives)
        }
        FetchRefspecPlan::Mixed {
            explicit,
            pattern_positive,
            ..
        } => {
            let mut matched = pattern_matched_refs(remote.refs, pattern_positive, negatives)?;
            let remote_index: HashMap<&str, ObjectId> =
                remote.refs.iter().map(|(n, o)| (n.as_str(), *o)).collect();
            for entry in explicit {
                if ref_excluded(&entry.remote_ref, negatives) {
                    continue;
                }
                let oid = if let Some(o) = entry.oid {
                    o
                } else if let Some(o) = remote_index.get(entry.remote_ref.as_str()).copied() {
                    o
                } else if prune_missing_explicit {
                    continue;
                } else {
                    return Err(FetchRefspecError::SourceNotFound {
                        ref_name: entry.remote_ref.clone(),
                        spec: entry.remote_ref.clone(),
                    });
                };
                matched.push(MatchedRef {
                    remote_ref: entry.remote_ref.clone(),
                    local_ref: entry.local_ref.clone(),
                    oid,
                    force: entry.force,
                    is_tag: entry.remote_ref.starts_with("refs/tags/"),
                    dest_is_tag: ref_update_uses_tag_rules(entry.local_ref.as_deref()),
                    advertised_peel: None,
                    fetch_head: entry.fetch_head,
                });
            }
            Ok(matched)
        }
    }
}

fn pattern_matched_refs(
    remote_refs: &[(String, ObjectId)],
    positive: &[RefspecItem],
    negatives: &[RefspecItem],
) -> Result<Vec<MatchedRef>, FetchRefspecError> {
    let mut matched = Vec::new();
    let mut seen = HashSet::new();
    for (name, oid) in remote_refs {
        if ref_excluded(name, negatives) {
            continue;
        }
        if let Some(local_ref) = match_positive(name, positive) {
            let key = (name.clone(), local_ref.clone());
            if seen.insert(key) {
                matched.push(MatchedRef {
                    remote_ref: name.clone(),
                    local_ref: local_ref.clone(),
                    oid: *oid,
                    force: crate::transfer::refspecs_force(name, positive),
                    is_tag: name.starts_with("refs/tags/"),
                    dest_is_tag: ref_update_uses_tag_rules(local_ref.as_deref()),
                    advertised_peel: None,
                    fetch_head: FetchHeadPolicy::ForMerge,
                });
            }
        }
    }
    Ok(matched)
}

fn append_opportunistic_tracking(
    entries: &mut Vec<ResolvedFetchRef>,
    configured: &[String],
    remote_name: Option<&str>,
) {
    if configured.is_empty() {
        return;
    }
    let Some(remote_name) = remote_name else {
        return;
    };
    if remote_name.contains('/') || remote_name.starts_with('.') {
        return;
    }

    let mut configured_positive = Vec::new();
    for spec in configured {
        if let Ok(item) = parse_fetch_refspec(spec) {
            if !item.negative {
                configured_positive.push(item);
            }
        }
    }
    if configured_positive.is_empty() {
        return;
    }

    let primary_remotes: Vec<String> = entries.iter().map(|e| e.remote_ref.clone()).collect();
    let mut have_local: HashSet<String> =
        entries.iter().filter_map(|e| e.local_ref.clone()).collect();

    for remote_ref in primary_remotes {
        if let Some(local) = match_positive(&remote_ref, &configured_positive) {
            let Some(local) = local else {
                continue;
            };
            if have_local.contains(&local) {
                continue;
            }
            have_local.insert(local.clone());
            entries.push(ResolvedFetchRef {
                remote_ref: remote_ref.to_owned(),
                oid: None,
                local_ref: Some(local),
                force: configured_positive.iter().any(|item| {
                    item.force
                        && item.src.as_deref().is_some_and(|src| {
                            crate::transfer::apply_refspec(
                                src,
                                item.dst.as_deref(),
                                remote_ref.as_str(),
                            )
                            .is_some()
                        })
                }),
                fetch_head: FetchHeadPolicy::Ignore,
                source_kind: FetchRefspecSourceKind::ExactRemoteRef,
            });
        }
    }
}

/// Best-effort remote ref name for an explicit refspec when the source vanished
/// and `--prune` will delete the stale tracking ref.
fn explicit_remote_ref_for_prune(src: &str) -> String {
    if src.is_empty() || src == "HEAD" || src == "@" {
        return "HEAD".to_owned();
    }
    if src.starts_with("refs/") {
        src.to_owned()
    } else {
        format!("refs/heads/{src}")
    }
}

/// Explicit refspecs without a `:dst` side (e.g. `fetch origin topic --prune`) do
/// not carry a tracking destination, so [`crate::transfer::prune_tracking_refs`]
/// would skip them. When configured refspecs map the vanished source to a
/// remote-tracking ref, add positive entries with that destination filled in.
pub(crate) fn supplement_positive_for_explicit_prune(
    positive: &mut Vec<RefspecItem>,
    configured_refspecs: &[String],
    remote_name: Option<&str>,
) {
    let Some(remote_name) = remote_name else {
        return;
    };
    if remote_name.contains('/') || remote_name.starts_with('.') {
        return;
    }
    let mut configured_positive = Vec::new();
    for spec in configured_refspecs {
        if let Ok(item) = parse_fetch_refspec(spec) {
            if !item.negative {
                configured_positive.push(item);
            }
        }
    }
    if configured_positive.is_empty() {
        return;
    }

    let mut extras = Vec::new();
    for item in positive.iter() {
        if item.negative || item.pattern || item.matching {
            continue;
        }
        if item.dst.as_ref().is_some_and(|d| !d.is_empty()) {
            continue;
        }
        let Some(src) = item.src.as_deref() else {
            continue;
        };
        let remote_ref = explicit_remote_ref_for_prune(src);
        let Some(local) =
            crate::transfer::match_positive(&remote_ref, &configured_positive).and_then(|o| o)
        else {
            continue;
        };
        extras.push(RefspecItem {
            dst: Some(local),
            ..item.clone()
        });
    }
    positive.extend(extras);
}

fn resolve_explicit_source(
    src: &str,
    remote: &HashMap<&str, ObjectId>,
    head_oid: Option<ObjectId>,
    spec: &str,
) -> Result<(String, FetchRefspecSourceKind, Option<ObjectId>), FetchRefspecError> {
    if src.is_empty() || src == "HEAD" || src == "@" {
        let Some(oid) = head_oid.filter(|o| !o.is_zero()) else {
            return Err(FetchRefspecError::SourceNotFound {
                ref_name: "HEAD".to_owned(),
                spec: spec.to_owned(),
            });
        };
        return Ok(("HEAD".to_owned(), FetchRefspecSourceKind::Head, Some(oid)));
    }

    if remote.contains_key(src) {
        return Ok((src.to_owned(), FetchRefspecSourceKind::ExactRemoteRef, None));
    }

    let mut candidates: Vec<(u8, String, FetchRefspecSourceKind)> = Vec::new();
    let try_push =
        |candidates: &mut Vec<_>, priority: u8, name: String, kind: FetchRefspecSourceKind| {
            if remote.contains_key(name.as_str()) {
                candidates.push((priority, name, kind));
            }
        };

    try_push(
        &mut candidates,
        0,
        format!("refs/heads/{src}"),
        FetchRefspecSourceKind::DwimHeads,
    );
    try_push(
        &mut candidates,
        1,
        format!("refs/tags/{src}"),
        FetchRefspecSourceKind::DwimTags,
    );
    if src.starts_with("refs/") {
        try_push(
            &mut candidates,
            2,
            src.to_owned(),
            FetchRefspecSourceKind::DwimLiteral,
        );
    } else {
        try_push(
            &mut candidates,
            2,
            format!("refs/{src}"),
            FetchRefspecSourceKind::DwimRefsPrefix,
        );
        try_push(
            &mut candidates,
            3,
            src.to_owned(),
            FetchRefspecSourceKind::DwimLiteral,
        );
    }

    candidates.sort_by_key(|(p, _, _)| *p);
    match candidates.len() {
        0 => Err(FetchRefspecError::SourceNotFound {
            ref_name: src.to_owned(),
            spec: spec.to_owned(),
        }),
        1 => Ok((candidates[0].1.clone(), candidates[0].2, None)),
        _ => {
            let best = candidates[0].0;
            let at_best: Vec<_> = candidates.iter().filter(|(p, _, _)| *p == best).collect();
            if at_best.len() == 1 {
                Ok((at_best[0].1.clone(), at_best[0].2, None))
            } else {
                Err(FetchRefspecError::AmbiguousSource {
                    ref_name: src.to_owned(),
                    spec: spec.to_owned(),
                })
            }
        }
    }
}

/// Whether ref-update rules treat the destination as a tag (no overwrite without `+`).
#[must_use]
pub(crate) fn ref_update_uses_tag_rules(local_ref: Option<&str>) -> bool {
    local_ref.is_some_and(|r| r.starts_with("refs/tags/"))
}

fn resolve_explicit_destination(dst: Option<&str>, _remote_ref: &str) -> Option<String> {
    match dst {
        None | Some("") => None,
        Some(d) if d.starts_with("refs/") => Some(d.to_owned()),
        Some(d)
            if d.starts_with("heads/") || d.starts_with("tags/") || d.starts_with("remotes/") =>
        {
            Some(format!("refs/{d}"))
        }
        Some(d) => Some(format!("refs/heads/{d}")),
    }
}

/// Description line for one explicit-fetch [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) entry.
#[must_use]
pub(crate) fn fetch_head_description(remote_ref: &str, is_tag: bool, source: &str) -> String {
    if remote_ref == "HEAD" {
        return format!("HEAD of {source}");
    }
    if is_tag || remote_ref.starts_with("refs/tags/") {
        let short = remote_ref.strip_prefix("refs/tags/").unwrap_or(remote_ref);
        return format!("tag '{short}' of {source}");
    }
    if let Some(short) = remote_ref.strip_prefix("refs/heads/") {
        return format!("branch '{short}' of {source}");
    }
    format!("'{remote_ref}' of {source}")
}

/// Score for [`refname_match_fetch_dwim`] (higher = preferred).
#[must_use]
pub fn refname_match_fetch_dwim(abbrev: &str, full: &str) -> u32 {
    if abbrev == full {
        return 100;
    }
    if full == format!("refs/heads/{abbrev}") {
        return 90;
    }
    if full == format!("refs/tags/{abbrev}") {
        return 80;
    }
    if full == format!("refs/{abbrev}") {
        return 70;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_refs(names: &[&str]) -> Vec<(String, ObjectId)> {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
        names.iter().map(|n| (n.to_string(), oid)).collect()
    }

    fn adv<'a>(refs: &'a [(String, ObjectId)]) -> FetchRemoteAdvertisement<'a> {
        FetchRemoteAdvertisement {
            refs,
            head_oid: None,
        }
    }

    fn matched_from_plan(
        plan: &FetchRefspecPlan,
        remote: FetchRemoteAdvertisement<'_>,
    ) -> Vec<MatchedRef> {
        let negatives = match plan {
            FetchRefspecPlan::Pattern { negatives, .. }
            | FetchRefspecPlan::Mixed { negatives, .. } => negatives.as_slice(),
        };
        matched_refs_with_negatives(plan, remote, negatives, false).unwrap()
    }

    #[test]
    fn dwim_prefers_heads_over_tags() {
        let refs = remote_refs(&["refs/heads/main", "refs/tags/main"]);
        let (name, kind, _) = resolve_explicit_source(
            "main",
            &refs.iter().map(|(n, o)| (n.as_str(), *o)).collect(),
            None,
            "main",
        )
        .unwrap();
        assert_eq!(name, "refs/heads/main");
        assert_eq!(kind, FetchRefspecSourceKind::DwimHeads);
    }

    #[test]
    fn pull_request_ref_expands_under_refs() {
        let refs = remote_refs(&["refs/pull/123/head"]);
        let (name, kind, _) = resolve_explicit_source(
            "pull/123/head",
            &refs.iter().map(|(n, o)| (n.as_str(), *o)).collect(),
            None,
            "pull/123/head",
        )
        .unwrap();
        assert_eq!(name, "refs/pull/123/head");
        assert_eq!(kind, FetchRefspecSourceKind::DwimRefsPrefix);
    }

    #[test]
    fn head_uses_advertised_head_oid() {
        let oid = ObjectId::from_hex("00000000000000000000000000000000000000ab").unwrap();
        let (name, kind, resolved) =
            resolve_explicit_source("HEAD", &HashMap::new(), Some(oid), "HEAD").unwrap();
        assert_eq!(name, "HEAD");
        assert_eq!(kind, FetchRefspecSourceKind::Head);
        assert_eq!(resolved, Some(oid));
    }

    #[test]
    fn mixed_explicit_and_pattern_refspecs() {
        let refs = remote_refs(&["refs/heads/main", "refs/heads/extra"]);
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["main".into(), "+refs/heads/*:refs/remotes/other/*".into()],
                configured_refspecs: vec![],
                remote_name: Some("origin".into()),
                ..Default::default()
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_from_plan(&plan, adv(&refs));
        assert!(
            matched
                .iter()
                .any(|m| m.local_ref.as_deref() == Some("refs/remotes/other/main")),
            "pattern refspec should map main to other/*"
        );
    }

    #[test]
    fn opportunistic_exact_configured_mapping() {
        let refs = remote_refs(&["refs/heads/main"]);
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["main".into()],
                configured_refspecs: vec!["refs/heads/main:refs/remotes/origin/main".into()],
                remote_name: Some("origin".into()),
                ..Default::default()
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_from_plan(&plan, adv(&refs));
        assert!(
            matched
                .iter()
                .any(|m| m.local_ref.as_deref() == Some("refs/remotes/origin/main")),
            "exact configured mapping should opportunistically update tracking ref"
        );
    }

    #[test]
    fn extra_negative_refspec_excludes_from_wildcard_match() {
        let refs = remote_refs(&["refs/heads/main", "refs/heads/topic", "refs/heads/side"]);
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
                ..Default::default()
            },
            adv(&refs),
        )
        .unwrap();
        let extra = vec![parse_fetch_refspec("^refs/heads/topic").unwrap()];
        let matched = matched_refs_with_negatives(&plan, adv(&refs), &extra, false).unwrap();
        assert!(
            matched
                .iter()
                .any(|m| m.local_ref.as_deref() == Some("refs/remotes/origin/main")),
            "main must match"
        );
        assert!(
            matched
                .iter()
                .any(|m| m.local_ref.as_deref() == Some("refs/remotes/origin/side")),
            "side must match"
        );
        assert!(
            !matched
                .iter()
                .any(|m| m.local_ref.as_deref() == Some("refs/remotes/origin/topic")),
            "negative refspec must drop topic"
        );
    }

    #[test]
    fn explicit_remote_ref_for_prune_keeps_head_symbol() {
        assert_eq!(explicit_remote_ref_for_prune("HEAD"), "HEAD");
        assert_eq!(explicit_remote_ref_for_prune("@"), "HEAD");
        assert_eq!(explicit_remote_ref_for_prune(""), "HEAD");
        assert_eq!(explicit_remote_ref_for_prune("topic"), "refs/heads/topic");
    }

    #[test]
    fn vanished_head_prune_does_not_match_refs_heads_head_branch() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000002").unwrap();
        let refs = vec![("refs/heads/HEAD".to_string(), oid)];
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["HEAD".into()],
                allow_missing_explicit_for_prune: true,
                ..Default::default()
            },
            FetchRemoteAdvertisement {
                refs: &refs,
                head_oid: None,
            },
        )
        .unwrap();
        let matched = matched_refs_with_negatives(
            &plan,
            FetchRemoteAdvertisement {
                refs: &refs,
                head_oid: None,
            },
            &[],
            true,
        )
        .unwrap();
        assert!(
            matched.is_empty(),
            "vanished HEAD must not be confused with a refs/heads/HEAD branch"
        );
    }

    #[test]
    fn supplement_positive_adds_tracking_dst_for_short_explicit_prune() {
        let mut positive = vec![parse_fetch_refspec("topic").unwrap()];
        supplement_positive_for_explicit_prune(
            &mut positive,
            &["+refs/heads/*:refs/remotes/origin/*".into()],
            Some("origin"),
        );
        assert!(
            positive.iter().any(|i| {
                i.src.as_deref() == Some("topic")
                    && i.dst.as_deref() == Some("refs/remotes/origin/topic")
            }),
            "short explicit refspec must gain configured tracking dst for prune"
        );
    }

    #[test]
    fn prune_skips_missing_explicit_source_instead_of_error() {
        let refs = remote_refs(&["refs/heads/main"]);
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["+refs/heads/topic:refs/remotes/origin/topic".into()],
                configured_refspecs: vec![],
                remote_name: None,
                allow_missing_explicit_for_prune: true,
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_refs_with_negatives(&plan, adv(&refs), &[], true).unwrap();
        assert!(
            matched.is_empty(),
            "vanished explicit source with prune must not error or match"
        );
    }

    #[test]
    fn duplicate_remote_ref_two_destinations() {
        let refs = remote_refs(&["refs/heads/main"]);
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["main:refs/heads/a".into(), "main:refs/heads/b".into()],
                ..Default::default()
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_from_plan(&plan, adv(&refs));
        assert_eq!(
            matched
                .iter()
                .filter(|m| m.remote_ref == "refs/heads/main")
                .count(),
            2
        );
    }

    #[test]
    fn object_id_refspec_carries_oid() {
        let oid = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let refs: Vec<(String, ObjectId)> = vec![];
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec![format!("{oid}:refs/heads/imported")],
                ..Default::default()
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_from_plan(&plan, adv(&refs));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].oid, oid);
    }

    #[test]
    fn short_destination_maps_to_refs_heads() {
        assert_eq!(
            resolve_explicit_destination(Some("pr-123"), "refs/pull/123/head").as_deref(),
            Some("refs/heads/pr-123")
        );
    }

    #[test]
    fn opportunistic_tracking_adds_origin_main() {
        let mut entries = vec![ResolvedFetchRef {
            remote_ref: "refs/heads/main".into(),
            oid: None,
            local_ref: Some("refs/heads/local-main".into()),
            force: false,
            fetch_head: FetchHeadPolicy::ForMerge,
            source_kind: FetchRefspecSourceKind::DwimHeads,
        }];
        append_opportunistic_tracking(
            &mut entries,
            &["+refs/heads/*:refs/remotes/origin/*".into()],
            Some("origin"),
        );
        assert!(entries.iter().any(
            |e| e.local_ref.as_deref() == Some("refs/remotes/origin/main")
                && e.fetch_head == FetchHeadPolicy::Ignore
        ));
    }

    #[test]
    fn refname_match_fetch_dwim_ordering() {
        assert!(
            refname_match_fetch_dwim("main", "refs/heads/main")
                > refname_match_fetch_dwim("main", "refs/tags/main")
        );
    }

    #[test]
    fn colon_refspec_is_not_treated_as_remote() {
        use super::{resolve_fetch_invocation, FetchInvocation};
        use crate::config::{ConfigFile, ConfigScope, ConfigSet};

        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            "[remote \"origin\"]\n\turl = /tmp/x.git\n",
            ConfigScope::Local,
        )
        .expect("parse");
        let mut config = ConfigSet::new();
        config.merge(&file);

        assert_eq!(
            resolve_fetch_invocation(&config, None, &["main:refs/heads/feature".into()], "origin"),
            FetchInvocation {
                remote: "origin".into(),
                user_refspecs: vec!["main:refs/heads/feature".into()],
            }
        );
    }

    #[test]
    fn explicit_remote_positional_includes_scp_style_urls() {
        use super::{resolve_fetch_invocation, FetchInvocation};
        use crate::config::ConfigSet;

        let config = ConfigSet::new();
        assert_eq!(
            resolve_fetch_invocation(&config, Some("example.com:repo"), &[], "origin"),
            FetchInvocation {
                remote: "example.com:repo".into(),
                user_refspecs: vec![],
            }
        );
        assert_eq!(
            resolve_fetch_invocation(&config, Some("git@host:org/repo"), &[], "origin"),
            FetchInvocation {
                remote: "git@host:org/repo".into(),
                user_refspecs: vec![],
            }
        );
    }

    #[test]
    fn resolve_remote_argument_prefers_configured_then_url_forms() {
        use crate::config::{ConfigFile, ConfigScope, ConfigSet};
        use crate::remote::RemoteUrl;

        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            "[remote \"origin\"]\n\turl = /tmp/x.git\n[remote \"nourl\"]\n\tfetch = +refs/heads/*:refs/remotes/nourl/*\n",
            ConfigScope::Local,
        )
        .expect("parse");
        let mut config = ConfigSet::new();
        config.merge(&file);

        let origin = resolve_remote_argument(&config, "origin").expect("origin");
        assert_eq!(origin.name.as_deref(), Some("origin"));

        for url in [
            "https://example.com/r.git",
            "./sibling",
            "../sibling",
            "/abs/repo",
            "git@host:org/repo",
            "host:repo",
        ] {
            let remote = resolve_remote_argument(&config, url).expect(url);
            assert_eq!(remote.name, None, "{url}");
        }
        assert!(matches!(
            resolve_remote_argument(&config, "host:repo").map(|r| r.fetch_url().clone()),
            Ok(RemoteUrl::Ssh(_))
        ));

        assert!(matches!(
            resolve_remote_argument(&config, "nourl"),
            Err(RemoteError::NoUrl)
        ));
        let err = resolve_remote_argument(&config, "nosuch").expect_err("unknown");
        assert!(matches!(&err, RemoteError::UnknownRemote(name) if name == "nosuch"));
        assert_eq!(err.to_string(), "unknown remote 'nosuch'");
    }

    #[test]
    fn configured_remote_with_colon_refspec() {
        use super::{resolve_fetch_invocation, FetchInvocation};
        use crate::config::{ConfigFile, ConfigScope, ConfigSet};

        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            "[remote \"origin\"]\n\turl = /tmp/x.git\n",
            ConfigScope::Local,
        )
        .expect("parse");
        let mut config = ConfigSet::new();
        config.merge(&file);

        assert_eq!(
            resolve_fetch_invocation(&config, Some("origin"), &["a:b".into()], "origin"),
            FetchInvocation {
                remote: "origin".into(),
                user_refspecs: vec!["a:b".into()],
            }
        );
    }

    #[test]
    fn resolve_fetch_invocation_remote_vs_refspec() {
        use super::{resolve_fetch_invocation, FetchInvocation};
        use crate::config::{ConfigFile, ConfigScope, ConfigSet};

        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            "[remote \"origin\"]\n\turl = /tmp/x.git\n",
            ConfigScope::Local,
        )
        .expect("parse");
        let mut config = ConfigSet::new();
        config.merge(&file);

        assert_eq!(
            resolve_fetch_invocation(&config, Some("origin"), &["main".into()], "origin"),
            FetchInvocation {
                remote: "origin".into(),
                user_refspecs: vec!["main".into()],
            }
        );
        assert_eq!(
            resolve_fetch_invocation(&config, None, &["main".into()], "origin"),
            FetchInvocation {
                remote: "origin".into(),
                user_refspecs: vec!["main".into()],
            }
        );

        let refspec = "refs/heads/main:refs/remotes/origin/main".to_owned();
        assert_eq!(
            resolve_fetch_invocation(&config, None, &[refspec.clone()], "origin"),
            FetchInvocation {
                remote: "origin".into(),
                user_refspecs: vec![refspec],
            }
        );

        assert!(super::is_fetch_remote_token(
            &config,
            "git@github.com:org/repo.git"
        ));
        assert!(!super::looks_like_fetch_refspec(
            "git@github.com:org/repo.git"
        ));
    }
}
