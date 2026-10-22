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
use crate::repo_handles::configured_remote_names;
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

/// Classify `grit fetch [<remote>] [<refspec>…]` positionals (no refspec parsing).
///
/// The first token is treated as the remote when it names a configured remote,
/// looks like a URL, or is an existing path; otherwise the default remote is
/// used and every token is a refspec.
#[must_use]
pub fn resolve_fetch_invocation(
    config: &ConfigSet,
    remote: Option<&str>,
    refspecs: &[String],
    default_remote: &str,
) -> FetchInvocation {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(name) = remote {
        if !name.is_empty() {
            tokens.push(name.to_owned());
        }
    }
    tokens.extend_from_slice(refspecs);

    if tokens.is_empty() {
        return FetchInvocation {
            remote: default_remote.to_owned(),
            user_refspecs: Vec::new(),
        };
    }

    if is_fetch_remote_token(config, &tokens[0]) {
        FetchInvocation {
            remote: tokens.remove(0),
            user_refspecs: tokens,
        }
    } else {
        FetchInvocation {
            remote: default_remote.to_owned(),
            user_refspecs: tokens,
        }
    }
}

fn is_fetch_remote_token(config: &ConfigSet, token: &str) -> bool {
    if configured_remote_names(config).iter().any(|n| n == token) {
        return true;
    }
    if Path::new(token).exists() {
        return true;
    }
    token.contains("://")
        || token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.ends_with(".git")
        || token.ends_with(".bundle")
        || (token.contains(':') && !token.contains(' ') && !looks_like_fetch_refspec(token))
}

/// True when `token` is an explicit `src:dst` fetch refspec, not an scp-style remote URL.
fn looks_like_fetch_refspec(token: &str) -> bool {
    let Ok(item) = parse_fetch_refspec(token) else {
        return false;
    };
    if item.negative || item.pattern || item.matching {
        return false;
    }
    token.contains(':') && item.src.is_some()
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
                resolve_explicit_source(src_raw, &remote_index, remote.head_oid, spec)?;
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

/// Build [`MatchedRef`] rows from a resolved plan and advertised remote refs.
pub(crate) fn matched_refs_from_plan(
    plan: &FetchRefspecPlan,
    remote: FetchRemoteAdvertisement<'_>,
) -> Result<Vec<MatchedRef>, FetchRefspecError> {
    match plan {
        FetchRefspecPlan::Pattern {
            positive,
            negatives,
        } => pattern_matched_refs(remote.refs, positive, negatives),
        FetchRefspecPlan::Mixed {
            explicit,
            pattern_positive,
            negatives,
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
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_refs_from_plan(&plan, adv(&refs)).unwrap();
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
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_refs_from_plan(&plan, adv(&refs)).unwrap();
        assert!(
            matched
                .iter()
                .any(|m| m.local_ref.as_deref() == Some("refs/remotes/origin/main")),
            "exact configured mapping should opportunistically update tracking ref"
        );
    }

    #[test]
    fn duplicate_remote_ref_two_destinations() {
        let refs = remote_refs(&["refs/heads/main"]);
        let plan = FetchRefspecPlan::resolve(
            &FetchRefspecInput {
                user_refspecs: vec!["main:refs/heads/a".into(), "main:refs/heads/b".into()],
                configured_refspecs: vec![],
                remote_name: None,
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_refs_from_plan(&plan, adv(&refs)).unwrap();
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
                configured_refspecs: vec![],
                remote_name: None,
            },
            adv(&refs),
        )
        .unwrap();
        let matched = matched_refs_from_plan(&plan, adv(&refs)).unwrap();
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
    }
}
