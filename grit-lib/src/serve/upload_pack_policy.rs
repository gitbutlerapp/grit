//! Upload-pack configuration: capabilities, want validation, filters.

use std::collections::{HashSet, VecDeque};

use super::{AdvertisedRef, Result, ServeError};
use crate::config::ConfigSet;
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;
use crate::upload_filter::{validate_upload_filter_request, UploadFilterError};

/// Policy derived from repository config for one upload-pack session.
pub(crate) struct UploadPackPolicy {
    pub allow_filter: bool,
    pub allow_tip_sha1_in_want: bool,
    pub allow_reachable_sha1_in_want: bool,
    pub ref_in_want: bool,
}

impl UploadPackPolicy {
    pub fn from_config(config: &ConfigSet) -> Self {
        let allow_filter = config
            .get_bool("uploadpack.allowfilter")
            .map(|v| v.unwrap_or(false))
            .unwrap_or(false);
        let allow_tip = config
            .get_bool("uploadpack.allowtipsha1inwant")
            .map(|v| v.unwrap_or(false))
            .unwrap_or(false);
        let allow_reachable = config
            .get_bool("uploadpack.allowreachablesha1inwant")
            .map(|v| v.unwrap_or(false))
            .unwrap_or(false);
        let ref_in_want = config
            .get_bool("uploadpack.allowrefinwant")
            .map(|v| v.unwrap_or(false))
            .unwrap_or(false);
        Self {
            allow_filter,
            allow_tip_sha1_in_want: allow_tip,
            allow_reachable_sha1_in_want: allow_reachable,
            ref_in_want,
        }
    }

    pub fn v0_capability_tokens(&self) -> Vec<&'static str> {
        let mut caps = vec!["shallow", "deepen-since", "deepen-not", "deepen-relative"];
        if self.allow_filter {
            caps.push("filter");
        }
        if self.allow_tip_sha1_in_want {
            caps.push("allow-tip-sha1-in-want");
        }
        if self.allow_reachable_sha1_in_want {
            caps.push("allow-reachable-sha1-in-want");
        }
        caps
    }

    pub fn v2_fetch_features(&self) -> Vec<&'static str> {
        let mut f = vec![
            "thin-pack",
            "no-progress",
            "include-tag",
            "ofs-delta",
            "sideband-all",
            "shallow",
            "deepen-since",
            "deepen-not",
            "deepen-relative",
        ];
        if self.allow_filter {
            f.push("filter");
        }
        if self.ref_in_want {
            f.push("ref-in-want");
        }
        f
    }

    pub fn parse_filter(&self, config: &ConfigSet, spec: &str) -> Result<ObjectFilter> {
        if !self.allow_filter {
            return Err(ServeError::Protocol(
                "filter not supported (uploadpack.allowFilter is false)".into(),
            ));
        }
        validate_upload_filter_request(config, spec).map_err(ServeError::UploadFilter)?;
        ObjectFilter::parse(spec)
            .map_err(|e| ServeError::UploadFilter(UploadFilterError::InvalidFilterSpec(e)))
    }
}

/// Resolve `want-ref` names to OIDs and record them for the `wanted-refs` section.
pub(crate) fn resolve_want_ref(
    repo: &Repository,
    refs: &[AdvertisedRef],
    hidden: &[String],
    name: &str,
) -> Result<ObjectId> {
    if matches!(name, "HEAD" | "refs/HEAD") {
        let head = super::head_info(repo);
        return head
            .oid
            .ok_or_else(|| ServeError::Protocol("unknown ref HEAD".into()));
    }
    let full = if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/{name}")
    };
    if crate::hide_refs::ref_is_hidden(name, &full, hidden) {
        return Err(ServeError::Protocol(format!("unknown ref {name}")));
    }
    let allowed: HashSet<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    if !allowed.contains(full.as_str()) && !allowed.contains(name) {
        return Err(ServeError::Protocol(format!("unknown ref {name}")));
    }
    crate::refs::resolve_ref(&repo.git_dir, &full)
        .or_else(|_| crate::refs::resolve_ref(&repo.git_dir, name))
        .map_err(|_| ServeError::Protocol(format!("unknown ref {name}")))
}

/// Validate `want` OIDs for a filtered fetch (object must exist in the ODB).
pub(crate) fn validate_wants_filtered(repo: &Repository, wants: &[ObjectId]) -> Result<()> {
    for w in wants {
        if !repo.odb.exists(w) {
            return Err(ServeError::NotOurRef(*w));
        }
    }
    Ok(())
}

/// Validate `want` OIDs against advertisement and config rela rules.
pub(crate) fn validate_wants(
    repo: &Repository,
    refs: &[AdvertisedRef],
    head: Option<ObjectId>,
    wants: &[ObjectId],
    policy: &UploadPackPolicy,
) -> Result<()> {
    if policy.allow_reachable_sha1_in_want {
        let reachable = reachable_from_advertised(repo, refs, head)?;
        for w in wants {
            if !reachable.contains(w) {
                return Err(ServeError::NotOurRef(*w));
            }
        }
        return Ok(());
    }
    if policy.allow_tip_sha1_in_want {
        let tips = advertised_tip_commits(repo, refs, head);
        for w in wants {
            if !tips.contains(w) {
                return Err(ServeError::NotOurRef(*w));
            }
        }
        return Ok(());
    }
    let mut allowed: HashSet<ObjectId> = refs
        .iter()
        .flat_map(|r| std::iter::once(r.oid).chain(r.peeled))
        .collect();
    if let Some(h) = head {
        allowed.insert(h);
    }
    if let Some(bad) = wants.iter().find(|w| !allowed.contains(w)) {
        return Err(ServeError::NotOurRef(*bad));
    }
    Ok(())
}

fn advertised_tip_commits(
    repo: &Repository,
    refs: &[AdvertisedRef],
    head: Option<ObjectId>,
) -> HashSet<ObjectId> {
    let mut tips = HashSet::new();
    if let Some(h) = head {
        if let Ok(p) = super::peel_tag(repo, h) {
            tips.insert(p);
        }
    }
    for r in refs {
        if let Ok(p) = super::peel_tag(repo, r.oid) {
            tips.insert(p);
        }
        if let Some(peeled) = r.peeled {
            tips.insert(peeled);
        }
    }
    tips
}

fn reachable_from_advertised(
    repo: &Repository,
    refs: &[AdvertisedRef],
    head: Option<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let tips = advertised_tip_commits(repo, refs, head);
    let mut seen = HashSet::new();
    let mut q: VecDeque<ObjectId> = tips.iter().copied().collect();
    while let Some(oid) = q.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = crate::objects::parse_commit(&obj.data) {
                    q.extend(c.parents);
                    q.push_back(c.tree);
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = crate::objects::parse_tree(&obj.data) {
                    for entry in entries {
                        q.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = crate::objects::parse_tag(&obj.data) {
                    q.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(seen)
}
