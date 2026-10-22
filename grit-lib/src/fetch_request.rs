//! Resolve explicit fetch refspecs (Git `get_ref_map` / `get_remote_ref` semantics).
//!
//! Command-line refspecs replace configured [`crate::remote::Remote::fetch_refspecs`] for
//! stored destinations, while configured refspecs still opportunistically update matching
//! remote-tracking refs (without duplicating [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) lines).

use std::collections::{HashMap, HashSet};

use thiserror::Error;

use crate::objects::ObjectId;
use crate::refspec::{parse_fetch_refspec, RefspecError, RefspecItem};
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
    /// Remote-side ref name (e.g. `refs/heads/main`).
    pub remote_ref: String,
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
    /// Wildcard / configured refspecs: match positive refspec items over all remote refs.
    Pattern {
        /// Positive refspec items to match against every advertised ref.
        positive: Vec<RefspecItem>,
        /// Negative refspec items excluding advertised refs.
        negatives: Vec<RefspecItem>,
    },
    /// Explicit refspecs: fixed set of remote/local pairs (plus opportunistic tracking).
    Explicit {
        /// Resolved fetch/update pairs in fetch order.
        entries: Vec<ResolvedFetchRef>,
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
    #[error("couldn't find remote ref {ref_name}")]
    SourceNotFound {
        /// User-facing source fragment (e.g. `missing-branch`).
        ref_name: String,
        /// Original refspec string.
        spec: String,
    },
    /// More than one advertised ref matches an ambiguous short source.
    #[error("src refspec {ref_name} matches more than one")]
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

impl FetchRefspecPlan {
    /// Classify refspecs and resolve explicit sources against `remote_refs`.
    ///
    /// # Errors
    ///
    /// Returns [`FetchRefspecError`] when an explicit source cannot be resolved uniquely.
    pub fn resolve(
        input: &FetchRefspecInput,
        remote_refs: &[(String, ObjectId)],
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
        for spec in &input.user_refspecs {
            if spec.starts_with('^') || spec.starts_with("+^") {
                continue;
            }
        }

        let has_explicit = positive_strings.iter().any(|s| {
            parse_fetch_refspec(s)
                .map(|i| !i.pattern && !i.matching)
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
            remote_refs.iter().map(|(n, o)| (n.as_str(), *o)).collect();

        let mut entries = Vec::new();
        let mut seen_remote: HashSet<String> = HashSet::new();

        for spec in &positive_strings {
            let item =
                parse_fetch_refspec(spec).map_err(|e| FetchRefspecError::InvalidRefspec {
                    spec: spec.clone(),
                    message: e.to_string(),
                })?;
            if item.pattern || item.matching {
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
                let local_ref = resolve_explicit_destination(item.dst.as_deref(), src_raw);
                entries.push(ResolvedFetchRef {
                    remote_ref: src_raw.to_owned(),
                    local_ref,
                    force: item.force,
                    fetch_head: FetchHeadPolicy::ForMerge,
                    source_kind: FetchRefspecSourceKind::ObjectId,
                });
                continue;
            }
            let (remote_ref, kind) = resolve_explicit_source(src_raw, &remote_index, spec)?;
            if !seen_remote.insert(remote_ref.clone()) {
                continue;
            }
            let local_ref = resolve_explicit_destination(item.dst.as_deref(), &remote_ref);
            entries.push(ResolvedFetchRef {
                remote_ref,
                local_ref,
                force: item.force,
                fetch_head: FetchHeadPolicy::ForMerge,
                source_kind: kind,
            });
        }

        append_opportunistic_tracking(
            &mut entries,
            &input.configured_refspecs,
            input.remote_name.as_deref(),
        );

        Ok(Self::Explicit { entries, negatives })
    }

    /// Whether this fetch used explicit (non-wildcard) refspecs.
    #[must_use]
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit { .. })
    }
}

/// Build [`MatchedRef`] rows from a resolved plan and advertised remote refs.
pub(crate) fn matched_refs_from_plan(
    plan: &FetchRefspecPlan,
    remote_refs: &[(String, ObjectId)],
) -> Result<Vec<MatchedRef>, FetchRefspecError> {
    match plan {
        FetchRefspecPlan::Pattern {
            positive,
            negatives,
        } => {
            let mut matched = Vec::new();
            let mut seen = HashSet::new();
            for (name, oid) in remote_refs {
                if ref_excluded(name, negatives) {
                    continue;
                }
                if let Some(local_ref) = match_positive(name, positive) {
                    if seen.insert(name.clone()) {
                        matched.push(MatchedRef {
                            remote_ref: name.clone(),
                            local_ref,
                            oid: *oid,
                            force: crate::transfer::refspecs_force(name, positive),
                            is_tag: name.starts_with("refs/tags/"),
                            advertised_peel: None,
                            fetch_head: FetchHeadPolicy::ForMerge,
                        });
                    }
                }
            }
            Ok(matched)
        }
        FetchRefspecPlan::Explicit { entries, negatives } => {
            let remote_index: HashMap<&str, ObjectId> =
                remote_refs.iter().map(|(n, o)| (n.as_str(), *o)).collect();
            let mut matched = Vec::new();
            for entry in entries {
                if ref_excluded(&entry.remote_ref, negatives) {
                    continue;
                }
                let Some(oid) = remote_index.get(entry.remote_ref.as_str()).copied() else {
                    continue;
                };
                matched.push(MatchedRef {
                    remote_ref: entry.remote_ref.clone(),
                    local_ref: entry.local_ref.clone(),
                    oid,
                    force: entry.force,
                    is_tag: entry.remote_ref.starts_with("refs/tags/"),
                    advertised_peel: None,
                    fetch_head: entry.fetch_head,
                });
            }
            Ok(matched)
        }
    }
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
            if !item.negative && item.pattern {
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
    spec: &str,
) -> Result<(String, FetchRefspecSourceKind), FetchRefspecError> {
    if src.is_empty() || src == "HEAD" || src == "@" {
        if let Some(oid) = remote.get("HEAD").copied() {
            let _ = oid;
        }
        if remote.contains_key("HEAD") {
            return Ok(("HEAD".to_owned(), FetchRefspecSourceKind::Head));
        }
        return Err(FetchRefspecError::SourceNotFound {
            ref_name: "HEAD".to_owned(),
            spec: spec.to_owned(),
        });
    }

    if remote.contains_key(src) {
        return Ok((src.to_owned(), FetchRefspecSourceKind::ExactRemoteRef));
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
        1 => Ok((candidates[0].1.clone(), candidates[0].2)),
        _ => {
            let best = candidates[0].0;
            let at_best: Vec<_> = candidates.iter().filter(|(p, _, _)| *p == best).collect();
            if at_best.len() == 1 {
                Ok((at_best[0].1.clone(), at_best[0].2))
            } else {
                Err(FetchRefspecError::AmbiguousSource {
                    ref_name: src.to_owned(),
                    spec: spec.to_owned(),
                })
            }
        }
    }
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

    #[test]
    fn dwim_prefers_heads_over_tags() {
        let refs = remote_refs(&["refs/heads/main", "refs/tags/main"]);
        let (name, kind) = resolve_explicit_source(
            "main",
            &refs.iter().map(|(n, o)| (n.as_str(), *o)).collect(),
            "main",
        )
        .unwrap();
        assert_eq!(name, "refs/heads/main");
        assert_eq!(kind, FetchRefspecSourceKind::DwimHeads);
    }

    #[test]
    fn pull_request_ref_expands_under_refs() {
        let refs = remote_refs(&["refs/pull/123/head"]);
        let (name, kind) = resolve_explicit_source(
            "pull/123/head",
            &refs.iter().map(|(n, o)| (n.as_str(), *o)).collect(),
            "pull/123/head",
        )
        .unwrap();
        assert_eq!(name, "refs/pull/123/head");
        assert_eq!(kind, FetchRefspecSourceKind::DwimRefsPrefix);
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
    fn pull_request_ref_is_fetch_advertised() {
        assert!(crate::refs::is_valid_fetch_advertised_ref(
            "refs/pull/123/head"
        ));
    }

    #[test]
    fn refname_match_fetch_dwim_ordering() {
        assert!(
            refname_match_fetch_dwim("main", "refs/heads/main")
                > refname_match_fetch_dwim("main", "refs/tags/main")
        );
    }
}
