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
    /// `uploadpack.allowAnySHA1InWant`: any object present in the repository may
    /// be wanted, reachable or not. A large-object promisor that holds blobs no
    /// ref points at needs this to answer lazy fetches.
    pub allow_any_sha1_in_want: bool,
    pub ref_in_want: bool,
    pub allow_sideband_all: bool,
    /// `transfer.advertiseObjectInfo`: advertise and serve the v2 `object-info`
    /// command. Off by default, as in Git.
    pub advertise_object_info: bool,
    /// `uploadpack.lazyFetch` (grit only): before answering a client that accepted
    /// no promisor remote, fetch the promised objects the repository lacks from its
    /// own promisor remotes. Off by default, like Git's upload-pack.
    pub lazy_fetch: bool,
}

impl UploadPackPolicy {
    pub fn from_config(config: &ConfigSet) -> Self {
        let flag = |key: &str| config.get_bool(key).and_then(|v| v.ok()).unwrap_or(false);
        Self {
            allow_filter: flag("uploadpack.allowfilter"),
            allow_tip_sha1_in_want: flag("uploadpack.allowtipsha1inwant"),
            allow_reachable_sha1_in_want: flag("uploadpack.allowreachablesha1inwant"),
            allow_any_sha1_in_want: flag("uploadpack.allowanysha1inwant"),
            ref_in_want: flag("uploadpack.allowrefinwant"),
            allow_sideband_all: flag("uploadpack.allowsidebandall"),
            advertise_object_info: flag("transfer.advertiseobjectinfo"),
            lazy_fetch: flag("uploadpack.lazyfetch"),
        }
    }

    pub fn v0_capability_tokens(&self) -> Vec<&'static str> {
        let mut caps = vec!["shallow", "deepen-since", "deepen-not", "deepen-relative"];
        if self.allow_filter {
            caps.push("filter");
        }
        // As in Git, allowing any object implies both narrower capabilities.
        if self.allow_tip_sha1_in_want || self.allow_any_sha1_in_want {
            caps.push("allow-tip-sha1-in-want");
        }
        if self.allow_reachable_sha1_in_want || self.allow_any_sha1_in_want {
            caps.push("allow-reachable-sha1-in-want");
        }
        caps
    }

    /// Features for the v2 `fetch=` capability line.
    ///
    /// Only v2 features belong here. Request arguments every v2 server accepts
    /// (`thin-pack`, `ofs-delta`, `deepen-not`, ...) are not advertised.
    pub fn v2_fetch_features(&self) -> Vec<&'static str> {
        let mut f = vec!["shallow", "wait-for-done"];
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
/// (plus `HEAD`). Stateless RPC (smart HTTP POST) matches Git: objects reachable
/// from current ref tips are allowed without extra config, but wants that are
/// already advertised ref tips skip reachability work.
pub(crate) fn validate_wants(
    repo: &Repository,
    refs: &[AdvertisedRef],
    head: Option<ObjectId>,
    wants: &[ObjectId],
    policy: &UploadPackPolicy,
    stateless_rpc: bool,
) -> Result<()> {
    if policy.allow_any_sha1_in_want {
        return validate_wants_filtered(repo, wants);
    }
    if policy.allow_reachable_sha1_in_want {
        let reachable = reachable_from_repository_tips(repo)?;
        for w in wants {
            if !reachable.contains(w) {
                return Err(ServeError::NotOurRef(*w));
            }
        }
        return Ok(());
    }

    let advertised = advertised_object_ids(refs, head);
    let extra: Vec<ObjectId> = wants
        .iter()
        .copied()
        .filter(|w| !advertised.contains(w))
        .collect();
    if extra.is_empty() {
        return Ok(());
    }

    if stateless_rpc {
        validate_stateless_extra_wants(repo, &extra)?;
        return Ok(());
    }

    if policy.allow_tip_sha1_in_want {
        let tips = repository_tip_commits(repo)?;
        for w in &extra {
            if !tips.contains(w) {
                return Err(ServeError::NotOurRef(*w));
            }
        }
        return Ok(());
    }

    Err(ServeError::NotOurRef(extra[0]))
}

