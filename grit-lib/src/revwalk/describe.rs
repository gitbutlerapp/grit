//! Nearest-tag description of a commit ([`RevWalk::describe`]).
//!
//! Chooses a reachable tag with minimum exact range count (`tag..commit`), not Git
//! `describe`'s lazy depth estimate. With commit-graph generations, one backward walk
//! from the target discovers candidates and early-exits; without a graph, fixpoint within
//! propagation over the reachable subgraph yields exact distances in one history pass.

use std::collections::{BinaryHeap, VecDeque};

use crate::commit_pretty::resolve_core_abbrev_len;
use crate::error::Result;
use crate::merge_base::{push_describe_heap_key, CommitGraphCache, PaintKey};
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

/// Picks the nearest tag and exact distance from `commit`.
///
/// With commit-graph generations, uses Git's one-pass backward walk and early exit. Without a
/// graph, uses fixpoint within propagation over the reachable subgraph (one history pass, not
/// one rev-list count per tag).
fn nearest_tag_by_walk(
    repo: &Repository,
    commit: ObjectId,
    unique: &[TagCandidate],
    peel_to_index: &OidHashMap<usize>,
    first_parent: bool,
) -> Result<Option<(usize, usize)>> {
    let mut cache = CommitGraphCache::with_commit_graph(repo, true);
    let generation_ranked = cache.generation_ranked_walk_from(commit)?;
    if !generation_ranked {
        return nearest_tag_by_fixpoint(repo, commit, unique, peel_to_index, first_parent);
    }

    let within_words = unique.len().max(1);

    let mut seen = oid_hash_set();
    let mut processed = oid_hash_set();
    let mut commit_flags: OidHashMap<WithinBits> = oid_hash_map();
    commit_flags.insert(commit, WithinBits::with_candidates(within_words));
    seen.insert(commit);
    let mut heap: BinaryHeap<PaintKey> = BinaryHeap::new();
    let mut seq = 0u64;
    push_describe_heap_key(&mut cache, &mut heap, &mut seq, commit, None)?;

    let mut matches: Vec<ActiveMatch> = Vec::new();
    let mut annotated_match_count = 0usize;
    let mut seen_commits = 0usize;

    while let Some(key) = heap.pop() {
        let cur = key.oid;
        if processed.contains(&cur) {
            continue;
        }
        processed.insert(cur);
        seen_commits += 1;

        let mut active_within = commit_flags
            .get(&cur)
            .cloned()
            .unwrap_or_else(|| WithinBits::with_candidates(within_words));

        if let Some(&candidate_index) = peel_to_index.get(&cur) {
            let depth = seen_commits.saturating_sub(1);
            register_tag_match(
                &mut matches,
                &mut annotated_match_count,
                unique,
                candidate_index,
                depth,
            );
            active_within.set(candidate_index);
            commit_flags.insert(cur, active_within.clone());
        }

        for m in &mut matches {
            if !active_within.has(m.found_order - 1) {
                m.depth += 1;
            }
        }

        if annotated_match_count > 0 {
            let (_, best_within) = best_match_within(&matches, unique.len());
            if describe_walk_can_stop(&active_within, &best_within, &heap, &commit_flags) {
                break;
            }
        }

        let parents = cache.parents_of(cur)?;
        let parent_iter: Box<dyn Iterator<Item = ObjectId>> = if first_parent {
            Box::new(parents.into_iter().take(1))
        } else {
            Box::new(parents.into_iter())
        };
        for parent in parent_iter {
            let entry = commit_flags
                .entry(parent)
                .or_insert_with(|| WithinBits::with_candidates(within_words));
            entry.merge_from(&active_within);
            if seen.insert(parent) {
                push_describe_heap_key(&mut cache, &mut heap, &mut seq, parent, None)?;
            }
        }
    }

    if matches.is_empty() {
        return Ok(None);
    }

    pick_winner_and_distance(repo, commit, unique, &matches, first_parent)
}

