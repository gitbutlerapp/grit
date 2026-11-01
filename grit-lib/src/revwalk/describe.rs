//! Nearest-tag description of a commit ([`RevWalk::describe`]).
//!
//! Chooses a reachable tag with minimum exact range count (`tag..commit`), not Git
//! `describe`'s lazy depth estimate.

use crate::commit_graph_file::CommitGraphChain;
use crate::error::Result;
use crate::merge_base::{
    branch_tips_merged_into, count_commits_reachable_excluding_ancestors_of, ReachableCountFilter,
};
use crate::objects::ObjectId;
use crate::refs::listing::{
    creator_date, query_refs, RefKind, RefListing, RefNamespace, RefQuery, RefQueryError,
    RefQueryOptions,
};
use crate::refs::store::RefName;
use crate::repo::Repository;
use crate::rev_parse::{peel_to_commit_for_merge_base, resolve_revision};
use crate::OidAbbrevBatch;

use super::RevWalk;

const UNRANKED_COMMIT: u64 = (1u64 << 63) - 1;

/// Options for [`RevWalk::describe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescribeOptions {
    /// When true, lightweight tags are candidates; when false, only annotated tags.
    pub tags: bool,
    /// Optional glob matched against the tag short name (same rules as ref queries).
    pub match_glob: Option<String>,
    /// Count distance along first-parent chains only.
    pub first_parent: bool,
    /// Minimum abbreviated oid length; `None` uses 7.
    pub abbrev: Option<usize>,
}

/// Result of a successful describe: nearest tag by exact range count.
#[derive(Debug, Clone, PartialEq, Eq)]
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

        let peels: Vec<ObjectId> = candidates.iter().map(|c| c.peeled).collect();
        let ancestor_tags = branch_tips_merged_into(repo, &[commit], &peels)?;
        let reachable: Vec<&TagCandidate> = candidates
            .iter()
            .filter(|c| c.peeled == commit || ancestor_tags.contains(&c.peeled))
            .collect();
        if reachable.is_empty() {
            return Ok(None);
        }

        let filter = ReachableCountFilter {
            first_parent_only: opts.first_parent,
            ..ReachableCountFilter::default()
        };
        let graph = repo.odb().commit_graph_chain();
        let mut best: Option<(usize, &TagCandidate)> = None;

        for cand in reachable {
            if let Some((best_dist, _)) = best {
                if let Some(lower) =
                    generation_distance_lower_bound(graph.as_deref(), cand.peeled, commit)
                {
                    if lower > best_dist {
                        continue;
                    }
                }
            }
            let dist = count_commits_reachable_excluding_ancestors_of(
                repo,
                &[commit],
                &[cand.peeled],
                filter,
            )?;
            match best {
                None => best = Some((dist, cand)),
                Some((best_dist, _)) if dist < best_dist => {
                    best = Some((dist, cand));
                }
                Some((best_dist, best_cand))
                    if dist == best_dist && tie_break_tag(cand, best_cand) =>
                {
                    best = Some((dist, cand));
                }
                _ => {}
            }
        }

        let Some((distance, winner)) = best else {
            return Ok(None);
        };
        let abbrev_len = opts.abbrev.unwrap_or(7);
        let mut abbrev_batch = OidAbbrevBatch::new(abbrev_len);
        let abbrev = abbrev_batch.abbrev_known(repo, commit)?;
        let tag = RefName::try_from(winner.listing.full_name.as_str())
            .map_err(|e| crate::error::Error::InvalidRef(e.to_string()))?;
        Ok(Some(DescribedCommit {
            tag,
            tag_oid: winner.listing.oid,
            peeled: winner.peeled,
            distance,
            commit,
            abbrev,
        }))
    }
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
    a.listing.short_name > b.listing.short_name
}

fn generation_distance_lower_bound(
    graph: Option<&CommitGraphChain>,
    tag_commit: ObjectId,
    head: ObjectId,
) -> Option<usize> {
    let graph = graph?;
    let gen_tag = graph.commit_generation_for_walk(&tag_commit);
    let gen_head = graph.commit_generation_for_walk(&head);
    if gen_tag == UNRANKED_COMMIT || gen_head == UNRANKED_COMMIT {
        return None;
    }
    if gen_head >= gen_tag {
        Some((gen_head - gen_tag) as usize)
    } else {
        None
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
}
