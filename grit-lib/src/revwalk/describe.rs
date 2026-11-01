//! Nearest-tag description of a commit ([`RevWalk::describe`]).
//!
//! Chooses a reachable tag with minimum exact range count (`tag..commit`), not Git
//! `describe`'s lazy depth estimate. One commit-graph-backed walk from the target
//! discovers every reachable tag and its distance, instead of a separate rev-list
//! count per tag.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::commit_pretty::resolve_core_abbrev_len;
use crate::error::Result;
use crate::merge_base::CommitGraphCache;
use crate::objects::ObjectId;
use crate::objects::{oid_hash_map, oid_hash_set, OidHashMap, OidHashSet};
use crate::refs::listing::{
    creator_date, query_refs, RefKind, RefListing, RefNamespace, RefQuery, RefQueryError,
    RefQueryOptions,
};
use crate::refs::store::RefName;
use crate::repo::Repository;
use crate::rev_parse::{peel_to_commit_for_merge_base, resolve_revision};
use crate::OidAbbrevBatch;

use super::RevWalk;

/// Options for [`RevWalk::describe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescribeOptions {
    /// When true, lightweight tags are candidates; when false, only annotated tags.
    pub tags: bool,
    /// Optional glob matched against the tag short name (same rules as ref queries).
    pub match_glob: Option<String>,
    /// Count distance along first-parent chains only.
    pub first_parent: bool,
    /// Minimum abbreviated oid length; `None` uses `core.abbrev` (auto from object count).
    pub abbrev: Option<usize>,
}

/// Result of a successful describe: nearest tag by exact range count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct DescribedCommit {
    /// Full tag ref name (`refs/tags/…`).
    pub tag: RefName,
    /// Object id stored at the tag ref (annotated tag object or commit for lightweight).
    pub tag_oid: ObjectId,
    /// Commit the tag peel chain resolves to.
    pub peeled: ObjectId,
    /// Exact count of commits reachable from [`Self::commit`] excluding ancestors of
    /// [`Self::peeled`] (same as `grit revs --count peeled..commit`). Can be smaller than
    /// Git `describe`'s displayed depth; the chosen tag can differ when Git stops after its
    /// default candidate cap.
    pub distance: usize,
    /// Described commit (peeled target revision).
    pub commit: ObjectId,
    /// Collision-aware abbreviated hex for [`Self::commit`].
    pub abbrev: String,
}

struct TagCandidate {
    listing: RefListing,
    peeled: ObjectId,
    annotated: bool,
    sort_time: i64,
}

/// Propagated "within cone" bits for discovered tag candidates (Git `describe` flags).
#[derive(Clone, Default)]
struct WithinBits {
    words: Vec<u64>,
}

impl WithinBits {
    fn with_candidates(n: usize) -> Self {
        Self {
            words: vec![0; n.div_ceil(64)],
        }
    }

    fn set(&mut self, index: usize) {
        self.words[index / 64] |= 1u64 << (index % 64);
    }

    fn has(&self, index: usize) -> bool {
        (self.words[index / 64] & (1u64 << (index % 64))) != 0
    }

    fn merge_from(&mut self, other: &Self) {
        for (slot, bits) in self.words.iter_mut().zip(&other.words) {
            *slot |= *bits;
        }
    }

    fn covers(&self, other: &Self) -> bool {
        other
            .words
            .iter()
            .zip(&self.words)
            .all(|(need, have)| (have & need) == *need)
    }
}

struct ActiveMatch {
    candidate_index: usize,
    depth: usize,
    found_order: usize,
}

/// Heap order for describe: largest shortest-path distance from the target first so merge
/// side branches are exhausted before shared ancestors (clock-skew safe without commit-graph),
/// then newest committer date, then stable enqueue order.
#[derive(Clone, Copy, Eq, PartialEq)]
struct DescribeWalkKey {
    path_depth: u32,
    committed_at: i64,
    seq: Reverse<u64>,
    epoch: u32,
    oid: ObjectId,
}

