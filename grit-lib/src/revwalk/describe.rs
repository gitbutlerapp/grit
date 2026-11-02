//! Nearest-tag description of a commit ([`RevWalk::describe`]).
//!
//! Chooses a reachable tag with minimum exact range count (`tag..commit`), not Git
//! `describe`'s lazy depth estimate. One commit-graph-ordered walk finds every reachable
//! tag distance instead of a separate rev-list count per tag. Within bits mark tag ancestry
//! by fixpoint propagation from peels, so counts match `git rev-list --count` without
//! relying on commit-graph rank or walk order.

use std::collections::VecDeque;

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
}

impl<'repo> RevWalk<'repo> {
    /// Find the nearest reachable tag for `rev` using [`DescribeOptions`].
    ///
    /// Returns `None` when no tag peel lies on the walked history to `rev` (all parents, or
    /// first-parent only when [`DescribeOptions::first_parent`] is set) or equals `rev`.
    /// Blob/tree tags are skipped.
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

/// Exact range counts via ancestor marking (fixpoint within propagation from tag peels).
fn nearest_tag_by_walk(
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

    /// Side branch carries the only tag; first-parent describe must not admit it (Git exits 128).
    #[test]
    fn describe_first_parent_only_side_tag_returns_none() -> Result<()> {
        use crate::refs;

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let root = write_commit_at(odb, &[], "root", 1)?;
        let main_tip = write_commit_at(odb, &[root], "main", 2)?;
        let side = write_commit_at(odb, &[root], "side", 3)?;
        let merge = write_commit_at(odb, &[main_tip, side], "merge", 4)?;
        refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        tag_annotated(&repo, "side-tag", side, "side only");
        assert!(repo
            .rev_walk()
            .describe(
                "HEAD",
                &DescribeOptions {
                    tags: true,
                    first_parent: true,
                    ..Default::default()
                },
            )?
            .is_none());
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

    /// Committer dates on a merge arm can order a shared parent before a tagged descendant;
    /// distance must still match an exact `peeled..HEAD` rev-list count.
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

    /// Merge-heavy history (git/git-shaped): parent reads must not scale with tag count.
    #[test]
    fn describe_merge_heavy_parent_reads_bounded() -> Result<()> {
        use crate::merge_base::intersect_walk_test_hooks::{
            commit_parent_queries, reset_commit_parent_queries,
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
            if path % 8 == 0 {
                tag_annotated(&repo, &format!("decoy-{path}"), cur, "decoy");
            }
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

        reset_commit_parent_queries();
        assert!(repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?
            .is_some());
        let lookups = commit_parent_queries();
        assert!(
            lookups <= 5_000,
            "merge-heavy describe must stay linear in history size, not tags × commits ({lookups} parent reads)"
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

    #[test]
    fn describe_seed6593_parent_reads_bounded_without_commit_graph() -> Result<()> {
        use crate::merge_base::intersect_walk_test_hooks::{
            commit_parent_queries, reset_commit_parent_queries,
        };
        use crate::refs;

        fn lcg(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state
        }

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let mut seed = 6593u64;
        let root = write_commit_at(odb, &[], "root", 5000)?;
        let mut commits = vec![root];
        while commits.len() < 150 {
            let i = (lcg(&mut seed) as usize) % commits.len();
            let j = (lcg(&mut seed) as usize) % commits.len();
            let time = 1000 + (lcg(&mut seed) % 50_000) as i64;
            let parents = if i == j {
                vec![commits[i]]
            } else {
                vec![commits[i], commits[j]]
            };
            commits.push(write_commit_at(
                odb,
                &parents,
                &format!("c{}", commits.len()),
                time,
            )?);
        }
        for t in 0..15 {
            let idx = (lcg(&mut seed) as usize) % commits.len();
            tag_annotated(&repo, &format!("t{t:02}"), commits[idx], "m");
        }
        let head = *commits.last().expect("tip");
        refs::write_ref(repo.git_dir(), "refs/heads/main", &head)?;

        reset_commit_parent_queries();
        let _ = repo
            .rev_walk()
            .describe("HEAD", &DescribeOptions::default())?;
        let queries = commit_parent_queries();
        assert!(
            queries <= 600,
            "describe on seed-6593 fixture must not scale with tag count × history ({queries} parent reads)"
        );
        Ok(())
    }

    #[test]
    fn seed6593_walk_depths_match_exact_count_helper() -> Result<()> {
        use crate::merge_base::{
            count_commits_reachable_excluding_ancestors_of, ReachableCountFilter,
        };
        use crate::refs;

        fn lcg(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state
        }

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let mut seed = 6593u64;
        let root = write_commit_at(odb, &[], "root", 5000)?;
        let mut commits = vec![root];
        while commits.len() < 150 {
            let i = (lcg(&mut seed) as usize) % commits.len();
            let j = (lcg(&mut seed) as usize) % commits.len();
            let time = 1000 + (lcg(&mut seed) % 50_000) as i64;
            let parents = if i == j {
                vec![commits[i]]
            } else {
                vec![commits[i], commits[j]]
            };
            commits.push(write_commit_at(
                odb,
                &parents,
                &format!("c{}", commits.len()),
                time,
            )?);
        }
        for t in 0..15 {
            let idx = (lcg(&mut seed) as usize) % commits.len();
            tag_annotated(&repo, &format!("t{t:02}"), commits[idx], "m");
        }
        let mut sample_seed = 6593u64 ^ 0xDEAD_BEEF;
        let head_idx = (lcg(&mut sample_seed) as usize) % commits.len();
        let head = commits[head_idx];
        refs::write_ref(repo.git_dir(), "refs/heads/main", &head)?;

        let opts = DescribeOptions::default();
        let candidates = collect_tag_candidates(&repo, &opts)?;
        let (unique, peel_to_index) = dedupe_candidates_by_peel(candidates);
        let Some((winner_idx, walk_dist)) =
            nearest_tag_by_walk(&repo, head, &unique, &peel_to_index, false)?
        else {
            panic!("no winner");
        };
        let mut cache = CommitGraphCache::with_commit_graph(&repo, true);
        let (reachable_order, parents_of) = collect_describe_reachable(&mut cache, head, false)?;
        let tag_count = unique.len();
        let within_words = tag_count.max(1);
        let reachable: OidHashSet = reachable_order.iter().copied().collect();
        let mut full_within: OidHashMap<WithinBits> = oid_hash_map();
        let mut queue = VecDeque::new();
        for (&peel, &pidx) in &peel_to_index {
            if !reachable.contains(&peel) {
                continue;
            }
            full_within
                .entry(peel)
                .or_insert_with(|| WithinBits::with_candidates(within_words))
                .set(pidx);
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
        for (idx, cand) in unique.iter().enumerate() {
            let expected = count_commits_reachable_excluding_ancestors_of(
                &repo,
                &[head],
                &[cand.peeled],
                ReachableCountFilter::default(),
            )?;
            if !reachable.contains(&cand.peeled) {
                continue;
            }
            assert_eq!(
                depths[idx],
                expected,
                "tag {} peel {}",
                cand.listing.short_name,
                cand.peeled.to_hex()
            );
        }
        let mut min_ancestor = usize::MAX;
        for cand in &unique {
            if !reachable.contains(&cand.peeled) {
                continue;
            }
            let d = count_commits_reachable_excluding_ancestors_of(
                &repo,
                &[head],
                &[cand.peeled],
                ReachableCountFilter::default(),
            )?;
            min_ancestor = min_ancestor.min(d);
        }
        assert_eq!(walk_dist, min_ancestor, "winner_idx={winner_idx}");
        Ok(())
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