/// Exact range counts via ancestor marking (fixpoint within propagation from tag peels).
fn nearest_tag_by_fixpoint(
    repo: &Repository,
    head: ObjectId,
    unique: &[TagCandidate],
    peel_to_index: &OidHashMap<usize>,
    first_parent: bool,
) -> Result<Option<(usize, usize)>> {
    let tag_count = unique.len();
    if tag_count == 0 {
        return Ok(None);
    }
    let within_words = tag_count.max(1);
    let mut cache = CommitGraphCache::with_commit_graph(repo, true);
    let (reachable_order, parents_of) = collect_describe_reachable(&mut cache, head, first_parent)?;
    if reachable_order.is_empty() {
        return Ok(None);
    }
    let reachable: OidHashSet = reachable_order.iter().copied().collect();

    let mut full_within: OidHashMap<WithinBits> = oid_hash_map();
    let mut queue = VecDeque::new();
    for (&peel, &idx) in peel_to_index {
        if !reachable.contains(&peel) {
            continue;
        }
        full_within
            .entry(peel)
            .or_insert_with(|| WithinBits::with_candidates(within_words))
            .set(idx);
        queue.push_back(peel);
    }

    while let Some(cur) = queue.pop_front() {
        let Some(active) = full_within.get(&cur).cloned() else {
            continue;
        };
        let parents = match parents_of.get(&cur) {
            Some(p) => p.as_slice(),
            None => continue,
        };
        for &parent in parents {
            if !reachable.contains(&parent) {
                continue;
            }
            let entry = full_within
                .entry(parent)
                .or_insert_with(|| WithinBits::with_candidates(within_words));
            let before = entry.words.clone();
            entry.merge_from(&active);
            if before != entry.words {
                queue.push_back(parent);
            }
        }
    }

    let mut depths = vec![0usize; tag_count];
    for &oid in &reachable_order {
        let active = full_within
            .get(&oid)
            .cloned()
            .unwrap_or_else(|| WithinBits::with_candidates(within_words));
        for (idx, depth) in depths.iter_mut().enumerate() {
            if !active.has(idx) {
                *depth += 1;
            }
        }
    }

    let mut best: Option<(usize, usize)> = None;
    for (idx, cand) in unique.iter().enumerate() {
        if !reachable.contains(&cand.peeled) {
            continue;
        }
        let distance = depths[idx];
        match best {
            None => best = Some((idx, distance)),
            Some((_best_idx, best_dist)) if distance < best_dist => best = Some((idx, distance)),
            Some((best_idx, best_dist))
                if distance == best_dist && tie_break_tag(cand, &unique[best_idx]) =>
            {
                best = Some((idx, distance));
            }
            _ => {}
        }
    }
    Ok(best)
}

/// All commits reachable from `head` (following parents) and their parent lists.
fn collect_describe_reachable(
    cache: &mut CommitGraphCache<'_>,
    head: ObjectId,
    first_parent: bool,
) -> Result<(Vec<ObjectId>, OidHashMap<Vec<ObjectId>>)> {
    let mut reachable_order = Vec::new();
    let mut seen = oid_hash_set();
    let mut queue = VecDeque::from([head]);
    let mut parents_of: OidHashMap<Vec<ObjectId>> = oid_hash_map();

    while let Some(cur) = queue.pop_front() {
        if !seen.insert(cur) {
            continue;
        }
        reachable_order.push(cur);
        let mut parents = cache.parents_of(cur)?;
        if first_parent {
            parents.truncate(1);
        }
        parents_of.insert(cur, parents.clone());
        for parent in parents {
            if !seen.contains(&parent) {
                queue.push_back(parent);
            }
        }
    }

    Ok((reachable_order, parents_of))
}

fn pick_winner_and_distance(
    repo: &Repository,
    commit: ObjectId,
    unique: &[TagCandidate],
    matches: &[ActiveMatch],
    first_parent: bool,
) -> Result<Option<(usize, usize)>> {
    let mut sorted: Vec<&ActiveMatch> = matches.iter().collect();
    sorted.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.found_order.cmp(&b.found_order))
    });
    let best_depth = sorted[0].depth;
    let mut best_match: Option<&ActiveMatch> = None;
    for m in sorted {
        if m.depth != best_depth {
            break;
        }
        let cand = &unique[m.candidate_index];
        match best_match {
            None => best_match = Some(m),
            Some(prev) => {
                if tie_break_tag(cand, &unique[prev.candidate_index]) {
                    best_match = Some(m);
                }
            }
        }
    }
    let Some(winner) = best_match else {
        return Ok(None);
    };
    let distance = describe_distance_count(
        repo,
        commit,
        unique[winner.candidate_index].peeled,
        first_parent,
    )?;
    Ok(Some((winner.candidate_index, distance)))
}

