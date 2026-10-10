//! Shallow/deepen handling for [`super::upload_pack`].

use std::collections::{HashMap, HashSet};

use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::rev_list::{
    shallow_boundary_oids, shallow_grafts_for_upload_pack_deepen,
    shallow_grafts_for_upload_pack_rev_list,
};
use crate::shallow::INFINITE_DEPTH;

/// Client shallow/deepen parameters from an upload-pack request.
#[derive(Debug, Default)]
pub(crate) struct ShallowRequest {
    pub client_shallow: Vec<ObjectId>,
    pub depth: Option<u32>,
    pub deepen_since: Option<i64>,
    pub deepen_not: Vec<ObjectId>,
    pub deepen_relative: bool,
}

impl ShallowRequest {
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.depth.is_some()
            || self.deepen_since.is_some()
            || !self.deepen_not.is_empty()
            || !self.client_shallow.is_empty()
    }
}

/// Shallow/unshallow lines to send and graft state for pack enumeration.
#[derive(Debug, Default)]
pub(crate) struct ShallowResponse {
    pub shallow: Vec<ObjectId>,
    pub unshallow: Vec<ObjectId>,
    /// Extra commit tips to include in the pack (parents of unshallowed commits).
    pub extra_wants: Vec<ObjectId>,
    /// Shallow grafts for `pack-objects` (do not walk past these commits' parents).
    pub pack_shallow_grafts: HashSet<ObjectId>,
}

/// Compute shallow/unshallow updates and pack grafts for one upload-pack fetch.
pub(crate) fn compute_shallow_response(
    repo: &Repository,
    wants: &[ObjectId],
    req: &ShallowRequest,
) -> crate::error::Result<ShallowResponse> {
    let mut out = ShallowResponse::default();
    if wants.is_empty() || !req.is_active() {
        return Ok(out);
    }

    let server_shallow = shallow_boundary_oids(&repo.git_dir);
    let client_set: HashSet<ObjectId> = req.client_shallow.iter().copied().collect();

    if req.depth == Some(INFINITE_DEPTH) && server_shallow.is_empty() {
        for oid in &req.client_shallow {
            out.unshallow.push(*oid);
            out.extra_wants.extend(parent_commits(repo, *oid)?);
        }
        out.pack_shallow_grafts = server_shallow;
        return Ok(out);
    }

    let mut new_shallow = Vec::new();
    if let Some(depth) = req.depth.filter(|d| *d > 0 && *d != INFINITE_DEPTH) {
        let depth = usize::try_from(depth).unwrap_or(usize::MAX);
        let depth = if req.deepen_relative {
            match relative_deepen_depth(repo, wants, &req.client_shallow, depth) {
                Some(d) => d,
                None => return Ok(out),
            }
        } else {
            depth
        };
        new_shallow =
            shallow_grafts_for_upload_pack_deepen(repo, wants, &req.client_shallow, depth);
    } else if req.deepen_since.is_some() || !req.deepen_not.is_empty() {
        new_shallow = shallow_grafts_for_upload_pack_rev_list(
            repo,
            wants,
            &req.client_shallow,
            req.deepen_since,
            &req.deepen_not,
        )?;
    } else if !server_shallow.is_empty() {
        new_shallow = crate::rev_list::shallow_borders_reachable_from_wants(repo, wants);
    }

    for oid in new_shallow {
        if !client_set.contains(&oid) {
            out.shallow.push(oid);
        }
    }

    let deepening = req.depth.is_some() || req.deepen_since.is_some() || !req.deepen_not.is_empty();
    let new_shallow_set: HashSet<ObjectId> = out.shallow.iter().copied().collect();
    if deepening {
        for oid in &req.client_shallow {
            if new_shallow_set.contains(oid) {
                continue;
            }
            if parents_in_repo(repo, *oid)? {
                out.unshallow.push(*oid);
                out.extra_wants.extend(parent_commits(repo, *oid)?);
            }
        }
    }

    out.pack_shallow_grafts = server_shallow;
    out.pack_shallow_grafts.extend(out.shallow.iter().copied());
    for oid in &req.client_shallow {
        if !out.unshallow.contains(oid) {
            out.pack_shallow_grafts.insert(*oid);
        }
    }

    Ok(out)
}

fn parent_commits(repo: &Repository, oid: ObjectId) -> crate::error::Result<Vec<ObjectId>> {
    let obj = repo.odb.read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Ok(Vec::new());
    }
    Ok(crate::objects::parse_commit(&obj.data)?.parents)
}

fn parents_in_repo(repo: &Repository, oid: ObjectId) -> crate::error::Result<bool> {
    for p in parent_commits(repo, oid)? {
        if !repo.odb.exists(&p) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Mirrors Git `get_shallows_depth` + `deepen_relative` depth adjustment.
fn relative_deepen_depth(
    repo: &Repository,
    wants: &[ObjectId],
    client_shallow: &[ObjectId],
    deepen: usize,
) -> Option<usize> {
    if client_shallow.is_empty() {
        return None;
    }
    let client_set: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    let mut best_depth: HashMap<ObjectId, usize> = HashMap::new();
    let mut q: std::collections::VecDeque<(ObjectId, usize)> = std::collections::VecDeque::new();
    for &w in wants {
        best_depth.insert(w, 1);
        q.push_back((w, 1));
    }
    let mut max_to_shallow = 0usize;
    while let Some((oid, depth)) = q.pop_front() {
        if best_depth.get(&oid).copied() != Some(depth) {
            continue;
        }
        if client_set.contains(&oid) {
            max_to_shallow = max_to_shallow.max(depth);
        }
        if depth > 256 {
            continue;
        }
        for p in parent_commits(repo, oid).ok()? {
            let nd = depth.saturating_add(1);
            let prev = best_depth.get(&p).copied().unwrap_or(usize::MAX);
            if nd < prev {
                best_depth.insert(p, nd);
                q.push_back((p, nd));
            }
        }
    }
    if max_to_shallow == 0 {
        return None;
    }
    Some(max_to_shallow.saturating_add(deepen))
}