impl Ord for DescribeWalkKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path_depth
            .cmp(&other.path_depth)
            .then_with(|| self.committed_at.cmp(&other.committed_at))
            .then_with(|| self.seq.cmp(&other.seq))
            .then_with(|| self.epoch.cmp(&other.epoch))
            .then_with(|| self.oid.cmp(&other.oid))
    }
}

impl PartialOrd for DescribeWalkKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Depth increments applied on one pop, so a skew-driven re-pop can undo them.
struct PopEffect {
    incremented_matches: Vec<usize>,
}

struct DescribeWalkState {
    within_words: usize,
    path_depth: OidHashMap<u32>,
    within_pending: OidHashMap<WithinBits>,
    popped: OidHashSet,
    queued: OidHashSet,
    heap: BinaryHeap<DescribeWalkKey>,
    seq: u64,
    pop_effects: OidHashMap<PopEffect>,
    seen_commits: usize,
    epoch: OidHashMap<u32>,
}

impl DescribeWalkState {
    fn new(within_words: usize, target: ObjectId) -> Self {
        let mut within_pending = oid_hash_map();
        within_pending.insert(target, WithinBits::with_candidates(within_words));
        let mut path_depth = oid_hash_map();
        path_depth.insert(target, 0);
        Self {
            within_words,
            path_depth,
            within_pending,
            popped: oid_hash_set(),
            queued: oid_hash_set(),
            heap: BinaryHeap::new(),
            seq: 0,
            pop_effects: oid_hash_map(),
            seen_commits: 0,
            epoch: oid_hash_map(),
        }
    }

    fn bump_epoch(&mut self, oid: ObjectId) {
        *self.epoch.entry(oid).or_insert(0) += 1;
    }

    fn current_epoch(&self, oid: ObjectId) -> u32 {
        self.epoch.get(&oid).copied().unwrap_or(0)
    }

