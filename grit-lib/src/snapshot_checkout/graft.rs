//! Grafting: turning commits made on a checkout's snapshots into commits on
//! the server's real history.
//!
//! A checkout's commits sit on snapshot commits whose trees hold only the
//! checkout's paths. To check them in, the server replays each one onto the
//! real history: the commit's changes against its parent (in snapshot space)
//! are applied path by path to the full tree of the parent's graft (a
//! snapshot's graft is the real commit it stands for). The new commit keeps
//! the author, committer, message and extra headers; only its tree and
//! parents change. Replays are deterministic, so checking in the same commit
//! twice gives the same commit.
//!
//! A change applies only where the full tree holds what the change started
//! from. When it doesn't (a file outside the checkout's view that the target
//! holds differently), the graft is refused for that commit.

use std::collections::{HashMap, HashSet};

use crate::merge_base::{walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit};
use crate::objects::{parse_commit, serialize_commit, ObjectId, ObjectKind};
use crate::repo::Repository;

use super::tree::{diff, edit, lookup};
use super::{CheckoutError, Result, SnapshotRoot};

/// Why a branch could not be grafted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraftError {
    /// A commit's parent is neither a snapshot of the checkout nor another of
    /// the branch's commits.
    #[error("commit {commit} does not build on this checkout's snapshots")]
    NotOnSnapshot {
        /// The commit.
        commit: String,
    },
    /// A commit changes a path the target holds differently from what the
    /// commit started from.
    #[error("commit {commit} changes {path}, which the target branch holds differently; refresh (grit pull) and try again")]
    Conflict {
        /// The commit.
        commit: String,
        /// The path.
        path: String,
    },
}

/// A branch replayed onto the real history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grafted {
    /// The graft of the branch's tip.
    pub tip: ObjectId,
    /// Each commit replayed, oldest first: the checkout's commit and its graft.
    pub commits: Vec<(ObjectId, ObjectId)>,
}

/// Replay `tip` and the commits under it, down to the checkout's snapshot
/// commits (`roots`), onto the real history.
///
/// # Returns
///
/// `Ok(Err(..))` when the branch cannot be grafted (see [`GraftError`]).
///
/// # Errors
///
/// Object read and write failures.
pub fn graft(
    repo: &Repository,
    roots: &[SnapshotRoot],
    tip: ObjectId,
) -> Result<std::result::Result<Grafted, GraftError>> {
    let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut hide = Vec::new();
    for root in roots {
        let (Ok(commit), Ok(base)) = (
            ObjectId::from_hex(&root.commit),
            ObjectId::from_hex(&root.base),
        ) else {
            continue;
        };
        map.insert(commit, base);
        hide.push(commit);
    }
    if let Some(base) = map.get(&tip) {
        return Ok(Ok(Grafted {
            tip: *base,
            commits: Vec::new(),
        }));
    }
    let (_, newest_first) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        &[tip],
        &hide,
        ReachableWalkLimit::Unlimited,
    )?;
    let odb = repo.odb();
    let order = parents_first(repo, &newest_first)?;
    let mut made = Vec::new();
    for commit in order {
        let short = commit.to_hex()[..12].to_owned();
        let mut data = parse_commit(&odb.read(&commit)?.data)?;
        let Some(first) = data.parents.first().copied() else {
            return Ok(Err(GraftError::NotOnSnapshot { commit: short }));
        };
        let mut mapped = Vec::with_capacity(data.parents.len());
        for parent in &data.parents {
            match map.get(parent) {
                Some(g) => mapped.push(*g),
                None => return Ok(Err(GraftError::NotOnSnapshot { commit: short })),
            }
        }
        let parent_tree = parse_commit(&odb.read(&first)?.data)?.tree;
        let onto = parse_commit(&odb.read(&mapped[0])?.data)?.tree;
        let mut edits = Vec::new();
        for change in diff(odb, Some(parent_tree), Some(data.tree))? {
            if lookup(odb, onto, &change.path)? != change.old {
                return Ok(Err(GraftError::Conflict {
                    commit: short,
                    path: String::from_utf8_lossy(&change.path).into_owned(),
                }));
            }
            edits.push((change.path, change.new));
        }
        let tree = if edits.is_empty() {
            onto
        } else {
            edit(odb, Some(onto), &edits)?
        };
        data.tree = tree;
        data.parents = mapped;
        let grafted = odb.write(ObjectKind::Commit, &serialize_commit(&data))?;
        map.insert(commit, grafted);
        made.push((commit, grafted));
    }
    match map.get(&tip) {
        Some(g) => Ok(Ok(Grafted {
            tip: *g,
            commits: made,
        })),
        None => Ok(Err(GraftError::NotOnSnapshot {
            commit: tip.to_hex()[..12].to_owned(),
        })),
    }
}

