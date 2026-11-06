//! Skipping fetch negotiator — implements Git's "skipping" negotiation strategy.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use crate::commit_graph_file::CommitGraphChain;
use crate::error::{Error, Result};
use crate::objects::{parse_commit, ObjectId};
use crate::repo::Repository;

const COMMON: u8 = 1 << 2;
const ADVERTISED: u8 = 1 << 3;
const SEEN: u8 = 1 << 4;
const POPPED: u8 = 1 << 5;

fn committer_unix_seconds(committer: &str) -> Result<i64> {
    let parts: Vec<&str> = committer.rsplitn(3, ' ').collect();
    if parts.len() < 2 {
        return Err(Error::CorruptObject(
            "committer line missing date".to_owned(),
        ));
    }
    parts[1]
        .parse::<i64>()
        .map_err(|_| Error::CorruptObject("invalid committer timestamp".to_owned()))
}

fn commit_date(repo: &Repository, graph: Option<&CommitGraphChain>, oid: ObjectId) -> Result<i64> {
    if let Some(chain) = graph {
        if let Some((_, commit_time)) = chain.graph_commit(&oid) {
            return Ok(commit_time);
        }
    }
    let obj = repo.odb().read(&oid)?;
    let c = parse_commit(&obj.data)?;
    committer_unix_seconds(&c.committer)
}

fn read_parents(
    repo: &Repository,
    graph: Option<&CommitGraphChain>,
    oid: ObjectId,
) -> Result<Vec<ObjectId>> {
    if let Some(chain) = graph {
        if let Some((parents, _)) = chain.graph_commit(&oid) {
            return Ok(parents);
        }
    }
    let obj = repo.odb().read(&oid)?;
    Ok(parse_commit(&obj.data)?.parents)
}

/// Read the shallow boundary commits recorded in `$GIT_DIR/shallow`.
///
/// These commits have their real parents grafted away locally — the objects beyond the boundary
/// are simply not present. The negotiator must treat them as parentless, matching how Git's
/// `register_shallow()` rewrites the commit graph during `rev-list` so negotiation never tries to
/// load (and fails on) objects past the shallow cut (t5539 shallow http fetch/deepen).
fn read_shallow_boundary(repo: &Repository) -> HashSet<ObjectId> {
    crate::shallow::load_shallow_boundaries(repo.git_dir())
}

#[derive(Clone, Copy)]
struct HeapItem {
    date: i64,
    oid: ObjectId,
}

impl Eq for HeapItem {}
impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| self.oid.cmp(&other.oid))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Entry {
    original_ttl: u16,
    ttl: u16,
}

/// Skipping algorithm negotiator for fetch `have` lines.
pub struct SkippingNegotiator<'r> {
    repo: &'r Repository,
    commit_graph: Option<Arc<CommitGraphChain>>,
    heap: BinaryHeap<HeapItem>,
    entries: HashMap<ObjectId, Entry>,
    flags: HashMap<ObjectId, u8>,
    non_common_revs: usize,
    shallow: HashSet<ObjectId>,
}

impl<'r> SkippingNegotiator<'r> {
    /// New negotiator reading commits through `repo`'s object store.
    pub fn new(repo: &'r Repository) -> Self {
        let shallow = read_shallow_boundary(repo);
        let commit_graph = repo.odb().commit_graph_chain();
        Self {
            repo,
            commit_graph,
            heap: BinaryHeap::new(),
            entries: HashMap::new(),
            flags: HashMap::new(),
            non_common_revs: 0,
            shallow,
        }
    }