    fn push(&mut self, cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> Result<()> {
        let depth = *self.path_depth.get(&oid).unwrap_or(&0);
        let committed_at = cache.commit_time(oid)?;
        self.heap.push(DescribeWalkKey {
            path_depth: depth,
            committed_at,
            seq: Reverse(self.seq),
            epoch: self.current_epoch(oid),
            oid,
        });
        self.seq += 1;
        Ok(())
    }

    fn undo_pop(&mut self, matches: &mut [ActiveMatch], oid: ObjectId) {
        let Some(effect) = self.pop_effects.remove(&oid) else {
            return;
        };
        for idx in effect.incremented_matches {
            matches[idx].depth = matches[idx].depth.saturating_sub(1);
        }
        self.seen_commits = self.seen_commits.saturating_sub(1);
    }

    fn merge_within_to_parent(
        &mut self,
        cache: &mut CommitGraphCache<'_>,
        parent: ObjectId,
        child_depth: u32,
        child_within: &WithinBits,
        matches: &mut [ActiveMatch],
    ) -> Result<()> {
        let new_depth = child_depth.saturating_add(1);
        let depth_entry = self.path_depth.entry(parent).or_insert(0);
        let depth_increased = new_depth > *depth_entry;
        if depth_increased {
            *depth_entry = new_depth;
            self.bump_epoch(parent);
        }

        let entry = self
            .within_pending
            .entry(parent)
            .or_insert_with(|| WithinBits::with_candidates(self.within_words));
        let before = entry.words.clone();
        entry.merge_from(child_within);
        let within_grew = before != entry.words;

        if self.popped.contains(&parent) {
            if depth_increased {
                self.undo_pop(matches, parent);
                self.popped.remove(&parent);
                self.queued.remove(&parent);
            } else if within_grew {
                self.fix_depths_for_new_within(parent, matches);
            }
        }

        if self.queued.insert(parent) {
            self.push(cache, parent)?;
        }
        Ok(())
    }

    fn fix_depths_for_new_within(&mut self, oid: ObjectId, matches: &mut [ActiveMatch]) {
        let pending = self.pending_within(oid);
        let Some(effect) = self.pop_effects.get_mut(&oid) else {
            return;
        };
        let mut still_outside = Vec::new();
        for idx in effect.incremented_matches.drain(..) {
            if pending.has(matches[idx].found_order - 1) {
                matches[idx].depth = matches[idx].depth.saturating_sub(1);
            } else {
                still_outside.push(idx);
            }
        }
        effect.incremented_matches = still_outside;
    }

    fn pending_within(&self, oid: ObjectId) -> WithinBits {
        self.within_pending
            .get(&oid)
            .cloned()
            .unwrap_or_else(|| WithinBits::with_candidates(self.within_words))
    }

    fn heap_covers(&self, best_within: &WithinBits) -> bool {
        if self.heap.is_empty() {
            return false;
        }
        self.heap.iter().all(|key| {
            self.within_pending
                .get(&key.oid)
                .is_some_and(|pending| pending.covers(best_within))
        })
    }
}

impl<'repo> RevWalk<'repo> {
    /// Find the nearest reachable tag for `rev` using [`DescribeOptions`].
    ///
    /// Returns `None` when no tag peel resolves to a commit that is an ancestor of `rev`
    /// (or equals `rev`). Blob/tree tags are skipped.
    ///
    /// # Errors
    ///
    /// Propagates rev-parse, ref listing, object read, and rev-list count errors.
    pub fn describe(&self, rev: &str, opts: &DescribeOptions) -> Result<Option<DescribedCommit>> {
        let repo = self.repository();
        let commit = peel_to_commit_for_merge_base(repo, resolve_revision(repo, rev)?)?;
        let candidates = collect_tag_candidates(repo, opts)?;
        if candidates.is_empty() {
            return Ok(None);
        }

        let (unique, peel_to_index) = dedupe_candidates_by_peel(candidates);
        if unique.is_empty() {
            return Ok(None);
        }

        if let Some(idx) = peel_to_index.get(&commit) {
            return Ok(Some(build_described(repo, commit, &unique[*idx], 0, opts)?));
        }

        let Some((winner_idx, distance)) =
            nearest_tag_by_walk(repo, commit, &unique, &peel_to_index, opts.first_parent)?
        else {
            return Ok(None);
        };

        Ok(Some(build_described(
            repo,
            commit,
            &unique[winner_idx],
            distance,
            opts,
        )?))
    }
}

fn build_described(
    repo: &Repository,
    commit: ObjectId,
    winner: &TagCandidate,
    distance: usize,
    opts: &DescribeOptions,
) -> Result<DescribedCommit> {
    let config = repo.config()?.clone();
    let abbrev_len = match opts.abbrev {
        Some(n) => n,
        None => resolve_core_abbrev_len(&config, repo.odb())?,
    };
    let mut abbrev_batch = OidAbbrevBatch::new(abbrev_len);
    let abbrev = abbrev_batch.abbrev_known(repo, commit)?;
    let tag = RefName::try_from(winner.listing.full_name.as_str())
        .map_err(|e| crate::error::Error::InvalidRef(e.to_string()))?;
    Ok(DescribedCommit {
        tag,
        tag_oid: winner.listing.oid,
        peeled: winner.peeled,
        distance,
        commit,
        abbrev,
    })
}

fn dedupe_candidates_by_peel(
    candidates: Vec<TagCandidate>,
) -> (Vec<TagCandidate>, OidHashMap<usize>) {
    let mut unique: Vec<TagCandidate> = Vec::new();
    let mut peel_to_index: OidHashMap<usize> = oid_hash_map();
    for cand in candidates {
        if let Some(&idx) = peel_to_index.get(&cand.peeled) {
            if tie_break_tag(&cand, &unique[idx]) {
                unique[idx] = cand;
            }
        } else {
            peel_to_index.insert(cand.peeled, unique.len());
            unique.push(cand);
        }
    }
    (unique, peel_to_index)
}

/// Git `describe_commit` walk: one backward pass from `commit`, flagging tag peels as found.
fn nearest_tag_by_walk(
    repo: &Repository,
    commit: ObjectId,
    unique: &[TagCandidate],
    peel_to_index: &OidHashMap<usize>,
    first_parent: bool,
) -> Result<Option<(usize, usize)>> {
    let within_words = unique.len().max(1);
    let mut cache = CommitGraphCache::with_commit_graph(repo, true);
    let mut walk = DescribeWalkState::new(within_words, commit);
    walk.queued.insert(commit);
    walk.push(&mut cache, commit)?;

    let mut matches: Vec<ActiveMatch> = Vec::new();
    let mut annotated_match_count = 0usize;

    while let Some(key) = walk.heap.pop() {
        let cur = key.oid;
        let expected_depth = *walk.path_depth.get(&cur).unwrap_or(&0);
        if key.path_depth != expected_depth || key.epoch != walk.current_epoch(cur) {
            continue;
        }
        if !walk.queued.remove(&cur) {
            continue;
        }

        if walk.popped.contains(&cur) {
            walk.undo_pop(&mut matches, cur);
        }
        walk.popped.insert(cur);
        walk.seen_commits += 1;

        let mut active_within = walk.pending_within(cur);

        if let Some(&candidate_index) = peel_to_index.get(&cur) {
            let depth = walk.seen_commits.saturating_sub(1);
            register_tag_match(
                &mut matches,
                &mut annotated_match_count,
                unique,
                candidate_index,
                depth,
            );
            active_within.set(candidate_index);
            walk.within_pending.insert(cur, active_within.clone());
        }

        let mut incremented_matches = Vec::new();
        for (idx, m) in matches.iter_mut().enumerate() {
            if !active_within.has(m.found_order - 1) {
                m.depth += 1;
                incremented_matches.push(idx);
            }
        }
        walk.pop_effects.insert(
            cur,
            PopEffect {
                incremented_matches,
            },
        );

        if annotated_match_count > 0 {
            let (_, best_within) = best_match_within(&matches, unique.len());
            if walk.heap_covers(&best_within)
                || (walk.heap.is_empty() && active_within.covers(&best_within))
            {
                break;
            }
        }

        walk.bump_epoch(cur);

        let parents = cache.parents_of(cur)?;
        let child_depth = expected_depth;
        let parent_iter: Box<dyn Iterator<Item = ObjectId>> = if first_parent {
            Box::new(parents.into_iter().take(1))
        } else {
            Box::new(parents.into_iter())
        };
        for parent in parent_iter {
            walk.merge_within_to_parent(
                &mut cache,
                parent,
                child_depth,
                &active_within,
                &mut matches,
            )?;
        }
    }

    if matches.is_empty() {
        return Ok(None);
    }

    matches.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.found_order.cmp(&b.found_order))
    });

    let best_depth = matches[0].depth;
    let mut best: Option<&ActiveMatch> = None;
    for m in &matches {
        if m.depth != best_depth {
            break;
        }
        let cand = &unique[m.candidate_index];
        match best {
            None => best = Some(m),
            Some(prev) => {
                let prev_cand = &unique[prev.candidate_index];
                if tie_break_tag(cand, prev_cand) {
                    best = Some(m);
                }
            }
        }
    }

    let Some(winner) = best else {
        return Ok(None);
    };
    Ok(Some((winner.candidate_index, winner.depth)))
}