/// `commits` ordered so every commit comes after its parents among them.
fn parents_first(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let set: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for c in commits {
        let data = parse_commit(&repo.odb().read(c)?.data)?;
        parents.insert(
            *c,
            data.parents
                .into_iter()
                .filter(|p| set.contains(p))
                .collect(),
        );
    }
    let mut out = Vec::with_capacity(commits.len());
    let mut done: HashSet<ObjectId> = HashSet::new();
    // Oldest first as walked, so ties keep history order.
    for start in commits.iter().rev() {
        let mut stack = vec![(*start, false)];
        while let Some((c, expanded)) = stack.pop() {
            if done.contains(&c) {
                continue;
            }
            if expanded {
                done.insert(c);
                out.push(c);
                continue;
            }
            stack.push((c, true));
            for p in parents.get(&c).into_iter().flatten() {
                if !done.contains(p) {
                    stack.push((*p, false));
                }
            }
        }
    }
    Ok(out)
}

/// How the server decides whether a check-in may set a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckinDecision {
    /// The branch does not exist yet.
    Create,
    /// The branch already points at the graft.
    Unchanged,
    /// The branch moves to the graft.
    Update,
    /// The server's branch has commits the graft would drop.
    Diverged,
}

/// Decide whether `grafted` may replace the server's branch, now at
/// `previous`.
///
/// An update is allowed when it is a fast-forward, when `force` is set, or
/// when this checkout set the branch last (`set_last`), so a checkout can
/// replace its own work after an amend or rebase.
///
/// # Errors
///
/// Ancestry walk failures.
pub fn decide_checkin(
    repo: &Repository,
    previous: Option<ObjectId>,
    grafted: ObjectId,
    set_last: Option<ObjectId>,
    force: bool,
) -> Result<CheckinDecision> {
    let Some(previous) = previous else {
        return Ok(CheckinDecision::Create);
    };
    if previous == grafted {
        return Ok(CheckinDecision::Unchanged);
    }
    if force || set_last == Some(previous) {
        return Ok(CheckinDecision::Update);
    }
    if crate::merge_base::is_ancestor(repo, previous, grafted).map_err(CheckoutError::from)? {
        return Ok(CheckinDecision::Update);
    }
    Ok(CheckinDecision::Diverged)
}

#[cfg(test)]
mod tests {
    use super::super::{make_snapshot, CheckoutError};
    use super::*;
    use crate::objects::{parse_commit, ObjectKind};
    use crate::repo::{InitOptions, Repository};
    use crate::snapshot_checkout::tree::{edit, lookup};

    fn commit(repo: &Repository, tree: ObjectId, parents: &[ObjectId], msg: &str) -> ObjectId {
        let mut s = format!("tree {}\n", tree.to_hex());
        for p in parents {
            s.push_str(&format!("parent {}\n", p.to_hex()));
        }
        s.push_str(&format!(
            "author A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n\n{msg}\n"
        ));
        repo.odb()
            .write(ObjectKind::Commit, s.as_bytes())
            .expect("commit")
    }

    fn blob(repo: &Repository, text: &str) -> (u32, ObjectId) {
        (
            0o100644,
            repo.odb()
                .write(ObjectKind::Blob, text.as_bytes())
                .expect("blob"),
        )
    }

