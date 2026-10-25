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
    pub allow_sideband_all: bool,
}

impl UploadPackPolicy {
    pub fn from_config(config: &ConfigSet) -> Self {
        let flag = |key: &str| config.get_bool(key).and_then(|v| v.ok()).unwrap_or(false);
        Self {
            allow_filter: flag("uploadpack.allowfilter"),
            allow_tip_sha1_in_want: flag("uploadpack.allowtipsha1inwant"),
            allow_reachable_sha1_in_want: flag("uploadpack.allowreachablesha1inwant"),
            ref_in_want: flag("uploadpack.allowrefinwant"),
            allow_sideband_all: flag("uploadpack.allowsidebandall"),
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

    /// Features for the v2 `fetch=` capability line.
    ///
    /// Only v2 features belong here. Request arguments every v2 server accepts
    /// (`thin-pack`, `ofs-delta`, `deepen-not`, ...) are not advertised.
    pub fn v2_fetch_features(&self) -> Vec<&'static str> {
        let mut f = vec!["shallow"];
        if self.allow_filter {
            f.push("filter");
        }
        if self.ref_in_want {
            f.push("ref-in-want");
        }
        if self.allow_sideband_all {
            f.push("sideband-all");
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
    crate::refs::resolve_ref(repo.git_dir(), &full)
        .or_else(|_| crate::refs::resolve_ref(repo.git_dir(), name))
        .map_err(|_| ServeError::Protocol(format!("unknown ref {name}")))
}

/// Validate `want` OIDs for a filtered fetch (object must exist in the ODB).
pub(crate) fn validate_wants_filtered(repo: &Repository, wants: &[ObjectId]) -> Result<()> {
    for w in wants {
        if !repo.odb().contains(w)? {
            return Err(ServeError::NotOurRef(*w));
        }
    }
    Ok(())
}

/// Validate `want` OIDs against advertisement, config rules, and session mode.
///
/// Stateful sessions only accept wants that appeared in the ref advertisement
/// (plus `HEAD`). Stateless RPC (smart HTTP POST) matches Git: any commit
/// reachable from current ref tips is allowed without extra config.
pub(crate) fn validate_wants(
    repo: &Repository,
    refs: &[AdvertisedRef],
    head: Option<ObjectId>,
    wants: &[ObjectId],
    policy: &UploadPackPolicy,
    stateless_rpc: bool,
) -> Result<()> {
    if policy.allow_reachable_sha1_in_want {
        let reachable = reachable_from_repository_tips(repo)?;
        for w in wants {
            if !reachable.contains(w) {
                return Err(ServeError::NotOurRef(*w));
            }
        }
        return Ok(());
    }
    if policy.allow_tip_sha1_in_want {
        let tips = repository_tip_commits(repo)?;
        for w in wants {
            if !tips.contains(w) {
                return Err(ServeError::NotOurRef(*w));
            }
        }
        return Ok(());
    }
    if stateless_rpc {
        let reachable = reachable_from_repository_tips(repo)?;
        for w in wants {
            if !reachable.contains(w) {
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

/// Peeled commit tips for every ref in the repository (including hidden refs).
fn repository_tip_commits(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut tips = HashSet::new();
    for (_, oid) in crate::refs::list_refs(repo.git_dir(), "refs/")? {
        if let Ok(p) = repo.odb().peel_tags(oid).map(|(p, _)| p) {
            tips.insert(p);
        }
    }
    if let Ok(head) = crate::refs::resolve_ref(repo.git_dir(), "HEAD") {
        if let Ok(p) = repo.odb().peel_tags(head).map(|(p, _)| p) {
            tips.insert(p);
        }
    }
    Ok(tips)
}

fn reachable_from_repository_tips(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let tips = repository_tip_commits(repo)?;
    let mut seen = HashSet::new();
    let mut q: VecDeque<ObjectId> = tips.iter().copied().collect();
    while let Some(oid) = q.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb().read(&oid) else {
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
