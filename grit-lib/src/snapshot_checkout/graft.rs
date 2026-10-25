//! Grafting: turning commits made on a checkout's snapshots into commits on
//! the server's real history.
//!
//! A checkout's commits sit on snapshot commits whose trees hold only the
//! checkout's paths. To check them in, the server replays each one onto the
//! real history: the commit's changes against its parent (in snapshot space)
//! are applied path by path to the full tree of the parent's graft (a
//! snapshot's graft is the real commit it stands for). The new commit keeps
//! the author, committer, message and other headers; only its tree and
//! parents change, so a signature (`gpgsig`, `gpgsig-sha256`) or merged tag
//! (`mergetag`) no longer holds for it and is dropped. Replays are
//! deterministic, so checking in the same commit twice gives the same commit.
//!
//! A change applies only where the full tree holds what the change started
//! from. When it doesn't (a file outside the checkout's view that the target
//! holds differently), the graft is refused for that commit. A file that
//! replaces a directory applies when the full tree's directory is the one the
//! checkout saw, so no file the checkout can't see is dropped.
//!
//! A merge commit is replayed onto a three-way merge of its parents' grafts,
//! so what the target changed outside the checkout's paths since an older
//! snapshot is kept rather than reverted. Inside the paths the merge commit's
//! own tree wins: it records how the merge was resolved. Parents whose grafts
//! conflict outside the paths refuse the graft.

use std::collections::{HashMap, HashSet};

