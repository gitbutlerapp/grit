//! Checking that a push leaves no ref pointing at an incomplete history.
//!
//! Like Git's receive-pack (`rev-list --objects <new tips> --not --all`), every
//! new ref tip is walked down to what the repository's refs already reach, however
//! the push was stored (unpacked into loose objects or installed as a pack, empty
//! or not). An object that was in the repository before the push is not trusted
//! just for being there: a refused ref of an earlier push leaves its objects
//! behind, and naming such a commit again must not pass. Every object the walk
//! reaches must be readable, or be promised by one of the repository's promisor
//! remotes; the parents of the pushing client's shallow boundaries, and of the
//! repository's own, need not exist.
//!
//! The walk has two parts, both bounded by the history the push adds rather than
//! the repository's:
//!
//! - Commits: the new tips are painted against the commits the existing refs
//!   point at, newest first by committer date (commit-graph dates and parents
//!   where the repository has one), until no queued commit is reached only from
//!   the new tips. The commits reached only from them are the new history. A
//!   commit reached from an existing ref is complete by Git's rule; clock skew can
//!   only make the walk take a reachable commit for a new one, which costs time,
//!   not correctness.
//! - Trees: each new commit's tree is compared with its parents' trees path by
//!   path, as a diff would, and only entries that differ from every parent are
//!   followed. An entry equal to a parent's at the same path is reachable from that
//!   parent, which is either reached from an existing ref or new and checked
//!   itself. Missing objects are looked up in one batch at the end: probing them
//!   one at a time would rescan the pack directory on every miss.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::rc::Rc;

use super::promised_objects::PromisorObjects;
use crate::commit_graph_file::CommitGraphChain;
use crate::error::{Error, Result};
use crate::objects::{tag_object_line_oid, CommitRef, Object, ObjectId, ObjectKind, TreeRefIter};
use crate::odb::Odb;

const GITLINK_MODE: u32 = 0o160000;
const TYPE_MASK: u32 = 0o170000;
const TREE_MODE: u32 = 0o040000;

/// Painted on commits the new tips reach.
const NEW: u8 = 1;
/// Painted on commits the existing refs reach.
const OLD: u8 = 2;

/// An object a pushed tip reaches that nothing can supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unconnected {
    /// The missing object.
    pub(crate) oid: ObjectId,
    /// The object that names it.
    pub(crate) referrer: ObjectId,
}

/// The objects a push brought and what the repository already had.
pub(crate) struct PushedObjects<'a> {
    /// Reads the pushed objects (a quarantine view of the push and the repository).
    pub(crate) odb: &'a Odb,
    /// The repository's own objects, where everything not in the push must be.
    pub(crate) repo_odb: &'a Odb,
    /// The repository's commit-graph, for the dates and parents of its commits.
    pub(crate) graph: Option<&'a CommitGraphChain>,
    /// Every object the push stored (in the installed pack, or unpacked loose).
    pub(crate) received: &'a HashSet<ObjectId>,
    /// What the repository's refs and `HEAD` pointed at before the push, tags
    /// peeled too: everything they reach is taken to be complete.
    pub(crate) existing: &'a HashSet<ObjectId>,
    /// Commits at the pushing client's shallow boundary: their parents need not
    /// exist, and are not compared with.
    pub(crate) shallow_boundaries: &'a HashSet<ObjectId>,
    /// The repository's own shallow boundaries (its `shallow` file).
    pub(crate) receiver_grafts: &'a HashSet<ObjectId>,
    /// Promised objects of a partial repository, allowed to be missing.
    pub(crate) promises: Option<&'a PromisorObjects<'a>>,
}