/// Exact `peeled..head` commit count (same as `git rev-list --count peeled..head`).
fn describe_distance_count(
    repo: &Repository,
    head: ObjectId,
    peel: ObjectId,
    first_parent: bool,
) -> Result<usize> {
    #[cfg(test)]
    describe_test_hooks::record_describe_distance_count();
    let mut cache = CommitGraphCache::with_commit_graph(repo, true);
    let mut hidden = oid_hash_set();
    let mut stack = vec![peel];
    while let Some(oid) = stack.pop() {
        if !hidden.insert(oid) {
            continue;
        }
        let parents = cache.parents_of(oid)?;
        let iter: Box<dyn Iterator<Item = ObjectId>> = if first_parent {
            Box::new(parents.into_iter().take(1))
        } else {
            Box::new(parents.into_iter())
        };
        stack.extend(iter);
    }

    let mut seen = oid_hash_set();
    let mut pending = vec![head];
    let mut count = 0usize;
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if hidden.contains(&oid) {
            continue;
        }
        count += 1;
        let parents = cache.parents_of(oid)?;
        let iter: Box<dyn Iterator<Item = ObjectId>> = if first_parent {
            Box::new(parents.into_iter().take(1))
        } else {
            Box::new(parents.into_iter())
        };
        pending.extend(iter);
    }
    Ok(count)
}

fn describe_walk_can_stop(
    cur_within: &WithinBits,
    best_within: &WithinBits,
    heap: &BinaryHeap<PaintKey>,
    commit_flags: &OidHashMap<WithinBits>,
) -> bool {
    if heap.is_empty() {
        return cur_within.covers(best_within);
    }
    heap.iter().all(|key| {
        commit_flags
            .get(&key.oid)
            .is_some_and(|pending| pending.covers(best_within))
    })
}