/// One [`ActiveMatch`] and within bit per deduped tag candidate; depth-bump re-pops update in place.
fn register_tag_match(
    matches: &mut Vec<ActiveMatch>,
    annotated_match_count: &mut usize,
    unique: &[TagCandidate],
    candidate_index: usize,
    depth: usize,
) {
    let found_order = candidate_index + 1;
    if let Some(existing) = matches
        .iter_mut()
        .find(|m| m.candidate_index == candidate_index)
    {
        existing.depth = depth;
        return;
    }
    if unique[candidate_index].annotated {
        *annotated_match_count += 1;
    }
    matches.push(ActiveMatch {
        candidate_index,
        depth,
        found_order,
    });
}

fn best_match_within(matches: &[ActiveMatch], candidate_count: usize) -> (usize, WithinBits) {
    let mut best_depth = usize::MAX;
    let word_count = candidate_count.max(1);
    let mut bits = WithinBits::with_candidates(word_count);
    for m in matches {
        if m.depth < best_depth {
            best_depth = m.depth;
            bits = WithinBits::with_candidates(word_count);
            bits.set(m.found_order - 1);
        } else if m.depth == best_depth {
            bits.set(m.found_order - 1);
        }
    }
    (best_depth, bits)
}

fn map_ref_query(err: RefQueryError) -> crate::error::Error {
    match err {
        RefQueryError::Store(e) => e,
        other => crate::error::Error::InvalidRef(other.to_string()),
    }
}