    /// Repository handle used for reading commits.
    pub fn repo(&self) -> &'r Repository {
        self.repo
    }

    /// Parents of `oid`, with shallow-boundary commits reported as parentless.
    ///
    /// A commit listed in `$GIT_DIR/shallow` has had its parents grafted away locally, so the
    /// objects beyond it are absent. Returning no parents prevents the negotiation walk from
    /// dereferencing those missing objects and erroring with "object not found".
    fn parents_of(&self, oid: ObjectId) -> Result<Vec<ObjectId>> {
        if self.shallow.contains(&oid) {
            return Ok(Vec::new());
        }
        read_parents(self.repo, self.commit_graph.as_deref(), oid)
    }

    fn f(&self, oid: ObjectId) -> u8 {
        *self.flags.get(&oid).unwrap_or(&0)
    }

    fn set_f(&mut self, oid: ObjectId, bits: u8) {
        *self.flags.entry(oid).or_insert(0) |= bits;
    }

    fn rev_list_push(&mut self, oid: ObjectId, mark: u8) -> Result<()> {
        self.set_f(oid, mark | SEEN);
        self.entries.insert(
            oid,
            Entry {
                original_ttl: 0,
                ttl: 0,
            },
        );
        if mark & COMMON == 0 {
            self.non_common_revs += 1;
        }
        let d = commit_date(self.repo, self.commit_graph.as_deref(), oid)?;
        self.heap.push(HeapItem { date: d, oid });
        Ok(())
    }

    /// Server-advertised commit (ref tip). Annotated-tag tips are peeled to the
    /// commit they reference; a tip that does not resolve to a commit is ignored.
    pub fn known_common(&mut self, oid: ObjectId) -> Result<()> {
        let Some(oid) = self.repo.odb().peel_to_commit(oid)? else {
            return Ok(());
        };
        if self.f(oid) & SEEN != 0 {
            return Ok(());
        }
        self.rev_list_push(oid, ADVERTISED)?;
        Ok(())
    }

    /// Local negotiation tip. Annotated-tag tips are peeled to the commit they
    /// reference; a tip that does not resolve to a commit is ignored.
    pub fn add_tip(&mut self, oid: ObjectId) -> Result<()> {
        let Some(oid) = self.repo.odb().peel_to_commit(oid)? else {
            return Ok(());
        };
        if self.f(oid) & SEEN != 0 {
            return Ok(());
        }
        self.rev_list_push(oid, 0)?;
        Ok(())
    }

    fn mark_common(&mut self, seen_oid: ObjectId) -> Result<()> {
        if self.f(seen_oid) & COMMON != 0 {
            return Ok(());
        }

        let mut stack = vec![seen_oid];
        self.set_f(seen_oid, COMMON);

        while let Some(c) = stack.pop() {
            if self.f(c) & POPPED == 0 {
                self.non_common_revs = self.non_common_revs.saturating_sub(1);
            }

            for p in self.parents_of(c)? {
                if self.f(p) & SEEN == 0 || self.f(p) & COMMON != 0 {
                    continue;
                }
                self.set_f(p, COMMON);
                stack.push(p);
            }
        }
        Ok(())
    }

    fn push_parent(&mut self, entry_oid: ObjectId, to_push: ObjectId) -> Result<bool> {
        let (entry_ttl, entry_orig) = {
            let e = self
                .entries
                .get(&entry_oid)
                .ok_or_else(|| Error::CorruptObject("missing queue entry".to_owned()))?;
            (e.ttl, e.original_ttl)
        };

        let entry_common_or_adv = self.f(entry_oid) & (COMMON | ADVERTISED) != 0;

        if self.f(to_push) & SEEN != 0 {
            if self.f(to_push) & POPPED != 0 {
                return Ok(false);
            }
        } else {
            self.rev_list_push(to_push, 0)?;
        }

        let parent_entry = self
            .entries
            .get_mut(&to_push)
            .ok_or_else(|| Error::CorruptObject("missing parent entry".to_owned()))?;

        if entry_common_or_adv {
            self.mark_common(to_push)?;
        } else {
            let new_original_ttl = if entry_ttl != 0 {
                entry_orig
            } else {
                entry_orig * 3 / 2 + 1
            };
            let new_ttl = if entry_ttl != 0 {
                entry_ttl - 1
            } else {
                new_original_ttl
            };
            if parent_entry.original_ttl < new_original_ttl {
                parent_entry.original_ttl = new_original_ttl;
                parent_entry.ttl = new_ttl;
            }
        }

        Ok(true)
    }

    /// Next OID to advertise as `have`, or `None` when finished.
    pub fn next_have(&mut self) -> Result<Option<ObjectId>> {
        loop {
            if self.heap.is_empty() || self.non_common_revs == 0 {
                return Ok(None);
            }

            let HeapItem {
                oid: commit_oid, ..
            } = self
                .heap
                .pop()
                .ok_or_else(|| Error::CorruptObject("negotiator heap inconsistency".to_owned()))?;

            let entry_ttl = self
                .entries
                .get(&commit_oid)
                .map(|e| e.ttl)
                .ok_or_else(|| Error::CorruptObject("popped missing entry".to_owned()))?;

            self.set_f(commit_oid, POPPED);
            if self.f(commit_oid) & COMMON == 0 {
                self.non_common_revs = self.non_common_revs.saturating_sub(1);
            }

            let mut to_send: Option<ObjectId> = None;
            if self.f(commit_oid) & COMMON == 0 && entry_ttl == 0 {
                to_send = Some(commit_oid);
            }

            let pars = self.parents_of(commit_oid)?;
            let mut parent_pushed = false;
            for p in pars {
                parent_pushed |= self.push_parent(commit_oid, p)?;
            }

            if self.f(commit_oid) & COMMON == 0 && !parent_pushed {
                to_send = Some(commit_oid);
            }

            self.entries.remove(&commit_oid);

            if let Some(oid) = to_send {
                return Ok(Some(oid));
            }
        }
    }

    /// Record server `ACK` for a commit previously returned by [`Self::next_have`].
    ///
    /// Returns `true` if that commit was already `COMMON` before this call.
    pub fn ack(&mut self, oid: ObjectId) -> Result<bool> {
        if self.f(oid) & SEEN == 0 {
            return Err(Error::CorruptObject(format!(
                "ack for commit {} not sent as have",
                oid.to_hex()
            )));
        }
        let known = self.f(oid) & COMMON != 0;
        self.mark_common(oid)?;
        Ok(known)
    }
}

