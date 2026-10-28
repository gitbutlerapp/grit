//! What upload-pack does with objects the served repository does not have.
//!
//! A server that keeps its large blobs on a large-object promisor (LOP) is a
//! partial repository of that LOP: the blobs are missing locally, and the trees
//! that name them sit in promisor packs. Git's rule for serving such a repository:
//! when the client accepted at least one advertised promisor remote, leave promised
//! objects out of the pack (the client fetches them from the LOP); otherwise fail,
//! unless lazy fetch was explicitly enabled, in which case the server fetches them
//! first. A missing object nothing promises is always an error.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::sync::Arc;

use super::{MissingObjectReason, Result, ServeError};
use crate::objects::ObjectId;
use crate::pack_index::PackIndex;
use crate::pack_object_select::{append_included_tags, PackObjectSelection, TagCandidate};
use crate::pack_objects::{select_upload_pack_objects_probing, PackBuildOptions};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;

/// How the pack planner treats objects the repository lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingObjects {
    /// Fail the request ([`super::MissingObjectReason::Promised`] or
    /// [`super::MissingObjectReason::NotPromised`]).
    Error,
    /// Leave promised objects out of the pack; the client fetches them from a
    /// promisor remote it accepted.
    OmitPromised,
    /// Fetch them from the repository's promisor remotes first, then send them.
    LazyFetch,
}

/// The rule for missing objects in one fetch.
///
/// # Parameters
///
/// - `repo_is_partial`: the served repository has promisor remotes
///   ([`crate::promisor::repo_treats_promisor_packs`]).
/// - `accepted`: the client accepted at least one advertised promisor remote.
/// - `lazy_fetch`: the server may fetch missing objects from its promisor remotes.
///
/// # Returns
///
/// `None` for a repository without promisor remotes, which keeps the ordinary
/// pack path (bitmaps, verbatim pack reuse) untouched. As in Git, an accepting
/// client takes precedence over lazy fetch: the server does not fetch what the
/// client can get from the LOP itself.
pub(crate) fn missing_objects_policy(
    repo_is_partial: bool,
    accepted: bool,
    lazy_fetch: bool,
) -> Option<MissingObjects> {
    if !repo_is_partial {
        return None;
    }
    Some(if accepted {
        MissingObjects::OmitPromised
    } else if lazy_fetch {
        MissingObjects::LazyFetch
    } else {
        MissingObjects::Error
    })
}

/// Answers "is this missing object promised?" for one repository, the way Git's
/// `is_promisor_object` does: an object is promised when an object stored in a
/// promisor pack refers to it.
///
/// The common case is answered from pack indexes alone: the tree that named the
/// blob is itself in a promisor pack. Otherwise the full set of objects referred
/// to directly from promisor packs
/// ([`crate::promisor::promisor_referenced_objects`]) is computed once (reading
/// every promisor commit, tag and tree) and reused for the rest of the request.
pub(crate) struct PromisorObjects<'a> {
    repo: &'a Repository,
    promisor_indexes: Vec<Arc<PackIndex>>,
    referenced: OnceCell<HashSet<ObjectId>>,
}

impl<'a> PromisorObjects<'a> {
    /// Index the promisor packs of `repo`.
    ///
    /// # Errors
    ///
    /// Pack directory listing or index parse failures.
    pub(crate) fn new(repo: &'a Repository) -> crate::error::Result<Self> {
        let promisor_indexes = if repo.odb().uses_files_primary() {
            repo.odb()
                .local_pack_indexes()?
                .into_iter()
                .filter(|idx| idx.is_promisor)
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            repo,
            promisor_indexes,
            referenced: OnceCell::new(),
        })
    }

    /// Whether `oid` is stored in a promisor pack.
    pub(crate) fn in_promisor_pack(&self, oid: &ObjectId) -> bool {
        self.promisor_indexes
            .iter()
            .any(|idx| idx.find_position(oid).is_some())
    }

    /// Whether the missing object `oid` is promised.
    ///
    /// # Parameters
    ///
    /// - `oid`: the missing object.
    /// - `referrer`: the object the walk found it in (the tree naming a blob), when
    ///   known; a referrer stored in a promisor pack settles it without a scan.
    ///
    /// # Errors
    ///
    /// Read or parse failures while computing the full promise set.
    pub(crate) fn is_promised(
        &self,
        oid: &ObjectId,
        referrer: Option<&ObjectId>,
    ) -> crate::error::Result<bool> {
        if self.promisor_indexes.is_empty() && self.referenced.get().is_none() {
            // Only the recorded-missing list can promise anything then.
            return Ok(self.referenced()?.contains(oid));
        }
        if referrer.is_some_and(|r| self.in_promisor_pack(r)) || self.in_promisor_pack(oid) {
            return Ok(true);
        }
        Ok(self.referenced()?.contains(oid))
    }

    fn referenced(&self) -> crate::error::Result<&HashSet<ObjectId>> {
        if let Some(set) = self.referenced.get() {
            return Ok(set);
        }
        let set = crate::promisor::promisor_referenced_objects(self.repo.odb())?;
        Ok(self.referenced.get_or_init(|| set))
    }
}

