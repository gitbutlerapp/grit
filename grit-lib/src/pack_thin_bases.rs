//! Delta bases for thin packs: what the receiver already holds at the paths being sent.
//!
//! A thin pack may delta against objects it does not carry. The best such bases for a
//! changed tree or blob are the objects at the same path in the trees of the *edge
//! commits*: the receiver-held parents of the commits being sent. These play the role of
//! Git's preferred bases: the delta search may pick them, the pack never contains them,
//! and a delta against one is written as `REF_DELTA` for the receiver to resolve.
//!
//! Paths are matched by their pack name hash, and only the receiver's trees on the way
//! to changed paths are read, so the cost follows the size of the change and not the
//! size of the repository.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::commit_graph_file::CommitGraphChain;
use crate::error::Result;
use crate::objects::{
    oid_hash_set, CommitRef, ObjectId, ObjectInfo, ObjectKind, OidHashSet, TreeRefIter,
};
use crate::odb::Odb;
use crate::pack_name_hash::pack_name_hash_extend;

/// A receiver-held object offered to the delta search as a base for a sent object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThinBase {
    /// The base object.
    pub(crate) oid: ObjectId,
    /// Its kind ([`ObjectKind::Tree`] or [`ObjectKind::Blob`]).
    pub(crate) kind: ObjectKind,
    /// Name hash of its path (`0` for root trees), which is also the name hash of the
    /// sent objects it pairs with, so they share a delta-search group.
    pub(crate) name_hash: u32,
}

/// Bases found by [`find_thin_bases`].
#[derive(Debug, Default)]
pub(crate) struct ThinBases {
    /// Each base once, in the order found (shallow paths first).
    pub(crate) bases: Vec<ThinBase>,
    /// The ids of [`Self::bases`].
    pub(crate) oids: OidHashSet,
    /// Sent objects that at least one base was found for.
    pub(crate) targets: OidHashSet,
}

impl ThinBases {
    fn add(&mut self, oid: ObjectId, kind: ObjectKind, name_hash: u32) {
        if self.oids.insert(oid) {
            self.bases.push(ThinBase {
                oid,
                kind,
                name_hash,
            });
        }
    }
}