fn register_tag_match(
    matches: &mut Vec<ActiveMatch>,
    annotated_match_count: &mut usize,
    unique: &[TagCandidate],
    candidate_index: usize,
    depth: usize,
) {
    let found_order = candidate_index + 1;
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
pub(crate) mod describe_test_hooks {
    use std::cell::Cell;

    thread_local! {
        static DESCRIBE_DISTANCE_COUNTS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset_describe_distance_counts() {
        DESCRIBE_DISTANCE_COUNTS.with(|c| c.set(0));
    }

    pub fn describe_distance_counts() -> usize {
        DESCRIBE_DISTANCE_COUNTS.with(|c| c.get())
    }

    pub(crate) fn record_describe_distance_count() {
        DESCRIBE_DISTANCE_COUNTS.with(|c| c.set(c.get().saturating_add(1)));
    }
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

    fn tag_annotated_with_tagger_time(
        repo: &Repository,
        name: &str,
        target: ObjectId,
        msg: &str,
        unix_time: i64,
    ) {
        use time::{OffsetDateTime, UtcOffset};
        let when = OffsetDateTime::from_unix_timestamp(unix_time)
            .expect("time")
            .to_offset(UtcOffset::UTC);
        let tagger = Signature::new("T", "t@e.com", when).expect("tagger");
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

    /// Merge topology with committer-date skew and no commit-graph (grit objects, Git oracle).
    #[test]
    fn describe_clock_skew_merge_without_commit_graph() -> Result<()> {
        use crate::refs;
        use std::process::Command;

        let (dir, repo) = init_repo();
        let odb = repo.odb();
        let base = write_commit_at(odb, &[], "base", 3129)?;
        let tagged = write_commit_at(odb, &[base], "tagged", 2598)?;
        let side = write_commit_at(odb, &[tagged], "side", 4411)?;
        let merge = write_commit_at(odb, &[base, side], "merge", 5000)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        tag_annotated_with_tagger_time(&repo, "t-tagged", tagged, "near", 1_000);
        tag_annotated_with_tagger_time(&repo, "t-base", base, "far", 9_000);

        let git_count = Command::new("git")
            .args([
                "-C",
                dir.path().to_str().expect("utf8"),
                "rev-list",
                "--count",
                "t-tagged..HEAD",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<usize>().ok())
            .expect("git rev-list oracle");

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
        assert_eq!(tag_short_name(&d), "t-tagged");
        assert_eq!(d.peeled, tagged);
        assert_eq!(d.distance, git_count);
        assert_eq!(d.distance, 2);
        Ok(())
    }

    /// Skewed committer dates via `git commit-tree` environment (not stdin headers).
    #[test]
    fn describe_clock_skew_git_commit_tree_env_dates() -> Result<()> {
        use crate::config::environment::RepositoryOptions;
        use std::process::Command;

        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path();
        assert!(Command::new("git")
            .arg("-C")
            .arg(wt)
            .arg("init")
            .arg("-q")
            .status()
            .expect("git init")
            .success());
        let tree = String::from_utf8(
            Command::new("git")
                .args(["-C", wt.to_str().expect("utf8"), "mktree"])
                .output()
                .expect("mktree")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_owned();
        let commit_tree = |parents: &[&str], time: i64, msg: &str| -> String {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(wt)
                .arg("commit-tree")
                .arg(&tree)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@e.com")
                .env("GIT_COMMITTER_EMAIL", "t@e.com")
                .env("GIT_AUTHOR_DATE", format!("@{time} +0000"))
                .env("GIT_COMMITTER_DATE", format!("@{time} +0000"))
                .arg("-m")
                .arg(msg);
            for p in parents {
                cmd.arg("-p").arg(p);
            }
            String::from_utf8(cmd.output().expect("commit-tree").stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        let base = commit_tree(&[], 3129, "base");
        let tagged = commit_tree(&[&base], 2598, "tagged");
        let side = commit_tree(&[&tagged], 4411, "side");
        let merge = commit_tree(&[&base, &side], 5000, "merge");
        assert!(Command::new("git")
            .args([
                "-C",
                wt.to_str().expect("utf8"),
                "update-ref",
                "refs/heads/main",
                &merge
            ])
            .status()
            .expect("update-ref")
            .success());
        assert!(Command::new("git")
            .args([
                "-C",
                wt.to_str().expect("utf8"),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ])
            .status()
            .expect("symbolic-ref")
            .success());
        assert!(Command::new("git")
            .args([
                "-C",
                wt.to_str().expect("utf8"),
                "tag",
                "-a",
                "t-tagged",
                "-m",
                "tag",
                &tagged,
            ])
            .status()
            .expect("tag t-tagged")
            .success());
        assert!(Command::new("git")
            .args([
                "-C",
                wt.to_str().expect("utf8"),
                "tag",
                "-a",
                "t-base",
                "-m",
                "tag",
                &base,
            ])
            .status()
            .expect("tag t-base")
            .success());

        let git_count = String::from_utf8(
            Command::new("git")
                .args([
                    "-C",
                    wt.to_str().expect("utf8"),
                    "rev-list",
                    "--count",
                    "t-tagged..HEAD",
                ])
                .output()
                .expect("rev-list")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .parse::<usize>()
        .expect("count");

        let repo = Repository::open(
            &wt.join(".git"),
            &RepositoryOptions::empty().with_work_tree(wt),
        )?;
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
        assert_eq!(tag_short_name(&d), "t-tagged");
        assert_eq!(d.distance, git_count);
        assert_eq!(d.distance, 2);
        Ok(())
    }

    /// Two reachable tags, skewed committer dates, no commit-graph: exact minimum is not the
    /// tag with the newer tagger date (base=3, tagged=2 in Git's rev-list sense).
    #[test]
    fn describe_two_tag_skew_picks_exact_minimum_without_commit_graph() -> Result<()> {
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let base = write_commit_at(odb, &[], "base", 3129)?;
        let tagged = write_commit_at(odb, &[base], "tagged", 2598)?;
        let side = write_commit_at(odb, &[tagged], "side", 4411)?;
        let merge = write_commit_at(odb, &[base, side], "merge", 5000)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        tag_annotated_with_tagger_time(&repo, "t-base", base, "base tag", 9_999);
        tag_annotated_with_tagger_time(&repo, "t-tagged", tagged, "near tag", 1_000);

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
        assert_eq!(tag_short_name(&d), "t-tagged");
        assert_eq!(d.peeled, tagged);
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

    /// Without a commit-graph, distance must come from one fixpoint pass (not per-tag counts).
    #[test]
    fn describe_no_commit_graph_avoids_per_tag_distance_walks() -> Result<()> {
        use super::describe_test_hooks::{
            describe_distance_counts, reset_describe_distance_counts,
        };

        let (_dir, repo) = init_repo();
        let mut chain = Vec::new();
        for i in 0..80 {
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
        for (i, oid) in chain.iter().enumerate().step_by(3) {
            tag_annotated(&repo, &format!("t{i}"), *oid, "m");
        }

        reset_describe_distance_counts();
        let d = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .expect("described");
        assert_eq!(d.distance, 1);
        assert_eq!(
            describe_distance_counts(),
            0,
            "no-graph describe must not run a full rev-list count per tag candidate"
        );
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