/// Choose the objects for a response under `mode`, before any of it is written:
/// promised blobs the repository lacks are left out ([`MissingObjects::OmitPromised`])
/// or fetched, after which the pack is planned again so the request's filter
/// applies to them too ([`MissingObjects::LazyFetch`]). The `include_tags` whose
/// peeled object is sent are added last, as on the streaming path.
///
/// # Errors
///
/// [`ServeError::ObjectMissing`] when a missing blob cannot be omitted or fetched,
/// and read failures from the walk.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_pack(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    include_tags: &[TagCandidate],
    pack_opts: &PackBuildOptions,
    mode: MissingObjects,
) -> Result<PackObjectSelection> {
    let (mut selection, missing) = select_upload_pack_objects_probing(
        repo,
        wants,
        haves,
        source_shallow,
        have_shallow,
        filter,
        pack_opts,
    )?;
    match mode {
        _ if missing.is_empty() => {}
        MissingObjects::OmitPromised => {
            let promised = PromisorObjects::new(repo)?;
            for blob in &missing {
                if !promised.is_promised(&blob.oid, blob.referrer.as_ref())? {
                    return Err(missing_error(blob.oid, MissingObjectReason::NotPromised));
                }
            }
        }
        MissingObjects::Error => {
            let promised = PromisorObjects::new(repo)?;
            let first = missing[0];
            let reason = if promised.is_promised(&first.oid, first.referrer.as_ref())? {
                MissingObjectReason::Promised
            } else {
                MissingObjectReason::NotPromised
            };
            return Err(missing_error(first.oid, reason));
        }
        MissingObjects::LazyFetch => {
            let oids: Vec<ObjectId> = missing.iter().map(|b| b.oid).collect();
            fetch_from_promisor_remotes(repo, &oids)?;
            // The walk could not size the missing blobs, so it let them through the
            // filter. Plan again now that they are here: a client that asked for
            // `blob:limit` still does not get the large ones, as from Git.
            let (replanned, still_missing) = select_upload_pack_objects_probing(
                repo,
                wants,
                haves,
                source_shallow,
                have_shallow,
                filter,
                pack_opts,
            )?;
            if let Some(blob) = still_missing.first() {
                return Err(missing_error(
                    blob.oid,
                    MissingObjectReason::LazyFetchFailed,
                ));
            }
            selection = replanned;
        }
    }
    append_included_tags(repo.odb(), &mut selection.send, include_tags)?;
    Ok(selection)
}

/// Deal with wanted objects the repository does not store, before the wants are
/// validated: under [`MissingObjects::LazyFetch`] fetch them, under
/// [`MissingObjects::OmitPromised`] drop the promised ones (the client then gets
/// a pack without them, as from Git), and under [`MissingObjects::Error`] fail
/// for a promised one. Wants nothing promises are left for want validation to
/// refuse.
///
/// # Errors
///
/// [`ServeError::ObjectMissing`] as described, and fetch or read failures.
pub(crate) fn resolve_missing_wants(
    repo: &Repository,
    wants: &mut Vec<ObjectId>,
    mode: MissingObjects,
) -> Result<()> {
    let missing = repo.odb().missing_oids(wants.iter().copied())?;
    if missing.is_empty() {
        return Ok(());
    }
    if mode == MissingObjects::LazyFetch {
        return fetch_from_promisor_remotes(repo, &missing);
    }
    let promised = PromisorObjects::new(repo)?;
    let mut omit = HashSet::new();
    for oid in missing {
        if !promised.is_promised(&oid, None)? {
            continue;
        }
        if mode == MissingObjects::Error {
            return Err(missing_error(oid, MissingObjectReason::Promised));
        }
        omit.insert(oid);
    }
    wants.retain(|oid| !omit.contains(oid));
    Ok(())
}

/// Fetch `oids` through the repository's promisor fetcher and check they arrived.
fn fetch_from_promisor_remotes(repo: &Repository, oids: &[ObjectId]) -> Result<()> {
    let Some(fetcher) = repo.caches().promisor_fetcher() else {
        return Err(missing_error(oids[0], MissingObjectReason::NoFetcher));
    };
    fetcher.fetch_missing(repo, oids)?;
    let odb = repo.odb();
    odb.reprepare_local_packs();
    odb.invalidate_packs();
    if let Some(oid) = odb.missing_oids(oids.iter().copied())?.first() {
        return Err(missing_error(*oid, MissingObjectReason::LazyFetchFailed));
    }
    Ok(())
}