/// Find receiver-held delta bases for the trees and blobs of a thin pack.
///
/// The edge commits are the parents of the sent commits (those in `send` whose kind in
/// `infos` is commit) that `peer_has_commit` accepts and that are not sent themselves,
/// in `send` order, at most `max_edges` of them (Git caps its preferred-base trees by the
/// delta window the same way). Their root trees are bases for the sent root trees.
/// Below the root, a held tree or blob is a base when a sent object of the same kind has
/// that path's name hash in `name_hashes`; the walk only descends into held directories
/// whose path matches a sent tree, which are the directories the change touches.
///
/// Sent objects without a name hash get no base below the root. Objects missing from
/// `odb` are skipped.
///
/// # Errors
///
/// Propagates commit and tree parse failures.
pub(crate) fn find_thin_bases(
    odb: &Odb,
    send: &[ObjectId],
    infos: &[ObjectInfo],
    in_send: &dyn Fn(&ObjectId) -> bool,
    peer_has_commit: &dyn Fn(&ObjectId) -> bool,
    name_hashes: &HashMap<ObjectId, u32>,
    max_edges: usize,
) -> Result<ThinBases> {
    let mut out = ThinBases::default();
    if max_edges == 0 {
        return Ok(out);
    }
    let graph = odb.commit_graph_chain();
    let graph = graph.as_deref();
    let mut sent_roots = oid_hash_set();
    let mut edges = Vec::new();
    let mut edge_seen = oid_hash_set();
    for (oid, info) in send.iter().zip(infos) {
        if info.kind != ObjectKind::Commit {
            continue;
        }
        let Some((tree, parents)) = commit_tree_and_parents(odb, graph, oid)? else {
            continue;
        };
        if in_send(&tree) {
            sent_roots.insert(tree);
        }
        for parent in parents {
            if edges.len() < max_edges
                && !in_send(&parent)
                && peer_has_commit(&parent)
                && edge_seen.insert(parent)
            {
                edges.push(parent);
            }
        }
    }
    let mut held_roots = Vec::with_capacity(edges.len());
    for edge in &edges {
        if let Some((tree, _)) = commit_tree_and_parents(odb, graph, edge)? {
            if !in_send(&tree) && !held_roots.contains(&tree) {
                held_roots.push(tree);
            }
        }
    }
    if held_roots.is_empty() {
        return Ok(out);
    }

    let mut sent_tree_paths: HashSet<u32> = HashSet::new();
    let mut sent_blob_paths: HashSet<u32> = HashSet::new();
    for (oid, info) in send.iter().zip(infos) {
        let paths = match info.kind {
            ObjectKind::Tree if !sent_roots.contains(oid) => &mut sent_tree_paths,
            ObjectKind::Blob => &mut sent_blob_paths,
            _ => continue,
        };
        if let Some(&hash) = name_hashes.get(oid) {
            paths.insert(hash);
        }
    }

    if !sent_roots.is_empty() {
        for &root in &held_roots {
            out.add(root, ObjectKind::Tree, 0);
        }
    }
    let oid_len = odb.hash_algo().len();
    let mut found_tree_paths: HashSet<u32> = HashSet::new();
    let mut found_blob_paths: HashSet<u32> = HashSet::new();
    let mut visited = oid_hash_set();
    let mut queue: VecDeque<(ObjectId, Option<u32>)> =
        held_roots.iter().map(|&root| (root, None)).collect();
    while let Some((tree_oid, dir_hash)) = queue.pop_front() {
        if !visited.insert(tree_oid) {
            continue;
        }
        let Ok(tree) = odb.read(&tree_oid) else {
            continue;
        };
        if tree.kind != ObjectKind::Tree {
            continue;
        }
        for entry in TreeRefIter::with_oid_len(&tree.data, oid_len) {
            let entry = entry?;
            let Some(is_tree) = entry_is_tree(entry.mode) else {
                continue;
            };
            let path_hash = match dir_hash {
                Some(dir) => pack_name_hash_extend(pack_name_hash_extend(dir, b"/"), entry.name),
                None => pack_name_hash_extend(0, entry.name),
            };
            let (sent_paths, found_paths, kind) = if is_tree {
                (&sent_tree_paths, &mut found_tree_paths, ObjectKind::Tree)
            } else {
                (&sent_blob_paths, &mut found_blob_paths, ObjectKind::Blob)
            };
            if !sent_paths.contains(&path_hash) || in_send(&entry.oid) {
                continue;
            }
            found_paths.insert(path_hash);
            out.add(entry.oid, kind, path_hash);
            if is_tree {
                queue.push_back((entry.oid, Some(path_hash)));
            }
        }
    }

    for (oid, info) in send.iter().zip(infos) {
        let found_paths = match info.kind {
            ObjectKind::Tree if sent_roots.contains(oid) => {
                out.targets.insert(*oid);
                continue;
            }
            ObjectKind::Tree => &found_tree_paths,
            ObjectKind::Blob => &found_blob_paths,
            _ => continue,
        };
        if name_hashes
            .get(oid)
            .is_some_and(|h| found_paths.contains(h))
        {
            out.targets.insert(*oid);
        }
    }
    Ok(out)
}

/// `Some(true)` for a directory entry, `Some(false)` for a file or symlink, `None` for
/// gitlinks and anything else that cannot be a delta base.
fn entry_is_tree(mode: u32) -> Option<bool> {
    match mode & 0o170000 {
        0o040000 => Some(true),
        0o100000 | 0o120000 => Some(false),
        _ => None,
    }
}