fn collect_tag_candidates(repo: &Repository, opts: &DescribeOptions) -> Result<Vec<TagCandidate>> {
    let mut query = RefQuery {
        namespaces: vec![RefNamespace::Tags],
        ..RefQuery::default()
    };
    if let Some(glob) = &opts.match_glob {
        query.patterns = vec![glob.clone()];
    }
    let listings = query_refs(repo, &query, &RefQueryOptions::default()).map_err(map_ref_query)?;
    let odb = repo.odb();
    let mut out = Vec::new();
    for listing in listings {
        if listing.kind != RefKind::Tag {
            continue;
        }
        let Some(peeled) = odb.peel_to_commit(listing.oid)? else {
            continue;
        };
        let annotated = listing.oid != peeled;
        if !opts.tags && !annotated {
            continue;
        }
        let sort_time = tag_sort_time(repo, listing.oid, peeled, annotated)?;
        out.push(TagCandidate {
            listing,
            peeled,
            annotated,
            sort_time,
        });
    }
    Ok(out)
}

fn tag_sort_time(
    repo: &Repository,
    tag_oid: ObjectId,
    peeled: ObjectId,
    annotated: bool,
) -> Result<i64> {
    let oid = if annotated { tag_oid } else { peeled };
    creator_date(repo.odb(), &oid)
}