fn missing_error(oid: ObjectId, reason: MissingObjectReason) -> ServeError {
    ServeError::ObjectMissing { oid, reason }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::objects::ObjectKind;
    use crate::repo::InitOptions;

    #[test]
    fn policy_truth_table() {
        assert_eq!(missing_objects_policy(false, false, false), None);
        assert_eq!(missing_objects_policy(false, true, true), None);
        assert_eq!(
            missing_objects_policy(true, false, false),
            Some(MissingObjects::Error)
        );
        assert_eq!(
            missing_objects_policy(true, false, true),
            Some(MissingObjects::LazyFetch)
        );
        assert_eq!(
            missing_objects_policy(true, true, false),
            Some(MissingObjects::OmitPromised)
        );
        // An accepting client fetches from the LOP itself; the server does not.
        assert_eq!(
            missing_objects_policy(true, true, true),
            Some(MissingObjects::OmitPromised)
        );
    }

    /// Write a tree naming `blob` and a commit on it into one pack, marked promisor
    /// when `promisor`, with no loose copies left; returns the tree.
    fn packed_tree(repo: &Repository, blob: ObjectId, promisor: bool) -> ObjectId {
        let odb = repo.odb();
        let mut body = b"100644 big\0".to_vec();
        body.extend_from_slice(blob.as_bytes());
        let tree = odb
            .write_loose_materialize(ObjectKind::Tree, &body)
            .unwrap();
        let commit =
            format!("tree {tree}\nauthor A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\nc\n");
        let commit = odb
            .write_loose_materialize(ObjectKind::Commit, commit.as_bytes())
            .unwrap();
        let pack = crate::pack_objects::build_pack_from_send_list(
            odb,
            &[commit, tree],
            &HashSet::new(),
            &crate::pack_objects::PackBuildOptions::default(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
        crate::index_pack::install_pack_bytes(
            pack,
            odb,
            &crate::index_pack::IngestPackOptions {
                mark_promisor_pack: promisor,
                ..Default::default()
            },
        )
        .unwrap();
        crate::prune_packed::prune_packed_objects(odb.objects_dir(), Default::default()).unwrap();
        tree
    }

    #[test]
    fn referrer_in_a_promisor_pack_settles_it_without_a_scan() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let blob = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let tree = packed_tree(&repo, blob, true);
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).unwrap();
        let promised = PromisorObjects::new(&repo).unwrap();
        assert!(promised.in_promisor_pack(&tree));
        assert!(promised.is_promised(&blob, Some(&tree)).unwrap());
        assert!(promised.referenced.get().is_none(), "fast path only");
        // Without the referrer the scan finds the tree's entry too.
        assert!(promised.is_promised(&blob, None).unwrap());
        assert!(promised.referenced.get().is_some());
        let other = ObjectId::from_hex("1123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(!promised.is_promised(&other, None).unwrap());
    }

    #[test]
    fn blob_named_only_by_ordinary_packs_is_not_promised() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let blob = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let tree = packed_tree(&repo, blob, false);
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).unwrap();
        let promised = PromisorObjects::new(&repo).unwrap();
        assert!(!promised.in_promisor_pack(&tree));
        assert!(!promised.is_promised(&blob, Some(&tree)).unwrap());
    }

    /// Pack `oids` into one pack (promisor when `promisor`) and drop their loose copies.
    fn pack(repo: &Repository, oids: &[ObjectId], promisor: bool) {
        let odb = repo.odb();
        let pack = crate::pack_objects::build_pack_from_send_list(
            odb,
            oids,
            &HashSet::new(),
            &crate::pack_objects::PackBuildOptions::default(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
        crate::index_pack::install_pack_bytes(
            pack,
            odb,
            &crate::index_pack::IngestPackOptions {
                mark_promisor_pack: promisor,
                ..Default::default()
            },
        )
        .unwrap();
        crate::prune_packed::prune_packed_objects(odb.objects_dir(), Default::default()).unwrap();
    }

    #[test]
    fn promise_does_not_reach_past_the_objects_promisor_packs_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let odb = repo.odb();
        let tree = |blob: ObjectId| {
            let mut body = b"100644 f\0".to_vec();
            body.extend_from_slice(blob.as_bytes());
            odb.write_loose_materialize(ObjectKind::Tree, &body)
                .unwrap()
        };
        let commit = |tree: ObjectId, parent: Option<ObjectId>| {
            let parent = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
            let body = format!(
                "tree {tree}\n{parent}author A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\nc\n"
            );
            odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
                .unwrap()
        };
        // Older history in an ordinary pack names a blob nobody has.
        let lost = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let old_tree = tree(lost);
        let old = commit(old_tree, None);
        pack(&repo, &[old, old_tree], false);
        // A promisor pack on top of it promises another blob.
        let promised_blob = ObjectId::from_hex("1123456789abcdef0123456789abcdef01234567").unwrap();
        let new_tree = tree(promised_blob);
        let new = commit(new_tree, Some(old));
        pack(&repo, &[new, new_tree], true);

        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).unwrap();
        let promised = PromisorObjects::new(&repo).unwrap();
        assert!(promised.is_promised(&promised_blob, None).unwrap());
        assert!(
            promised.is_promised(&old, None).unwrap(),
            "a promisor commit's parent"
        );
        assert!(
            !promised.is_promised(&lost, Some(&old_tree)).unwrap(),
            "named only by an ordinary-pack tree"
        );
    }
}