fn advertised_object_ids(refs: &[AdvertisedRef], head: Option<ObjectId>) -> HashSet<ObjectId> {
    let mut allowed: HashSet<ObjectId> = refs
        .iter()
        .flat_map(|r| std::iter::once(r.oid).chain(r.peeled))
        .collect();
    if let Some(h) = head {
        allowed.insert(h);
    }
    allowed
}

/// Extra wants in stateless RPC: reachable from ref tips (Git smart HTTP race case).
fn validate_stateless_extra_wants(repo: &Repository, wants: &[ObjectId]) -> Result<()> {
    let tips = repository_tip_commits(repo)?;
    let ref_objects = repository_ref_object_ids(repo)?;
    for w in wants {
        if ref_objects.contains(w) {
            continue;
        }
        if !object_reachable_from_tip_commits(repo, *w, &tips)? {
            return Err(ServeError::NotOurRef(*w));
        }
    }
    Ok(())
}

/// Direct ref targets (unpeeled), including annotated tag objects on refs.
fn repository_ref_object_ids(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut ids = HashSet::new();
    for (_, oid) in crate::refs::list_refs(repo.git_dir(), "refs/")? {
        ids.insert(oid);
    }
    if let Ok(head) = crate::refs::resolve_ref(repo.git_dir(), "HEAD") {
        ids.insert(head);
    }
    Ok(ids)
}

/// Whether `want` is reachable from current tip commits (ancestor of some tip).
fn object_reachable_from_tip_commits(
    repo: &Repository,
    want: ObjectId,
    tips: &HashSet<ObjectId>,
) -> Result<bool> {
    if tips.contains(&want) {
        return Ok(true);
    }
    if !repo.odb().contains(&want)? {
        return Ok(false);
    }
    let Ok(obj) = repo.odb().read(&want) else {
        return Ok(false);
    };
    match obj.kind {
        ObjectKind::Commit => commit_is_ancestor_of_some_tip(repo, want, tips),
        ObjectKind::Tag => {
            let Ok(tag) = crate::objects::parse_tag(&obj.data) else {
                return Ok(false);
            };
            reachable_object_from_tips(repo, tag.object, tips)
        }
        ObjectKind::Tree | ObjectKind::Blob => object_reachable_in_tip_closure(repo, want, tips),
    }
}

/// Reachability of a non-tag object from ref tips (tag targets and nested tags).
fn reachable_object_from_tips(
    repo: &Repository,
    object: ObjectId,
    tips: &HashSet<ObjectId>,
) -> Result<bool> {
    if tips.contains(&object) {
        return Ok(true);
    }
    if !repo.odb().contains(&object)? {
        return Ok(false);
    }
    let Ok(obj) = repo.odb().read(&object) else {
        return Ok(false);
    };
    match obj.kind {
        ObjectKind::Commit => commit_is_ancestor_of_some_tip(repo, object, tips),
        ObjectKind::Tree | ObjectKind::Blob => object_reachable_in_tip_closure(repo, object, tips),
        ObjectKind::Tag => {
            let Ok(tag) = crate::objects::parse_tag(&obj.data) else {
                return Ok(false);
            };
            reachable_object_from_tips(repo, tag.object, tips)
        }
    }
}

fn commit_is_ancestor_of_some_tip(
    repo: &Repository,
    want: ObjectId,
    tips: &HashSet<ObjectId>,
) -> Result<bool> {
    for tip in tips {
        if want == *tip || crate::merge_base::is_ancestor(repo, want, *tip)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Tree/blob wants: walk from tips until `target` appears (early exit).
fn object_reachable_in_tip_closure(
    repo: &Repository,
    target: ObjectId,
    tips: &HashSet<ObjectId>,
) -> Result<bool> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<ObjectId> = tips.iter().copied().collect();
    while let Some(oid) = queue.pop_front() {
        if oid == target {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb().read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = crate::objects::parse_commit(&obj.data) {
                    queue.extend(c.parents);
                    queue.push_back(c.tree);
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = crate::objects::parse_tree(&obj.data) {
                    for entry in entries {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = crate::objects::parse_tag(&obj.data) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(false)
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