    #[test]
    fn commits_on_a_snapshot_graft_onto_the_full_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();
        let full = edit(
            odb,
            None,
            &[
                (b"README".to_vec(), Some(blob(&repo, "top"))),
                (b"src/a".to_vec(), Some(blob(&repo, "a"))),
                (b"docs/d".to_vec(), Some(blob(&repo, "d"))),
            ],
        )
        .expect("tree");
        let base = commit(&repo, full, &[], "base");
        let snap = make_snapshot(&repo, base, &["src".to_owned()], None).expect("snapshot");
        assert_eq!(lookup(odb, snap.tree, b"docs/d").expect("lookup"), None);
        let roots = vec![SnapshotRoot {
            commit: snap.commit.to_hex(),
            base: base.to_hex(),
        }];

        // Two commits on the snapshot: change src/a, then add src/b.
        let t1 = edit(
            odb,
            Some(snap.tree),
            &[(b"src/a".to_vec(), Some(blob(&repo, "a2")))],
        )
        .expect("t1");
        let c1 = commit(&repo, t1, &[snap.commit], "change a");
        let t2 = edit(
            odb,
            Some(t1),
            &[(b"src/b".to_vec(), Some(blob(&repo, "b")))],
        )
        .expect("t2");
        let c2 = commit(&repo, t2, &[c1], "add b");

        let grafted = graft(&repo, &roots, c2).expect("graft").expect("grafted");
        assert_eq!(grafted.commits.len(), 2);
        let tip = parse_commit(&odb.read(&grafted.tip).expect("read").data).expect("parse");
        assert_eq!(tip.message, "add b\n");
        assert_eq!(
            lookup(odb, tip.tree, b"docs/d").expect("lookup"),
            Some(blob(&repo, "d")),
            "outside paths kept"
        );
        assert_eq!(
            lookup(odb, tip.tree, b"src/a").expect("lookup"),
            Some(blob(&repo, "a2"))
        );
        assert_eq!(
            lookup(odb, tip.tree, b"src/b").expect("lookup"),
            Some(blob(&repo, "b"))
        );
        let first = parse_commit(&odb.read(&tip.parents[0]).expect("read").data).expect("parse");
        assert_eq!(first.parents, vec![base]);

        // Deterministic: grafting again gives the same commits.
        assert_eq!(
            graft(&repo, &roots, c2).expect("graft").expect("grafted"),
            grafted
        );
        // The snapshot itself grafts to its base.
        assert_eq!(
            graft(&repo, &roots, snap.commit)
                .expect("graft")
                .expect("grafted")
                .tip,
            base
        );

        // A commit not on the snapshot is refused.
        let stray = commit(&repo, t1, &[], "stray");
        assert!(matches!(
            graft(&repo, &roots, stray).expect("graft"),
            Err(GraftError::NotOnSnapshot { .. })
        ));
        let _ = CheckoutError::InvalidUrl(String::new());
    }

    #[test]
    fn checkins_update_on_fast_forward_force_or_own_last_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let empty = edit(repo.odb(), None, &[]).expect("tree");
        let a = commit(&repo, empty, &[], "a");
        let b = commit(&repo, empty, &[a], "b");
        let other = commit(&repo, empty, &[], "other");
        let decide =
            |prev, new, last, force| decide_checkin(&repo, prev, new, last, force).expect("decide");
        assert_eq!(decide(None, b, None, false), CheckinDecision::Create);
        assert_eq!(decide(Some(b), b, None, false), CheckinDecision::Unchanged);
        assert_eq!(decide(Some(a), b, None, false), CheckinDecision::Update);
        assert_eq!(
            decide(Some(b), other, None, false),
            CheckinDecision::Diverged
        );
        assert_eq!(
            decide(Some(b), other, Some(b), false),
            CheckinDecision::Update
        );
        assert_eq!(decide(Some(b), other, None, true), CheckinDecision::Update);
    }
}