fn tie_break_tag(a: &TagCandidate, b: &TagCandidate) -> bool {
    if a.annotated != b.annotated {
        return a.annotated;
    }
    if a.sort_time != b.sort_time {
        return a.sort_time > b.sort_time;
    }
    a.listing.short_name < b.listing.short_name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::Signature;
    use crate::objects::{serialize_tag, ObjectKind, TagData};
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::refs::store::RefName;
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    fn empty_commit(repo: &Repository, msg: &str) -> ObjectId {
        let mut req = CommitRequest::new(msg);
        req.allow_empty = true;
        req.author = Some(Signature::new("A", "a@e.com", repo.identity_now()).expect("author"));
        req.committer =
            Some(Signature::new("C", "c@e.com", repo.identity_now()).expect("committer"));
        create_commit(repo, &req, &mut NullProgress, &NeverCancel)
            .expect("commit")
            .oid
    }

    fn tag_light(repo: &Repository, name: &str, target: ObjectId) {
        let full = format!("refs/tags/{name}");
        let name = RefName::try_from(full.as_str()).expect("name");
        repo.references().update(&name, &target).expect("tag");
    }

    fn tag_annotated(repo: &Repository, name: &str, target: ObjectId, msg: &str) {
        let tagger = Signature::new("T", "t@e.com", repo.identity_now()).expect("tagger");
        let data = TagData {
            object: target,
            object_type: "commit".into(),
            tag: name.to_owned(),
            tagger: Some(tagger.header_line()),
            extra_headers: Vec::new(),
            message: format!("{msg}\n"),
        };
        let bytes = serialize_tag(&data);
        let oid = repo.odb().write(ObjectKind::Tag, &bytes).expect("write");
        let full = format!("refs/tags/{name}");
        let name = RefName::try_from(full.as_str()).expect("name");
        repo.references().update(&name, &oid).expect("update");
    }

    #[test]
    fn describe_linear_exact_tag() -> Result<()> {
        let (_dir, repo) = init_repo();
        let c1 = empty_commit(&repo, "c1");
        let _c2 = empty_commit(&repo, "c2");
        let c3 = empty_commit(&repo, "c3");
        tag_light(&repo, "v1", c1);
        let head = repo.rev_walk().resolve("HEAD")?;
        assert_eq!(head, c3);
        let d = repo
            .rev_walk()
            .describe(
                "HEAD",
                &DescribeOptions {
                    tags: true,
                    ..Default::default()
                },
            )?
            .expect("described");
        assert_eq!(d.peeled, c1);
        assert_eq!(d.distance, 2);
        assert_eq!(d.commit, c3);
        Ok(())
    }

    #[test]
    fn describe_nested_annotated_tag() -> Result<()> {
        let (_dir, repo) = init_repo();
        let base = empty_commit(&repo, "base");
        let _tip = empty_commit(&repo, "tip");
        tag_annotated(&repo, "inner", base, "inner");
        tag_annotated(&repo, "outer", inner_tag_oid(&repo, "inner"), "outer");
        let d = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .expect("described");
        assert_eq!(d.peeled, base);
        assert_eq!(d.distance, 1);
        Ok(())
    }

    fn inner_tag_oid(repo: &Repository, short: &str) -> ObjectId {
        let full = format!("refs/tags/{short}");
        let name = RefName::try_from(full.as_str()).expect("name");
        repo.references().resolve(&name).expect("resolve")
    }

    #[test]
    fn describe_skips_blob_tag() -> Result<()> {
        let (_dir, repo) = init_repo();
        let c1 = empty_commit(&repo, "c1");
        let blob = repo
            .odb()
            .write_loose_materialize(ObjectKind::Blob, b"x")
            .expect("blob");
        tag_light(&repo, "blob-tag", blob);
        tag_light(&repo, "v1", c1);
        let d = repo.rev_walk().describe(
            "HEAD",
            &DescribeOptions {
                tags: true,
                ..Default::default()
            },
        )?;
        assert!(d.is_some());
        Ok(())
    }

    #[test]
    fn describe_picks_minimum_distance_not_list_order() -> Result<()> {
        let (_dir, repo) = init_repo();
        let c1 = empty_commit(&repo, "c1");
        let _c2 = empty_commit(&repo, "c2");
        let c3 = empty_commit(&repo, "c3");
        let _c4 = empty_commit(&repo, "c4");
        tag_light(&repo, "near", c3);
        tag_light(&repo, "far", c1);
        let d = repo
            .rev_walk()
            .describe(
                "HEAD",
                &DescribeOptions {
                    tags: true,
                    ..Default::default()
                },
            )?
            .expect("described");
        assert_eq!(d.peeled, c3);
        assert_eq!(d.distance, 1);
        Ok(())
    }

    #[test]
    fn describe_annotated_only_skips_lightweight() -> Result<()> {
        let (_dir, repo) = init_repo();
        let c1 = empty_commit(&repo, "c1");
        let _c2 = empty_commit(&repo, "c2");
        tag_light(&repo, "light", c1);
        tag_annotated(&repo, "ann", c1, "m");
        let d = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .expect("described");
        assert_eq!(d.tag_oid, inner_tag_oid(&repo, "ann"));
        assert_eq!(d.distance, 1);
        Ok(())
    }

    #[test]
    fn describe_no_tag_returns_none() -> Result<()> {
        let (_dir, repo) = init_repo();
        let _c = empty_commit(&repo, "only");
        assert!(repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .is_none());
        Ok(())
    }

    #[test]
    fn describe_stable_after_commit_graph_write() -> Result<()> {
        use crate::maintenance::CommitGraphOptions;
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let day = 86_400i64;
        let c1 = write_commit_at(odb, &[], "c1", day)?;
        let c2 = write_commit_at(odb, &[c1], "c2", day * 2)?;
        let c3 = write_commit_at(odb, &[c2], "c3", day * 3)?;
        let c4 = write_commit_at(odb, &[c3], "c4", day * 4)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &c4)?;
        tag_annotated(&repo, "a-far", c1, "far");
        tag_annotated(&repo, "z-near", c3, "near");
        let opts = DescribeOptions::default();
        let before = repo.rev_walk().describe("HEAD", &opts)?.expect("before");
        assert_eq!(tag_short_name(&before), "z-near");
        assert_eq!(before.distance, 1);
        repo.write_commit_graph(&CommitGraphOptions::default())?;
        let after = repo.rev_walk().describe("HEAD", &opts)?.expect("after");
        assert_eq!(tag_short_name(&after), "z-near");
        assert_eq!(after.distance, 1);
        Ok(())
    }

    #[test]
    fn describe_first_parent_reachability() -> Result<()> {
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let root = write_commit_at(odb, &[], "root", 1)?;
        let main1 = write_commit_at(odb, &[root], "main1", 2)?;
        let main2 = write_commit_at(odb, &[main1], "main2", 3)?;
        let side = write_commit_at(odb, &[root], "side", 4)?;
        let merge = write_commit_at(odb, &[main2, side], "merge", 5)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        tag_annotated(&repo, "old", root, "old");
        tag_annotated(&repo, "side-tag", side, "side");
        let all = repo.rev_walk().describe(
            "HEAD",
            &DescribeOptions {
                tags: true,
                ..Default::default()
            },
        )?;
        assert_eq!(all.as_ref().map(tag_short_name), Some("side-tag"));
        let fp = repo
            .rev_walk()
            .describe(
                "HEAD",
                &DescribeOptions {
                    tags: true,
                    first_parent: true,
                    ..Default::default()
                },
            )?
            .expect("first-parent");
        assert_eq!(tag_short_name(&fp), "old");
        assert_eq!(fp.distance, 3);
        Ok(())
    }

    #[test]
    fn describe_tie_break_prefers_ascending_ref_name() -> Result<()> {
        let (_dir, repo) = init_repo();
        let c1 = empty_commit(&repo, "c1");
        let c2 = empty_commit(&repo, "c2");
        tag_annotated(&repo, "ztag", c1, "z");
        tag_annotated(&repo, "atag", c1, "a");
        let d = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .expect("described");
        assert_eq!(tag_short_name(&d), "atag");
        assert_eq!(d.distance, 1);
        assert_eq!(d.peeled, c1);
        assert_eq!(c2, repo.rev_walk().resolve("HEAD")?);
        Ok(())
    }

    #[test]
    fn describe_match_glob() -> Result<()> {
        let (_dir, repo) = init_repo();
        let c1 = empty_commit(&repo, "c1");
        let _c2 = empty_commit(&repo, "c2");
        tag_light(&repo, "release-1", c1);
        tag_light(&repo, "other", c1);
        let d = repo.rev_walk().describe(
            "HEAD",
            &DescribeOptions {
                tags: true,
                match_glob: Some("release-*".to_owned()),
                ..Default::default()
            },
        )?;
        assert!(d.is_some());
        assert!(d.unwrap().tag.as_str().contains("release-1"));
        Ok(())
    }

    /// Merge with decoy tags: side branch tag is closer in walk order but main tag has the
    /// smaller exact `peeled..HEAD` count (Git's 10-candidate cap can pick the side tag).
    #[test]
    fn describe_exact_minimum_beats_early_side_tag() -> Result<()> {
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let root = write_commit_at(odb, &[], "root", 10)?;
        let main_old = write_commit_at(odb, &[root], "main-old", 20)?;
        let main_mid = write_commit_at(odb, &[main_old], "main-mid", 30)?;
        let main_tip = write_commit_at(odb, &[main_mid], "main-tip", 40)?;
        let side = write_commit_at(odb, &[root], "side", 35)?;
        let merge = write_commit_at(odb, &[main_tip, side], "merge", 50)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        for i in 0..12 {
            tag_annotated(&repo, &format!("decoy-{i}"), main_old, "decoy");
        }
        tag_annotated(&repo, "on-main", main_tip, "main tag");
        tag_annotated(&repo, "on-side", side, "side tag");
        let d = repo
            .rev_walk()
            .describe(
                "HEAD",
                &DescribeOptions {
                    tags: true,
                    ..Default::default()
                },
            )?
            .expect("described");
        assert_eq!(tag_short_name(&d), "on-main");
        assert_eq!(d.distance, 2);
        assert_eq!(d.peeled, main_tip);
        Ok(())
    }

    /// Committer dates on a merge arm can order a shared parent before a tagged descendant
    /// unless the walk uses graph distance from the target (and re-pops on new within bits).
    #[test]
    fn describe_clock_skew_merge_without_commit_graph() -> Result<()> {
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let d2ce4b6f = write_commit_at(odb, &[], "base", 3129)?;
        let c036901c = write_commit_at(odb, &[d2ce4b6f], "tagged", 2598)?;
        let p4d1d52e7 = write_commit_at(odb, &[c036901c], "side", 4411)?;
        let f346ffdd = write_commit_at(odb, &[d2ce4b6f, p4d1d52e7], "merge", 5000)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &f346ffdd)?;
        tag_annotated(&repo, "t20", c036901c, "tag");
        let opts = DescribeOptions {
            tags: true,
            ..Default::default()
        };
        let d = repo.rev_walk().describe("HEAD", &opts)?.expect("described");
        assert_eq!(d.peeled, c036901c);
        assert_eq!(d.distance, 2);
        Ok(())
    }

    /// Depth-bump re-pops of a tagged commit must not register extra within bits (one bit per
    /// deduped candidate), which otherwise panics once `matches.len()` exceeds `unique.len()`.
    #[test]
    fn describe_repop_tagged_commit_reuses_stable_match_bit() -> Result<()> {
        use crate::merge_base::{
            count_commits_reachable_excluding_ancestors_of, ReachableCountFilter,
        };
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let tagged = write_commit_at(odb, &[], "tagged", 1)?;
        tag_annotated(&repo, "t20", tagged, "tag");

        const PATHS: usize = 65;
        const PATH_LEN: usize = 32;
        let mut heads = Vec::with_capacity(PATHS);
        for path in 0..PATHS {
            let mut cur = tagged;
            for step in 0..PATH_LEN {
                cur = write_commit_at(
                    odb,
                    &[cur],
                    &format!("p{path}-s{step}"),
                    100 + path as i64 * 100 + step as i64,
                )?;
            }
            heads.push(cur);
        }

        while heads.len() > 1 {
            let mut next = Vec::new();
            let mut iter = heads.into_iter();
            while let Some(left) = iter.next() {
                match iter.next() {
                    Some(right) => {
                        let time = 10_000 + next.len() as i64;
                        next.push(write_commit_at(
                            odb,
                            &[left, right],
                            &format!("m{}", next.len()),
                            time,
                        )?);
                    }
                    None => next.push(left),
                }
            }
            heads = next;
        }
        let head = heads.into_iter().next().expect("head");
        refs::write_ref(repo.git_dir(), "refs/heads/main", &head)?;

        let expected = count_commits_reachable_excluding_ancestors_of(
            &repo,
            &[head],
            &[tagged],
            ReachableCountFilter::default(),
        )?;

        let d = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .expect("described without within-bit overflow");
        assert_eq!(tag_short_name(&d), "t20");
        assert_eq!(d.peeled, tagged);
        assert_eq!(d.distance, expected);
        Ok(())
    }

    /// Many tags on one line: distance work must stay O(commits), not O(tags × commits).
    #[test]
    fn describe_many_tags_on_chain_parent_reads_bounded() -> Result<()> {
        use crate::merge_base::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        let (_dir, repo) = init_repo();
        let mut chain = Vec::new();
        for i in 0..120 {
            let parents = if i == 0 { vec![] } else { vec![chain[i - 1]] };
            chain.push(write_commit_at(
                repo.odb(),
                &parents,
                &format!("c{i}"),
                100 + i as i64,
            )?);
        }
        let tip = *chain.last().expect("tip");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip)?;
        for (i, oid) in chain.iter().enumerate().step_by(2) {
            tag_annotated(&repo, &format!("t{i}"), *oid, "m");
        }

        reset_commit_parent_lookups();
        let d = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .expect("described");
        assert_eq!(d.distance, 1);
        let lookups = commit_parent_lookups();
        assert!(
            lookups <= 400,
            "describe must not multiply parent reads by tag count ({lookups})"
        );
        Ok(())
    }

    fn tag_short_name(d: &DescribedCommit) -> &str {
        d.tag
            .as_str()
            .strip_prefix("refs/tags/")
            .unwrap_or(d.tag.as_str())
    }

    fn write_commit_at(
        odb: &crate::objects::odb::Odb,
        parents: &[ObjectId],
        msg: &str,
        time: i64,
    ) -> Result<ObjectId> {
        let tree = odb.write_loose_materialize(ObjectKind::Tree, b"")?;
        let mut body = format!("tree {tree}\n");
        for p in parents {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {time} +0000\ncommitter T <t@e.com> {time} +0000\n\n{msg}\n"
        ));
        odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
    }
}