/// Root tree and parents of commit `oid`, from the commit-graph when it lists the commit.
/// `None` when the object is missing or not a commit.
fn commit_tree_and_parents(
    odb: &Odb,
    graph: Option<&CommitGraphChain>,
    oid: &ObjectId,
) -> Result<Option<(ObjectId, Vec<ObjectId>)>> {
    if let Some(entry) = graph.and_then(|g| g.graph_walk_entry(oid)) {
        return Ok(Some((entry.tree, entry.parents)));
    }
    let Ok(obj) = odb.read(oid) else {
        return Ok(None);
    };
    if obj.kind != ObjectKind::Commit {
        return Ok(None);
    }
    let commit = CommitRef::parse(&obj.data)?;
    Ok(Some((commit.tree(), commit.parents().to_vec())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::objects::{serialize_tree, TreeEntry};
    use crate::pack_name_hash::pack_name_hash;
    use crate::repo::{InitOptions, Repository};

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repository,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
            Self { _dir: dir, repo }
        }

        fn blob(&self, data: &str) -> ObjectId {
            self.repo
                .odb()
                .write(ObjectKind::Blob, data.as_bytes())
                .unwrap()
        }

        fn tree(&self, entries: &[(u32, &str, ObjectId)]) -> ObjectId {
            let mut entries: Vec<TreeEntry> = entries
                .iter()
                .map(|&(mode, name, oid)| TreeEntry {
                    mode,
                    name: name.as_bytes().to_vec(),
                    oid,
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            self.repo
                .odb()
                .write(ObjectKind::Tree, &serialize_tree(&entries))
                .unwrap()
        }

        fn commit(&self, tree: ObjectId, parents: &[ObjectId]) -> ObjectId {
            let mut body = format!("tree {tree}\n");
            for p in parents {
                body.push_str(&format!("parent {p}\n"));
            }
            body.push_str("author A <a@e> 0 +0000\ncommitter A <a@e> 0 +0000\n\nc\n");
            self.repo
                .odb()
                .write(ObjectKind::Commit, body.as_bytes())
                .unwrap()
        }

        /// Thin bases for `send`, where `named` gives the path of each sent tree or
        /// blob that has one.
        fn find(
            &self,
            send: &[ObjectId],
            named: &[(ObjectId, &str)],
            peer: &HashSet<ObjectId>,
            max_edges: usize,
        ) -> ThinBases {
            let odb = self.repo.odb();
            let infos: Vec<ObjectInfo> = send.iter().map(|o| odb.read_info(o).unwrap()).collect();
            let in_send: HashSet<ObjectId> = send.iter().copied().collect();
            let name_hashes: HashMap<ObjectId, u32> = named
                .iter()
                .map(|&(oid, path)| (oid, pack_name_hash(path)))
                .collect();
            find_thin_bases(
                odb,
                send,
                &infos,
                &|o| in_send.contains(o),
                &|o| peer.contains(o),
                &name_hashes,
                max_edges,
            )
            .unwrap()
        }
    }

    #[test]
    fn bases_are_the_held_objects_at_the_changed_paths() {
        let fx = Fixture::new();
        let same = fx.blob("unchanged");
        let old_lib = fx.blob("old lib");
        let new_lib = fx.blob("new lib");
        let old_readme = fx.blob("old readme");
        let new_readme = fx.blob("new readme");
        let added = fx.blob("added");
        let old_src = fx.tree(&[(0o100644, "lib.rs", old_lib), (0o100644, "same.rs", same)]);
        let new_src = fx.tree(&[
            (0o100644, "lib.rs", new_lib),
            (0o100644, "same.rs", same),
            (0o100644, "added.rs", added),
        ]);
        let old_root = fx.tree(&[(0o040000, "src", old_src), (0o100644, "README", old_readme)]);
        let new_root = fx.tree(&[(0o040000, "src", new_src), (0o100644, "README", new_readme)]);
        let edge = fx.commit(old_root, &[]);
        let tip = fx.commit(new_root, &[edge]);

        let send = [tip, new_root, new_src, new_lib, new_readme, added];
        let named = [
            (new_src, "src"),
            (new_lib, "src/lib.rs"),
            (new_readme, "README"),
            (added, "src/added.rs"),
        ];
        let found = fx.find(&send, &named, &HashSet::from([edge]), 10);

        let by_oid: HashMap<ObjectId, ThinBase> = found.bases.iter().map(|b| (b.oid, *b)).collect();
        assert_eq!(
            by_oid.keys().copied().collect::<HashSet<_>>(),
            HashSet::from([old_root, old_src, old_lib, old_readme]),
            "unchanged and added entries have no base"
        );
        assert_eq!(by_oid[&old_root].kind, ObjectKind::Tree);
        assert_eq!(by_oid[&old_root].name_hash, 0, "root trees have no path");
        assert_eq!(by_oid[&old_src].kind, ObjectKind::Tree);
        assert_eq!(by_oid[&old_src].name_hash, pack_name_hash("src"));
        assert_eq!(by_oid[&old_lib].kind, ObjectKind::Blob);
        assert_eq!(by_oid[&old_lib].name_hash, pack_name_hash("src/lib.rs"));
        assert_eq!(by_oid[&old_readme].name_hash, pack_name_hash("README"));
        assert_eq!(
            found.targets,
            [new_root, new_src, new_lib, new_readme]
                .into_iter()
                .collect(),
            "every sent object a base was found for"
        );
    }

    #[test]
    fn unchanged_directories_are_not_read_and_unnamed_objects_get_no_base() {
        let fx = Fixture::new();
        let old_deep = fx.blob("old deep");
        let new_deep = fx.blob("new deep");
        let old_dir = fx.tree(&[(0o100644, "deep.rs", old_deep)]);
        let new_dir = fx.tree(&[(0o100644, "deep.rs", new_deep)]);
        let missing_dir = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
        let old_root = fx.tree(&[(0o040000, "dir", old_dir), (0o040000, "gone", missing_dir)]);
        let new_root = fx.tree(&[(0o040000, "dir", new_dir), (0o040000, "gone", missing_dir)]);
        let edge = fx.commit(old_root, &[]);
        let tip = fx.commit(new_root, &[edge]);
        let send = [tip, new_root, new_dir, new_deep];

        // `gone` names an object the store lacks: it is only read if the walk descends.
        let found = fx.find(
            &send,
            &[(new_dir, "dir"), (new_deep, "dir/deep.rs")],
            &HashSet::from([edge]),
            10,
        );
        assert_eq!(
            found.oids,
            [old_root, old_dir, old_deep].into_iter().collect()
        );

        let unnamed = fx.find(&send, &[], &HashSet::from([edge]), 10);
        assert_eq!(unnamed.oids, [old_root].into_iter().collect());
        assert_eq!(unnamed.targets, [new_root].into_iter().collect());
    }

    #[test]
    fn only_receiver_held_parents_are_edges_and_their_number_is_capped() {
        let fx = Fixture::new();
        let file = |s: &str| fx.tree(&[(0o100644, "f", fx.blob(s))]);
        let held_a = fx.commit(file("a"), &[]);
        let held_b = fx.commit(file("b"), &[]);
        let not_held = fx.commit(file("c"), &[]);
        let new_tree = file("new");
        let tip = fx.commit(new_tree, &[held_a, held_b, not_held]);
        let new_blob = fx.blob("new");
        let send = [tip, new_tree, new_blob];
        let named = [(new_blob, "f")];
        let peer = HashSet::from([held_a, held_b]);

        let all = fx.find(&send, &named, &peer, 10);
        let blobs: HashSet<ObjectId> = all
            .bases
            .iter()
            .filter(|b| b.kind == ObjectKind::Blob)
            .map(|b| b.oid)
            .collect();
        assert_eq!(blobs, HashSet::from([fx.blob("a"), fx.blob("b")]));

        let capped = fx.find(&send, &named, &peer, 1);
        assert_eq!(capped.bases.len(), 2, "one edge: its root tree and blob");
        assert!(fx.find(&send, &named, &peer, 0).bases.is_empty());
        assert!(fx.find(&send, &named, &HashSet::new(), 10).bases.is_empty());
    }

    #[test]
    fn type_changes_and_gitlinks_yield_no_base() {
        let fx = Fixture::new();
        let gitlink = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
        let old_dir = fx.tree(&[(0o100644, "x", fx.blob("x"))]);
        let old_root = fx.tree(&[(0o040000, "p", old_dir), (0o160000, "m", gitlink)]);
        let new_p = fx.blob("p is a file now");
        let new_root = fx.tree(&[(0o100644, "p", new_p), (0o160000, "m", gitlink)]);
        let edge = fx.commit(old_root, &[]);
        let tip = fx.commit(new_root, &[edge]);
        let found = fx.find(
            &[tip, new_root, new_p],
            &[(new_p, "p")],
            &HashSet::from([edge]),
            10,
        );
        assert_eq!(found.oids, [old_root].into_iter().collect());
    }
}