impl PushedObjects<'_> {
    /// For each of `tips`, the first object it reaches that is missing, in order.
    ///
    /// All tips are walked together first; only when something is missing is
    /// each tip walked on its own, to say which ref updates it breaks. A tip that
    /// is itself missing is left to the caller's check of new values.
    ///
    /// # Errors
    ///
    /// Read or parse failures of reachable objects, and promise-set computation
    /// failures.
    pub(crate) fn unconnected_tips(&self, tips: &[ObjectId]) -> Result<Vec<Option<Unconnected>>> {
        if self.walk(tips)?.is_none() {
            return Ok(vec![None; tips.len()]);
        }
        tips.iter()
            .map(|tip| self.walk(std::slice::from_ref(tip)))
            .collect()
    }

    /// Walk the history `tips` add; the first missing object.
    fn walk(&self, tips: &[ObjectId]) -> Result<Option<Unconnected>> {
        let mut walk = Walk::new(self);
        let mut seeds: Vec<(ObjectId, ObjectId)> = Vec::new();
        for &tip in tips {
            walk.peel_tip(tip, &mut seeds)?;
        }
        let commits = walk.new_commits(&seeds)?;
        walk.check_trees(&commits)?;
        self.first_missing(&walk.outside)
    }

    /// Read `oid` without fetching, from the push when it brought it, else from
    /// the repository; `None` when neither has it.
    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        if self.received.contains(oid) {
            self.odb.try_read_once(oid)
        } else {
            self.repo_odb.try_read_once(oid)
        }
    }

    /// The kind of `oid`, without fetching; `None` when it is missing.
    fn kind(&self, oid: &ObjectId) -> Result<Option<ObjectKind>> {
        let odb = if self.received.contains(oid) {
            self.odb
        } else {
            self.repo_odb
        };
        match odb.read_info_without_fetch(oid) {
            Ok(info) => Ok(Some(info.kind)),
            Err(Error::ObjectNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Committer date, parents and tree of the commit `oid`: from the commit-graph
    /// for one the repository had, else from the object. `None` when it is missing
    /// or not a commit.
    fn commit(&self, oid: &ObjectId) -> Result<Option<CommitLinks>> {
        if !self.received.contains(oid) {
            if let Some(graph) = self.graph {
                if let (Some((parents, time)), Some(tree)) =
                    (graph.graph_commit(oid), graph.graph_commit_tree(oid))
                {
                    return Ok(Some(CommitLinks {
                        time,
                        parents,
                        tree,
                    }));
                }
            }
        }
        let Some(obj) = self.read(oid)? else {
            return Ok(None);
        };
        if obj.kind != ObjectKind::Commit {
            return Ok(None);
        }
        let commit = CommitRef::parse(&obj.data)?;
        Ok(Some(CommitLinks {
            time: commit.committer_timestamp(),
            parents: commit.parents().to_vec(),
            tree: commit.tree(),
        }))
    }

    /// Whether the history above `oid` is cut off (a shallow boundary).
    fn is_boundary(&self, oid: &ObjectId) -> bool {
        self.shallow_boundaries.contains(oid) || self.receiver_grafts.contains(oid)
    }

    /// The first of `outside` (objects the walk reached but could not read, or
    /// blobs it did not read, each with the object naming it) that the repository
    /// lacks and no promisor remote promises.
    fn first_missing(&self, outside: &[(ObjectId, ObjectId)]) -> Result<Option<Unconnected>> {
        let absent: HashSet<ObjectId> = self
            .repo_odb
            .missing_stored_oids(outside.iter().map(|(oid, _)| *oid))?
            .into_iter()
            .collect();
        for &(oid, referrer) in outside.iter().filter(|(oid, _)| absent.contains(oid)) {
            // Rare: in an alternate, or promised (an offloaded blob a pushed tree names).
            if self.repo_odb.contains(&oid)? {
                continue;
            }
            if let Some(promises) = self.promises {
                if promises.is_promised(&oid, Some(&referrer))? {
                    continue;
                }
            }
            return Ok(Some(Unconnected { oid, referrer }));
        }
        Ok(None)
    }
}

/// What the tree check needs of a commit.
struct CommitLinks {
    time: i64,
    parents: Vec<ObjectId>,
    tree: ObjectId,
}

/// A new commit: its tree, and the parents whose trees it is compared with.
struct NewCommit {
    oid: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

/// One queued commit of the paint walk, newest first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Queued {
    time: i64,
    oid: ObjectId,
}

/// State of one [`PushedObjects::walk`].
struct Walk<'p, 'a> {
    pushed: &'p PushedObjects<'a>,
    /// Objects the walk named but did not read, with the object naming them.
    outside: Vec<(ObjectId, ObjectId)>,
    /// Tag objects and non-commit tips already followed.
    peeled: HashSet<ObjectId>,
    /// Trees a tip or tag names directly: checked whole.
    loose_trees: Vec<(ObjectId, ObjectId)>,
    /// Root trees of the commits read so far.
    trees: HashMap<ObjectId, ObjectId>,
}

impl<'p, 'a> Walk<'p, 'a> {
    fn new(pushed: &'p PushedObjects<'a>) -> Self {
        Self {
            pushed,
            outside: Vec::new(),
            peeled: HashSet::new(),
            loose_trees: Vec::new(),
            trees: HashMap::new(),
        }
    }

    /// Follow `tip` through tags; commits go to `seeds` (with the object naming
    /// them), trees are checked whole, blobs need only exist.
    fn peel_tip(&mut self, tip: ObjectId, seeds: &mut Vec<(ObjectId, ObjectId)>) -> Result<()> {
        let mut oid = tip;
        let mut referrer = tip;
        loop {
            if self.pushed.existing.contains(&oid) || !self.peeled.insert(oid) {
                return Ok(());
            }
            let Some(kind) = self.pushed.kind(&oid)? else {
                // The tip itself missing is refused elsewhere.
                if oid != tip {
                    self.outside.push((oid, referrer));
                }
                return Ok(());
            };
            match kind {
                ObjectKind::Commit => {
                    seeds.push((oid, referrer));
                    return Ok(());
                }
                ObjectKind::Tree => {
                    self.loose_trees.push((oid, referrer));
                    return Ok(());
                }
                ObjectKind::Blob => return Ok(()),
                ObjectKind::Tag => {
                    let Some(obj) = self.pushed.read(&oid)? else {
                        return Ok(());
                    };
                    let Some(target) = tag_object_line_oid(&obj.data) else {
                        return Ok(());
                    };
                    referrer = oid;
                    oid = target;
                }
            }
        }
    }

    /// The commits `seeds` reach that no existing ref reaches. Commits the push
    /// brought are new; the walk follows them first. The commits they reach that
    /// the repository already had are painted against the existing refs, newest
    /// first, until no queued commit is reached from the new side alone; the
    /// existing refs are read only when that is needed. Missing commits on the new
    /// side go to [`Self::outside`].
    fn new_commits(&mut self, seeds: &[(ObjectId, ObjectId)]) -> Result<Vec<NewCommit>> {
        let mut new = Vec::new();
        let mut is_new: HashSet<ObjectId> = HashSet::new();
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut frontier: Vec<(ObjectId, ObjectId)> = Vec::new();
        let mut stack: Vec<(ObjectId, ObjectId)> = seeds.to_vec();
        while let Some((oid, referrer)) = stack.pop() {
            if self.pushed.existing.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            if !self.pushed.received.contains(&oid) {
                frontier.push((oid, referrer));
                continue;
            }
            let Some(links) = self.commit(&oid)? else {
                self.outside.push((oid, referrer));
                continue;
            };
            let parents = self.parents_to_follow(&oid, links.parents);
            stack.extend(parents.iter().map(|parent| (*parent, oid)));
            is_new.insert(oid);
            new.push(NewCommit {
                oid,
                tree: links.tree,
                parents,
            });
        }
        if frontier.is_empty() {
            return Ok(new);
        }

        let mut paint = Paint::default();
        for (oid, referrer) in frontier {
            self.paint(&mut paint, oid, NEW, referrer)?;
        }
        if paint.new_only > 0 {
            for &oid in self.pushed.existing {
                self.paint(&mut paint, oid, OLD, oid)?;
            }
        }
        // Commits popped from the new side alone; an existing ref may still reach
        // one through a commit of the same date or newer, so the walk goes on until
        // none is queued.
        let mut candidates: Vec<NewCommit> = Vec::new();
        let mut oldest_new = i64::MAX;
        while paint.new_only > 0 || paint.heap.peek().is_some_and(|q| q.time >= oldest_new) {
            let Some(Queued { oid, time }) = paint.heap.pop() else {
                break;
            };
            let Some(links) = paint.queued.remove(&oid) else {
                continue;
            };
            let flags = paint.flags.get(&oid).copied().unwrap_or(0);
            let parents = self.parents_to_follow(&oid, links.parents);
            paint.popped.insert(oid, parents.clone());
            if flags & OLD != 0 {
                for parent in &parents {
                    self.paint(&mut paint, *parent, OLD, oid)?;
                }
                continue;
            }
            paint.new_only -= 1;
            oldest_new = oldest_new.min(time);
            for parent in &parents {
                self.paint(&mut paint, *parent, NEW, oid)?;
            }
            candidates.push(NewCommit {
                oid,
                tree: links.tree,
                parents,
            });
        }
        for commit in candidates {
            let reached = paint.flags.get(&commit.oid).is_some_and(|f| f & OLD != 0);
            if !reached && is_new.insert(commit.oid) {
                new.push(commit);
            }
        }
        Ok(new)
    }

    /// The parents of `oid` the walk follows: none at a shallow boundary.
    fn parents_to_follow(&self, oid: &ObjectId, parents: Vec<ObjectId>) -> Vec<ObjectId> {
        if self.pushed.is_boundary(oid) {
            Vec::new()
        } else {
            parents
        }
    }

    /// [`PushedObjects::commit`], remembering each commit's tree for the tree check.
    fn commit(&mut self, oid: &ObjectId) -> Result<Option<CommitLinks>> {
        let links = self.pushed.commit(oid)?;
        if let Some(links) = &links {
            self.trees.insert(*oid, links.tree);
        }
        Ok(links)
    }

    /// Add `flag` to commit `oid` (named by `referrer`), queueing it when first
    /// reached. A commit an existing ref names is painted [`OLD`] at once, and
    /// [`OLD`] reaching a commit already popped goes on to its parents. One the new
    /// side reaches but cannot read is recorded as missing.
    fn paint(
        &mut self,
        paint: &mut Paint,
        oid: ObjectId,
        flag: u8,
        referrer: ObjectId,
    ) -> Result<()> {
        let mut work = vec![(oid, flag, referrer)];
        while let Some((oid, flag, referrer)) = work.pop() {
            if paint.missing.contains(&oid) {
                if flag == NEW {
                    self.outside.push((oid, referrer));
                }
                continue;
            }
            let flag = if self.pushed.existing.contains(&oid) {
                flag | OLD
            } else {
                flag
            };
            let before = paint.flags.get(&oid).copied().unwrap_or(0);
            let after = before | flag;
            if after == before {
                continue;
            }
            if before == 0 {
                let Some(links) = self.commit(&oid)? else {
                    paint.missing.insert(oid);
                    if flag == NEW {
                        self.outside.push((oid, referrer));
                    }
                    continue;
                };
                paint.heap.push(Queued {
                    time: links.time,
                    oid,
                });
                paint.queued.insert(oid, links);
                if after == NEW {
                    paint.new_only += 1;
                }
            } else if before == NEW && paint.queued.contains_key(&oid) {
                // Still queued, and an existing ref reaches it after all.
                paint.new_only -= 1;
            } else if after & OLD != 0 && before & OLD == 0 {
                if let Some(parents) = paint.popped.get(&oid) {
                    work.extend(parents.iter().map(|parent| (*parent, OLD, oid)));
                }
            }
            paint.flags.insert(oid, after);
        }
        Ok(())
    }

    /// Check the trees of `commits` against their parents' trees, and the trees
    /// tips name directly in full. Blobs and trees that cannot be read go to
    /// [`Self::outside`].
    fn check_trees(&mut self, commits: &[NewCommit]) -> Result<()> {
        let oid_len = self.pushed.odb.hash_algo().len();
        let mut done: HashSet<ObjectId> = HashSet::new();
        let mut cache = TreeCache::default();
        let mut stack: Vec<(ObjectId, Vec<ObjectId>, ObjectId)> = Vec::new();
        for (tree, referrer) in std::mem::take(&mut self.loose_trees) {
            stack.push((tree, Vec::new(), referrer));
        }
        for commit in commits {
            let mut bases = Vec::with_capacity(commit.parents.len());
            for parent in &commit.parents {
                if let Some(tree) = self.trees.get(parent) {
                    bases.push(*tree);
                } else if let Some(links) = self.commit(parent)? {
                    bases.push(links.tree);
                }
            }
            stack.push((commit.tree, bases, commit.oid));
        }
        while let Some((tree, bases, referrer)) = stack.pop() {
            if bases.contains(&tree) || !done.insert(tree) {
                continue;
            }
            let Some(obj) = cache.read(self.pushed, &tree)? else {
                self.outside.push((tree, referrer));
                continue;
            };
            // A base that cannot be read compares as empty.
            let mut base_objects: Vec<Rc<Object>> = Vec::with_capacity(bases.len());
            for base in &bases {
                base_objects.extend(cache.read(self.pushed, base)?);
            }
            let mut cursors: Vec<_> = base_objects
                .iter()
                .map(|base| TreeRefIter::with_oid_len(&base.data, oid_len).peekable())
                .collect();
            for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                let entry = entry?;
                if entry.mode == GITLINK_MODE {
                    continue;
                }
                let is_tree = entry.mode & TYPE_MASK == TREE_MODE;
                // Trees are sorted, so each parent's entry at this path is found by
                // moving its cursor forward. An unsorted or corrupt parent tree only
                // makes entries look changed, which checks more, never less.
                let mut unchanged = false;
                let mut sub_bases = Vec::new();
                for cursor in &mut cursors {
                    while let Some(Ok(base)) = cursor.peek() {
                        let base_is_tree = base.mode & TYPE_MASK == TREE_MODE;
                        match tree_order(base.name, base_is_tree, entry.name, is_tree) {
                            std::cmp::Ordering::Less => {
                                cursor.next();
                            }
                            std::cmp::Ordering::Equal => {
                                if base.oid == entry.oid {
                                    unchanged = true;
                                } else if is_tree {
                                    sub_bases.push(base.oid);
                                }
                                break;
                            }
                            std::cmp::Ordering::Greater => break,
                        }
                    }
                }
                if unchanged {
                    continue;
                }
                if is_tree {
                    stack.push((entry.oid, sub_bases, tree));
                } else if !self.pushed.received.contains(&entry.oid) {
                    self.outside.push((entry.oid, tree));
                }
            }
        }
        Ok(())
    }
}

/// Git's tree entry order: names compared bytewise, a tree's as if it ended in `/`.
fn tree_order(a: &[u8], a_is_tree: bool, b: &[u8], b_is_tree: bool) -> std::cmp::Ordering {
    let slash = |is_tree: bool| is_tree.then_some(b'/');
    a.iter()
        .copied()
        .chain(slash(a_is_tree))
        .cmp(b.iter().copied().chain(slash(b_is_tree)))
}

/// Trees the tree check read recently: a commit's trees are its child's bases,
/// read right after. Bounded; when full it starts over.
#[derive(Default)]
struct TreeCache {
    trees: HashMap<ObjectId, Rc<Object>>,
    bytes: usize,
}

impl TreeCache {
    const MAX_BYTES: usize = 64 << 20;

    fn read(&mut self, pushed: &PushedObjects<'_>, oid: &ObjectId) -> Result<Option<Rc<Object>>> {
        if let Some(obj) = self.trees.get(oid) {
            return Ok(Some(Rc::clone(obj)));
        }
        let Some(obj) = pushed.read(oid)? else {
            return Ok(None);
        };
        let obj = Rc::new(obj);
        if self.bytes + obj.data.len() > Self::MAX_BYTES {
            self.trees.clear();
            self.bytes = 0;
        }
        self.bytes += obj.data.len();
        self.trees.insert(*oid, Rc::clone(&obj));
        Ok(Some(obj))
    }
}

/// The paint walk's queue and colors.
#[derive(Default)]
struct Paint {
    heap: BinaryHeap<Queued>,
    /// Colors of every commit reached.
    flags: HashMap<ObjectId, u8>,
    /// Links of the commits still queued, taken when popped.
    queued: HashMap<ObjectId, CommitLinks>,
    /// Parents followed from each popped commit.
    popped: HashMap<ObjectId, Vec<ObjectId>>,
    /// Commits that could not be read (or are not commits).
    missing: HashSet<ObjectId>,
    /// Queued commits painted [`NEW`] only.
    new_only: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tree(odb: &Odb, entries: &[(&str, &str, ObjectId)]) -> ObjectId {
        let mut body = Vec::new();
        for (mode, name, oid) in entries {
            body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        odb.write(ObjectKind::Tree, &body).unwrap()
    }

    fn commit(odb: &Odb, tree: ObjectId, parents: &[ObjectId]) -> ObjectId {
        commit_with(odb, tree, parents, "c")
    }

    fn commit_with(odb: &Odb, tree: ObjectId, parents: &[ObjectId], message: &str) -> ObjectId {
        let mut body = format!("tree {tree}\n");
        for p in parents {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str("author A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\n");
        body.push_str(message);
        odb.write(ObjectKind::Commit, body.as_bytes()).unwrap()
    }

    fn absent(n: u8) -> ObjectId {
        ObjectId::from_bytes(&[n; 20]).unwrap()
    }

    /// Check `tips` against a repository whose refs pointed at `existing`, with
    /// the push having brought `received`.
    fn check(
        odb: &Odb,
        received: &[ObjectId],
        existing: &[ObjectId],
        shallow: &[ObjectId],
        tips: &[ObjectId],
    ) -> Vec<Option<Unconnected>> {
        let received: HashSet<ObjectId> = received.iter().copied().collect();
        let existing: HashSet<ObjectId> = existing.iter().copied().collect();
        let shallow: HashSet<ObjectId> = shallow.iter().copied().collect();
        let none = HashSet::new();
        PushedObjects {
            odb,
            repo_odb: odb,
            graph: None,
            received: &received,
            existing: &existing,
            shallow_boundaries: &shallow,
            receiver_grafts: &none,
            promises: None,
        }
        .unconnected_tips(tips)
        .unwrap()
    }

    #[test]
    fn complete_pushes_pass_and_gaps_are_named_per_tip() {
        let dir = tempfile::tempdir().unwrap();
        let odb = Odb::new(dir.path());
        let old_blob = odb.write(ObjectKind::Blob, b"old\n").unwrap();
        let old = commit(&odb, tree(&odb, &[("100644", "o", old_blob)]), &[]);
        let blob = odb.write(ObjectKind::Blob, b"new\n").unwrap();
        let sub = tree(&odb, &[("100644", "f", blob), ("160000", "m", absent(7))]);
        let root = tree(&odb, &[("40000", "d", sub), ("100644", "o", old_blob)]);
        let good = commit(&odb, root, &[old]);
        let broken_tree = tree(&odb, &[("100644", "gone", absent(9))]);
        let broken = commit(&odb, broken_tree, &[good]);
        let received = [blob, sub, root, good, broken_tree, broken];
        assert_eq!(check(&odb, &received, &[old], &[], &[good]), vec![None]);
        assert_eq!(
            check(&odb, &received, &[old], &[], &[good, broken, old]),
            vec![
                None,
                Some(Unconnected {
                    oid: absent(9),
                    referrer: broken_tree
                }),
                None
            ]
        );
    }

    #[test]
    fn objects_the_repository_had_are_walked_unless_a_ref_reaches_them() {
        let dir = tempfile::tempdir().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"base\n").unwrap();
        let base = commit(&odb, tree(&odb, &[("100644", "b", blob)]), &[]);
        // Left behind by a refused push: in the repository, but no ref reaches it.
        let broken_tree = tree(
            &odb,
            &[("100644", "b", blob), ("100644", "gone", absent(9))],
        );
        let left = commit(&odb, broken_tree, &[base]);
        let gap = Some(Unconnected {
            oid: absent(9),
            referrer: broken_tree,
        });
        // Named again with nothing new (an empty pack), or as the parent of a
        // pushed commit.
        assert_eq!(check(&odb, &[], &[base], &[], &[left]), vec![gap]);
        let fixed = commit(&odb, tree(&odb, &[("100644", "b", blob)]), &[left]);
        assert_eq!(check(&odb, &[fixed], &[base], &[], &[fixed]), vec![gap]);
        // A ref reaching it would vouch for it, as Git's `--not --all` does.
        assert_eq!(check(&odb, &[fixed], &[left], &[], &[fixed]), vec![None]);
        // An annotated tag on it is followed to the same gap.
        let body = format!("object {left}\ntype commit\ntag t\ntagger A <a@e> 1 +0000\n\nt\n");
        let tag = odb.write(ObjectKind::Tag, body.as_bytes()).unwrap();
        assert_eq!(check(&odb, &[tag], &[base], &[], &[tag]), vec![gap]);
    }

    #[test]
    fn the_walk_stops_where_the_existing_refs_reach() {
        let dir = tempfile::tempdir().unwrap();
        let odb = Odb::new(dir.path());
        // The repository's own history is trusted, gaps and all (Git does the same):
        // `main` names a tree with a missing blob.
        let trusted_tree = tree(&odb, &[("100644", "gone", absent(4))]);
        let root = commit(&odb, trusted_tree, &[]);
        // A pushed branch forks from `root`, below `main`: the paint walk finds `root`
        // reached from `main`, and the unchanged entry is not looked at again. Every
        // commit has the same date, so whichever of `root` and `main` the queue
        // takes first, `main` must still be painted down to `root`.
        let blob = odb.write(ObjectKind::Blob, b"new\n").unwrap();
        let t = tree(
            &odb,
            &[("100644", "gone", absent(4)), ("100644", "n", blob)],
        );
        let topic = commit(&odb, t, &[root]);
        let mains: Vec<ObjectId> = (0..8)
            .map(|n| commit_with(&odb, trusted_tree, &[root], &format!("main {n}")))
            .collect();
        assert!(mains.iter().any(|m| *m < root) && mains.iter().any(|m| *m > root));
        for main in mains {
            assert_eq!(
                check(&odb, &[blob, t, topic], &[main], &[], &[topic]),
                vec![None]
            );
        }
        // Without the ref, `root` is new history and its tree is checked.
        assert_eq!(
            check(&odb, &[blob, t, topic], &[], &[], &[topic]),
            vec![Some(Unconnected {
                oid: absent(4),
                referrer: trusted_tree
            })]
        );
    }

    #[test]
    fn shallow_boundary_parents_may_be_missing() {
        let dir = tempfile::tempdir().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"x\n").unwrap();
        let root = tree(&odb, &[("100644", "x", blob)]);
        let tip = commit(&odb, root, &[absent(3)]);
        let received = [blob, root, tip];
        assert_eq!(
            check(&odb, &received, &[], &[], &[tip]),
            vec![Some(Unconnected {
                oid: absent(3),
                referrer: tip
            })]
        );
        assert_eq!(check(&odb, &received, &[], &[tip], &[tip]), vec![None]);
    }
}