use crate::merge_base::{
    merge_bases_first_vs_rest, walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, remove_header_lines, serialize_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;

use super::tree::{diff, edit, lookup, Change, TREE_MODE};
use super::{in_view, view_tree, CheckoutError, Result, SnapshotRoot};

/// Why a branch could not be grafted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraftError {
    /// A commit's parent is neither a snapshot of the checkout nor another of
    /// the branch's commits.
    #[error("commit {commit} does not build on this checkout's snapshots")]
    NotOnSnapshot {
        /// The commit (12 hex digits).
        commit: String,
    },
    /// A commit changes a path the target holds differently from what the
    /// commit started from.
    #[error(
        "commit {commit} changes {path}{}, and the target branch holds it differently",
        if *outside_paths { ", outside this checkout's paths" } else { "" }
    )]
    Conflict {
        /// The commit (12 hex digits).
        commit: String,
        /// The path.
        path: String,
        /// Whether the path is outside the checkout's paths, where a refresh
        /// never brings in what the target has.
        outside_paths: bool,
    },
    /// A merge commit's parents, grafted, conflict in paths outside the
    /// checkout's paths, which the checkout can't resolve.
    #[error("merge commit {commit} joins changes that conflict in {}, outside this checkout's paths", paths.join(", "))]
    MergeConflict {
        /// The merge commit (12 hex digits).
        commit: String,
        /// The conflicting paths.
        paths: Vec<String>,
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

/// Why one commit's replay was refused, before the commit is named.
enum Refused {
    Conflict(Vec<u8>),
    MergeConflict(Vec<String>),
}

/// Replay `tip` and the commits under it, down to the checkout's snapshot
/// commits (`roots`), onto the real history.
///
/// # Parameters
///
/// - `repo`: the served repository, holding the checkout's pushed commits.
/// - `roots`: the checkout's snapshot commits and the real commits they stand for.
/// - `paths`: the checkout's paths (empty: the whole tree), which decide
///   what a merge commit's tree is the answer for.
/// - `tip`: the branch tip the checkout pushed.
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
    paths: &[String],
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
        let parent_tree = commit_tree(odb, first)?;
        let replayed = if mapped.len() == 1 {
            replay(odb, commit_tree(odb, mapped[0])?, parent_tree, data.tree)?
        } else {
            replay_merge(repo, &mapped, parent_tree, data.tree, paths)?
        };
        data.tree = match replayed {
            Ok(tree) => tree,
            Err(Refused::Conflict(path)) => {
                return Ok(Err(GraftError::Conflict {
                    commit: short,
                    outside_paths: !in_view(&path, paths),
                    path: String::from_utf8_lossy(&path).into_owned(),
                }))
            }
            Err(Refused::MergeConflict(paths)) => {
                return Ok(Err(GraftError::MergeConflict {
                    commit: short,
                    paths,
                }))
            }
        };
        data.parents = mapped;
        data.extra_headers = remove_header_lines(&data.extra_headers, |key| {
            matches!(key, b"gpgsig" | b"gpgsig-sha256" | b"mergetag")
        });
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

fn commit_tree(odb: &Odb, commit: ObjectId) -> crate::error::Result<ObjectId> {
    Ok(parse_commit(&odb.read(&commit)?.data)?.tree)
}

/// Whether `change`, made against the snapshot-space tree `from`, applies to
/// the full tree `onto`: it holds what the change started from, or, for a
/// file replacing a directory, the very directory `from` has there (whose
/// files the change's removals account for).
fn applies(odb: &Odb, onto: ObjectId, from: ObjectId, change: &Change) -> Result<bool> {
    let have = lookup(odb, onto, &change.path)?;
    if have == change.old {
        return Ok(true);
    }
    let replaces_dir =
        change.old.is_none() && change.new.is_some() && have.is_some_and(|e| e.0 == TREE_MODE);
    Ok(replaces_dir && lookup(odb, from, &change.path)? == have)
}

/// `onto` with the changes from `parent_tree` to `tree` made.
fn replay(
    odb: &Odb,
    onto: ObjectId,
    parent_tree: ObjectId,
    tree: ObjectId,
) -> Result<std::result::Result<ObjectId, Refused>> {
    let mut edits = Vec::new();
    for change in diff(odb, Some(parent_tree), Some(tree))? {
        if !applies(odb, onto, parent_tree, &change)? {
            return Ok(Err(Refused::Conflict(change.path)));
        }
        edits.push((change.path, change.new));
    }
    Ok(Ok(if edits.is_empty() {
        onto
    } else {
        edit(odb, Some(onto), &edits)?
    }))
}

/// The tree of a merge commit (snapshot tree `tree`, first parent's
/// snapshot tree `parent_tree`) whose parents' grafts are `mapped`.
fn replay_merge(
    repo: &Repository,
    mapped: &[ObjectId],
    parent_tree: ObjectId,
    tree: ObjectId,
    paths: &[String],
) -> Result<std::result::Result<ObjectId, Refused>> {
    let odb = repo.odb();
    let (onto, conflicts) = merge_parents(repo, mapped)?;
    let hidden: Vec<String> = conflicts
        .into_iter()
        .filter(|p| !in_view(p.as_bytes(), paths))
        .collect();
    if !hidden.is_empty() {
        return Ok(Err(Refused::MergeConflict(hidden)));
    }
    let mut edits = Vec::new();
    // Inside the paths the merge commit's tree is the answer.
    let view = view_tree(odb, onto, paths)?;
    for change in diff(odb, Some(view), Some(view_tree(odb, tree, paths)?))? {
        if !applies(odb, onto, view, &change)? {
            return Ok(Err(Refused::Conflict(change.path)));
        }
        edits.push((change.path, change.new));
    }
    // Outside them, what the merge commit itself changes in files the
    // checkout has, where the merged parents don't already hold it.
    for change in diff(odb, Some(parent_tree), Some(tree))? {
        if in_view(&change.path, paths) || lookup(odb, onto, &change.path)? == change.new {
            continue;
        }
        if !applies(odb, onto, parent_tree, &change)? {
            return Ok(Err(Refused::Conflict(change.path)));
        }
        edits.push((change.path, change.new));
    }
    Ok(Ok(if edits.is_empty() {
        onto
    } else {
        edit(odb, Some(onto), &edits)?
    }))
}

/// The three-way merge of the trees of `commits` (merged in order, each over
/// its merge base with those before it), and the paths that conflict.
fn merge_parents(repo: &Repository, commits: &[ObjectId]) -> Result<(ObjectId, Vec<String>)> {
    let odb = repo.odb();
    let mut tree = commit_tree(odb, commits[0])?;
    let mut conflicts = Vec::new();
    for (i, other) in commits.iter().enumerate().skip(1) {
        let theirs = commit_tree(odb, *other)?;
        if theirs == tree {
            continue;
        }
        let base = match merge_bases_first_vs_rest(repo, *other, &commits[..i])?.first() {
            Some(base) => commit_tree(odb, *base)?,
            None => edit(odb, None, &[])?,
        };
        let merged = merge_trees_three_way(
            repo,
            base,
            tree,
            theirs,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
        )?;
        conflicts.extend(
            merged
                .index
                .entries()
                .iter()
                .filter(|e| e.stage() != 0)
                .map(|e| String::from_utf8_lossy(&e.path).into_owned()),
        );
        tree = merged.result_tree;
    }
    conflicts.sort();
    conflicts.dedup();
    Ok((tree, conflicts))
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
        let src = vec!["src".to_owned()];
        let snap = make_snapshot(&repo, base, &src, None).expect("snapshot");
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

        let grafted = graft(&repo, &roots, &src, c2)
            .expect("graft")
            .expect("grafted");
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
            graft(&repo, &roots, &src, c2)
                .expect("graft")
                .expect("grafted"),
            grafted
        );
        // The snapshot itself grafts to its base.
        assert_eq!(
            graft(&repo, &roots, &src, snap.commit)
                .expect("graft")
                .expect("grafted")
                .tip,
            base
        );

        // A commit not on the snapshot is refused.
        let stray = commit(&repo, t1, &[], "stray");
        assert!(matches!(
            graft(&repo, &roots, &src, stray).expect("graft"),
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

    /// A repository whose main has `README`, `src/a`, `src/lib/{x,y}`,
    /// `src/lib/sub/z` and `docs/d`, and a snapshot of `src`.
    fn fixture() -> (
        tempfile::TempDir,
        Repository,
        ObjectId,
        super::super::Snapshot,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let full = edit(
            repo.odb(),
            None,
            &[
                (b"README".to_vec(), Some(blob(&repo, "top"))),
                (b"src/a".to_vec(), Some(blob(&repo, "a"))),
                (b"src/lib/x".to_vec(), Some(blob(&repo, "x"))),
                (b"src/lib/y".to_vec(), Some(blob(&repo, "y"))),
                (b"src/lib/sub/z".to_vec(), Some(blob(&repo, "z"))),
                (b"docs/d".to_vec(), Some(blob(&repo, "d"))),
            ],
        )
        .expect("tree");
        let base = commit(&repo, full, &[], "base");
        let snap = make_snapshot(&repo, base, &["src".to_owned()], None).expect("snapshot");
        (dir, repo, base, snap)
    }

    fn roots_of(snaps: &[&super::super::Snapshot]) -> Vec<SnapshotRoot> {
        snaps
            .iter()
            .map(|s| SnapshotRoot {
                commit: s.commit.to_hex(),
                base: s.base.to_hex(),
            })
            .collect()
    }

    fn tree_of(repo: &Repository, commit: ObjectId) -> ObjectId {
        parse_commit(&repo.odb().read(&commit).expect("read").data)
            .expect("parse")
            .tree
    }

    #[test]
    fn grafts_drop_signatures_and_merge_tags_but_keep_other_headers() {
        let (_dir, repo, base, snap) = fixture();
        let odb = repo.odb();
        let tree = edit(
            odb,
            Some(snap.tree),
            &[(b"src/a".to_vec(), Some(blob(&repo, "a2")))],
        )
        .expect("tree");
        let signed = format!(
            "tree {}\nparent {}\nauthor A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n\
             change-id zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\n\
             gpgsig -----BEGIN SSH SIGNATURE-----\n abc\n -----END SSH SIGNATURE-----\n\
             gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n def\n -----END PGP SIGNATURE-----\n\
             mergetag object {}\n type commit\n tag v1\n\nSigned change\n",
            tree.to_hex(),
            snap.commit.to_hex(),
            base.to_hex(),
        );
        let raw = odb
            .write(ObjectKind::Commit, signed.as_bytes())
            .expect("commit");
        let roots = roots_of(&[&snap]);
        let grafted = graft(&repo, &roots, &["src".to_owned()], raw)
            .expect("graft")
            .expect("grafted");
        let bytes = odb.read(&grafted.tip).expect("read").data;
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("gpgsig") && !text.contains("mergetag"),
            "{text}"
        );
        assert!(
            text.contains("\nchange-id zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\n"),
            "{text}"
        );
        assert!(text.contains("author A <a@e> 1700000000 +0000\n"), "{text}");
        assert!(text.ends_with("\n\nSigned change\n"), "{text}");
        assert_eq!(
            graft(&repo, &roots, &["src".to_owned()], raw)
                .expect("graft")
                .expect("grafted"),
            grafted,
            "still deterministic"
        );
    }

    #[test]
    fn a_directory_replaced_by_a_file_grafts_when_nothing_is_hidden_there() {
        let (_dir, repo, base, snap) = fixture();
        let odb = repo.odb();
        let roots = roots_of(&[&snap]);
        let paths = vec!["src".to_owned()];
        let file = blob(&repo, "now a file");
        let tree = edit(
            odb,
            Some(snap.tree),
            &[
                (b"src/lib/x".to_vec(), None),
                (b"src/lib/y".to_vec(), None),
                (b"src/lib/sub/z".to_vec(), None),
                (b"src/lib".to_vec(), Some(file)),
            ],
        )
        .expect("tree");
        assert_eq!(lookup(odb, tree, b"src/lib").expect("lookup"), Some(file));
        let c = commit(&repo, tree, &[snap.commit], "lib becomes a file");
        let grafted = graft(&repo, &roots, &paths, c)
            .expect("graft")
            .expect("grafted");
        let full = tree_of(&repo, grafted.tip);
        assert_eq!(lookup(odb, full, b"src/lib").expect("lookup"), Some(file));
        assert_eq!(
            lookup(odb, full, b"docs/d").expect("lookup"),
            Some(blob(&repo, "d"))
        );

        // And back: the file becomes a directory again.
        let back = commit(&repo, snap.tree, &[c], "lib is a directory again");
        let grafted = graft(&repo, &roots, &paths, back)
            .expect("graft")
            .expect("grafted");
        assert_eq!(tree_of(&repo, grafted.tip), tree_of(&repo, base));

        // Where the target's directory has files the checkout can't see
        // (src/lib/sub, for a checkout of src/lib/x), the file would drop
        // them: refused.
        let deep = make_snapshot(&repo, base, &["src/lib/x".to_owned()], None).expect("snapshot");
        assert_eq!(
            lookup(odb, deep.tree, b"src/lib/sub").expect("lookup"),
            None
        );
        let tree = edit(
            odb,
            Some(deep.tree),
            &[
                (b"src/lib/x".to_vec(), None),
                (b"src/lib/y".to_vec(), None),
                (b"src/lib".to_vec(), Some(blob(&repo, "file"))),
            ],
        )
        .expect("tree");
        let c = commit(&repo, tree, &[deep.commit], "hide sub");
        assert_eq!(
            graft(&repo, &roots_of(&[&deep]), &["src/lib/x".to_owned()], c).expect("graft"),
            Err(GraftError::Conflict {
                commit: c.to_hex()[..12].to_owned(),
                path: "src/lib".to_owned(),
                outside_paths: false,
            })
        );
    }

    #[test]
    fn a_change_outside_the_paths_the_target_holds_differently_is_refused() {
        let (_dir, repo, _base, snap) = fixture();
        let odb = repo.odb();
        let tree = edit(
            odb,
            Some(snap.tree),
            &[(b"docs/d".to_vec(), Some(blob(&repo, "mine")))],
        )
        .expect("tree");
        let c = commit(&repo, tree, &[snap.commit], "write docs");
        assert_eq!(
            graft(&repo, &roots_of(&[&snap]), &["src".to_owned()], c).expect("graft"),
            Err(GraftError::Conflict {
                commit: c.to_hex()[..12].to_owned(),
                path: "docs/d".to_owned(),
                outside_paths: true,
            })
        );
    }

    #[test]
    fn a_merge_of_a_newer_snapshot_keeps_the_targets_changes_outside_the_paths() {
        let (_dir, repo, base, s1) = fixture();
        let odb = repo.odb();
        let paths = vec!["src".to_owned()];
        // Work on the old snapshot.
        let t = edit(
            odb,
            Some(s1.tree),
            &[(b"src/a".to_vec(), Some(blob(&repo, "a2")))],
        )
        .expect("tree");
        let work = commit(&repo, t, &[s1.commit], "change a");
        // The target moves: outside the paths (docs) and inside them (src/lib/x).
        let moved = edit(
            odb,
            Some(tree_of(&repo, base)),
            &[
                (b"docs/d".to_vec(), Some(blob(&repo, "d by bob"))),
                (b"src/lib/x".to_vec(), Some(blob(&repo, "x by bob"))),
            ],
        )
        .expect("tree");
        let b2 = commit(&repo, moved, &[base], "bob");
        let s2 = make_snapshot(&repo, b2, &paths, Some(s1.commit)).expect("snapshot");
        // A merge pull: the work branch merges the newer snapshot.
        let merged_view = edit(
            odb,
            Some(s2.tree),
            &[(b"src/a".to_vec(), Some(blob(&repo, "a2")))],
        )
        .expect("tree");
        let merge = commit(&repo, merged_view, &[work, s2.commit], "merge");
        let roots = roots_of(&[&s1, &s2]);
        let grafted = graft(&repo, &roots, &paths, merge)
            .expect("graft")
            .expect("grafted");
        let tip = parse_commit(&odb.read(&grafted.tip).expect("read").data).expect("parse");
        assert_eq!(tip.parents.len(), 2);
        assert_eq!(tip.parents[1], b2);
        let full = tip.tree;
        assert_eq!(
            lookup(odb, full, b"docs/d").expect("lookup"),
            Some(blob(&repo, "d by bob")),
            "the target's change outside the paths is kept"
        );
        assert_eq!(
            lookup(odb, full, b"src/lib/x").expect("lookup"),
            Some(blob(&repo, "x by bob"))
        );
        assert_eq!(
            lookup(odb, full, b"src/a").expect("lookup"),
            Some(blob(&repo, "a2"))
        );

        // Inside the paths the merge's own resolution wins: here it keeps the
        // old x though bob changed it.
        let resolved = edit(
            odb,
            Some(merged_view),
            &[(b"src/lib/x".to_vec(), Some(blob(&repo, "x")))],
        )
        .expect("tree");
        let merge = commit(&repo, resolved, &[work, s2.commit], "merge, keep x");
        let grafted = graft(&repo, &roots, &paths, merge)
            .expect("graft")
            .expect("grafted");
        let full = tree_of(&repo, grafted.tip);
        assert_eq!(
            lookup(odb, full, b"src/lib/x").expect("lookup"),
            Some(blob(&repo, "x"))
        );
        assert_eq!(
            lookup(odb, full, b"docs/d").expect("lookup"),
            Some(blob(&repo, "d by bob"))
        );
    }

    #[test]
    fn a_merge_whose_parents_conflict_outside_the_paths_is_refused() {
        let (_dir, repo, base, s1) = fixture();
        let odb = repo.odb();
        let paths = vec!["src".to_owned()];
        // Two real commits change docs/d (outside the paths) differently; the
        // checkout has a snapshot of each and merges them.
        let side = |text: &str, msg: &str| {
            let full = edit(
                odb,
                Some(tree_of(&repo, base)),
                &[(b"docs/d".to_vec(), Some(blob(&repo, text)))],
            )
            .expect("tree");
            let real = commit(&repo, full, &[base], msg);
            make_snapshot(&repo, real, &paths, Some(s1.commit)).expect("snapshot")
        };
        let mine = side("d mine", "mine");
        let bob = side("d by bob", "bob");
        assert_eq!(mine.tree, bob.tree, "the checkout can't see the difference");
        let merge = commit(&repo, bob.tree, &[mine.commit, bob.commit], "merge");
        assert_eq!(
            graft(&repo, &roots_of(&[&s1, &mine, &bob]), &paths, merge).expect("graft"),
            Err(GraftError::MergeConflict {
                commit: merge.to_hex()[..12].to_owned(),
                paths: vec!["docs/d".to_owned()],
            })
        );
    }
}