#[cfg(test)]
mod tests {
    use super::SkippingNegotiator;
    use crate::config::environment::RepositoryOptions;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::repo::{InitOptions, Repository};

    fn write(repo: &Repository, kind: ObjectKind, data: &[u8]) -> ObjectId {
        repo.odb().write(kind, data).expect("write object")
    }

    #[test]
    fn add_tip_accepts_annotated_tag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
            tmp.path(),
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");

        let tree = write(&repo, ObjectKind::Tree, &[]);
        let commit_bytes = format!(
            "tree {tree}\n\
             author A U Thor <a@example.com> 0 +0000\n\
             committer A U Thor <a@example.com> 0 +0000\n\
             \n\
             a commit\n"
        );
        let commit = write(&repo, ObjectKind::Commit, commit_bytes.as_bytes());
        let tag_bytes = format!(
            "object {commit}\n\
             type commit\n\
             tag v1\n\
             tagger A U Thor <a@example.com> 0 +0000\n\
             \n\
             annotated v1\n"
        );
        let tag = write(&repo, ObjectKind::Tag, tag_bytes.as_bytes());

        let repo = Repository::open(repo.git_dir(), &RepositoryOptions::empty()).expect("open");
        let mut neg = SkippingNegotiator::new(&repo);

        neg.add_tip(tag)
            .expect("add_tip must accept an annotated tag");
        neg.known_common(tag)
            .expect("known_common must accept an annotated tag");

        let have = neg.next_have().expect("next_have").expect("a have");
        assert_eq!(have, commit, "tag tip should peel to its target commit");

        let reopened = Repository::open(tmp.path(), &RepositoryOptions::empty()).expect("open");
        let mut neg = SkippingNegotiator::new(&reopened);
        neg.add_tip(commit).expect("commit tip ok");
        neg.add_tip(tree)
            .expect("tree tip must be skipped, not error");
    }
}
