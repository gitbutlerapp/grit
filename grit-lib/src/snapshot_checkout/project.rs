//! Projection: a branch of the server's real history as a checkout sees it,
//! the reverse of a graft.
//!
//! Each commit of the branch that the checkout's target branch lacks is
//! written again with its tree cut down to the checkout's paths
//! ([`super::filtered_tree`]), on the projection of its parents. A parent on
//! the target becomes the checkout's newest snapshot under it. Author,
//! committer and message are kept; a signature or merged tag no longer holds
//! and is dropped, as in a graft, and a commit without a change id gets the
//! one derived from the real commit ([`ChangeId::derived_from_commit_id`]),
//! so a rebased copy of it can be traced back. A commit that only touched
//! paths the checkout doesn't have becomes an empty commit, so the history
//! still lines up with the server's.
//!
//! Every pair is recorded in the repository's [`GraftLog`], so work on top of
//! a projected commit grafts onto the real one. A real commit that is the
//! graft of one of the checkout's own commits (or that was projected before
//! on the same parents, to the same tree) projects to that commit as it is,
//! and files outside the paths that such a commit carries stay in the
//! projections above it while the real commits still have them as files, so
//! they don't look deleted. Another checkout's commit, on the same snapshot
//! but with other paths, is only reused when it has the tree this
//! checkout's projection would make.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::change_id::{change_id, set_change_id, ChangeId};
use crate::merge_base::{
    is_ancestor, walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::objects::{parse_commit, parse_tree, serialize_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;

use super::graft::{parents_first, without_signatures};
use super::graft_log::GraftLog;
use super::tree::{edit, lookup, TREE_MODE};
use super::{
    in_ref, in_view, out_ref, same_branch_name, valid_branch_name, view_tree, BranchInfo,
    CheckoutInfo, Result, SnapshotRoot,
};

/// Why a branch could not be projected for a checkout.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectError {
    /// The name is not a valid branch name.
    #[error("not a valid branch name")]
    InvalidName,
    /// The name is (in any case) the branch the checkout follows, which a
    /// refresh brings, as snapshots.
    #[error("the branch the checkout follows ({target})")]
    TargetBranch {
        /// The checkout's branch, as the server has it.
        target: String,
    },
    /// The server has no such branch.
    #[error("no such branch")]
    NoSuchBranch,
    /// The target branch already has every commit of the branch.
    #[error("already on the target branch")]
    OnTarget,
    /// The branch builds on a commit of the target from before the
    /// checkout's first snapshot, which no snapshot stands for.
    #[error("builds on {commit}, from before the checkout's first snapshot")]
    BeforeFirstSnapshot {
        /// That commit (12 hex digits).
        commit: String,
    },
}

impl ProjectError {
    /// A short, stable name for the kind of refusal (the serialized `kind`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidName => "invalid_name",
            Self::TargetBranch { .. } => "target_branch",
            Self::NoSuchBranch => "no_such_branch",
            Self::OnTarget => "on_target",
            Self::BeforeFirstSnapshot { .. } => "before_first_snapshot",
        }
    }
}

/// One commit of a branch [`project`] showed the checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectedCommit {
    /// The real commit.
    pub real: ObjectId,
    /// What the checkout sees of it.
    pub projected: ObjectId,
    /// Whether `projected` was there already: a checkout commit on the same
    /// snapshots that the real one is the graft of (one of this checkout's
    /// own, or of a checkout with the same snapshots), or an earlier
    /// projection.
    pub reused: bool,
}

/// A branch as [`project`] showed it to a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// The projection of the branch's tip: a snapshot commit when the branch
    /// is at a commit a snapshot stands for.
    pub tip: ObjectId,
    /// The branch's commits the target lacks, oldest first (each after its
    /// parents among them).
    pub commits: Vec<ProjectedCommit>,
}

impl Projection {
    /// The pairs the projection made, as (projected commit, real commit), to
    /// record in the graft log.
    #[must_use]
    pub fn made(&self) -> Vec<(ObjectId, ObjectId)> {
        self.commits
            .iter()
            .filter(|c| !c.reused)
            .map(|c| (c.projected, c.real))
            .collect()
    }

    /// How many commits were shown as commits that were there already.
    #[must_use]
    pub fn reused(&self) -> usize {
        self.commits.iter().filter(|c| c.reused).count()
    }
}

/// The snapshot commits of `roots` with the real commits they stand for,
/// oldest first, skipping any that are not object ids.
fn root_pairs(roots: &[SnapshotRoot]) -> Vec<(ObjectId, ObjectId)> {
    roots
        .iter()
        .filter_map(|r| {
            Some((
                ObjectId::from_hex(&r.commit).ok()?,
                ObjectId::from_hex(&r.base).ok()?,
            ))
        })
        .collect()
}

/// Show the branch at `tip` to a checkout: the reverse of
/// [`super::graft`] (see the module docs).
///
/// The commits projected are those of `tip` that neither `targets` nor any
/// snapshot's real commit has; a branch at the real commit a snapshot stands
/// for projects to that snapshot.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `roots`: the checkout's snapshot commits and the real commits they
///   stand for, oldest first.
/// - `paths`: the checkout's paths (empty: the whole tree).
/// - `known`: the repository's graft log, whose checkout commits for a real
///   commit are reused when their parents are the projected parents and
///   their tree is the one the projection would make, or they are the
///   checkout's own; `None` to make every commit afresh. Nothing is recorded
///   in it here.
/// - `own`: the tips the checkout pushed its branches to (its
///   [`super::out_ref`]s). A logged commit they reach is the checkout's own
///   and is shown as it is, with the files outside `paths` it carries.
/// - `targets`: the tips of the target branch the checkout follows.
/// - `tip`: the branch's tip.
///
/// # Returns
///
/// The projection, whose new pairs ([`Projection::made`]) the caller
/// records; `Ok(Err(..))` with [`ProjectError::OnTarget`] or
/// [`ProjectError::BeforeFirstSnapshot`].
///
/// # Errors
///
/// Walk, object and graft log failures.
pub fn project(
    repo: &Repository,
    roots: &[SnapshotRoot],
    paths: &[String],
    known: Option<&GraftLog>,
    own: &[ObjectId],
    targets: &[ObjectId],
    tip: ObjectId,
) -> Result<std::result::Result<Projection, ProjectError>> {
    let roots = root_pairs(roots);
    if let Some((root, _)) = roots.iter().rev().find(|(_, base)| *base == tip) {
        return Ok(Ok(Projection {
            tip: *root,
            commits: Vec::new(),
        }));
    }
    let mut stop = targets.to_vec();
    stop.extend(roots.iter().map(|(_, base)| *base));
    let (_, newest_first) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        &[tip],
        &stop,
        ReachableWalkLimit::Unlimited,
    )?;
    if newest_first.is_empty() {
        return Ok(Err(ProjectError::OnTarget));
    }
    let odb = repo.odb();
    let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut commits = Vec::with_capacity(newest_first.len());
    for real in parents_first(repo, &newest_first)? {
        let mut data = parse_commit(&odb.read(&real)?.data)?;
        let mut parents: Vec<ObjectId> = Vec::with_capacity(data.parents.len());
        for parent in &data.parents {
            let mapped = match map.get(parent) {
                Some(projected) => *projected,
                None => match snapshot_under(repo, &roots, *parent)? {
                    Some(root) => root,
                    None => {
                        return Ok(Err(ProjectError::BeforeFirstSnapshot {
                            commit: parent.to_hex()[..12].to_owned(),
                        }))
                    }
                },
            };
            if !parents.contains(&mapped) {
                parents.push(mapped);
            }
        }
        let tree = projected_tree(odb, paths, data.tree, parents.first().copied())?;
        let projected = match reusable(repo, known, own, &real, &parents, tree)? {
            Some(logged) => ProjectedCommit {
                real,
                projected: logged,
                reused: true,
            },
            None => {
                data.tree = tree;
                data.parents = parents;
                data.extra_headers = without_signatures(&data.extra_headers);
                if change_id(&data).is_none() {
                    set_change_id(&mut data, &ChangeId::derived_from_commit_id(&real));
                }
                ProjectedCommit {
                    real,
                    projected: odb.write(ObjectKind::Commit, &serialize_commit(&data))?,
                    reused: false,
                }
            }
        };
        map.insert(real, projected.projected);
        commits.push(projected);
    }
    match map.get(&tip) {
        Some(tip) => Ok(Ok(Projection { tip: *tip, commits })),
        None => Ok(Err(ProjectError::OnTarget)),
    }
}

/// The newest snapshot of `roots` whose real commit is `commit` or one of its
/// ancestors.
fn snapshot_under(
    repo: &Repository,
    roots: &[(ObjectId, ObjectId)],
    commit: ObjectId,
) -> Result<Option<ObjectId>> {
    for (root, base) in roots.iter().rev() {
        if *base == commit || is_ancestor(repo, *base, commit)? {
            return Ok(Some(*root));
        }
    }
    Ok(None)
}

/// A commit `known` records for `real` that the checkout can be shown as it
/// is: one on `parents` whose tree is `tree` (what a fresh projection would
/// have), or the checkout's own (reached from `own`).
///
/// Snapshots of different paths can be the same commit (when one path holds
/// only another), so another checkout's commit on the same parents can carry
/// files outside these paths, and is not reused.
fn reusable(
    repo: &Repository,
    known: Option<&GraftLog>,
    own: &[ObjectId],
    real: &ObjectId,
    parents: &[ObjectId],
    tree: ObjectId,
) -> Result<Option<ObjectId>> {
    let Some(log) = known else {
        return Ok(None);
    };
    for raw in log.raws(real)? {
        let Ok(object) = repo.odb().read(&raw) else {
            continue;
        };
        if object.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&object.data)?;
        if commit.parents != parents {
            continue;
        }
        if commit.tree == tree || reaches(repo, own, raw)? {
            return Ok(Some(raw));
        }
    }
    Ok(None)
}

/// Whether one of `tips` is `commit` or has it in its history.
fn reaches(repo: &Repository, tips: &[ObjectId], commit: ObjectId) -> Result<bool> {
    for tip in tips {
        if is_ancestor(repo, commit, *tip)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `real_tree` as the checkout sees it: cut down to `paths`, plus the files
/// outside them that `parent` (a projected commit) carries and `real_tree`
/// still has as files, as `real_tree` has them.
fn projected_tree(
    odb: &Odb,
    paths: &[String],
    real_tree: ObjectId,
    parent: Option<ObjectId>,
) -> Result<ObjectId> {
    let tree = view_tree(odb, real_tree, paths)?;
    let Some(parent) = parent else {
        return Ok(tree);
    };
    let parent_tree = parse_commit(&odb.read(&parent)?.data)?.tree;
    let mut extras = Vec::new();
    outside_files(odb, parent_tree, paths, &mut Vec::new(), &mut extras)?;
    let mut edits = Vec::new();
    for path in extras {
        // A file the real commit turned into a directory is gone; the
        // directory's files are outside the paths too.
        if let Some(entry) = lookup(odb, real_tree, &path)?.filter(|e| e.0 != TREE_MODE) {
            edits.push((path, Some(entry)));
        }
    }
    if edits.is_empty() {
        return Ok(tree);
    }
    Ok(edit(odb, Some(tree), &edits)?)
}

/// The files under `tree` (at `prefix`) outside `paths`, without going into
/// the directories `paths` select.
fn outside_files(
    odb: &Odb,
    tree: ObjectId,
    paths: &[String],
    prefix: &mut Vec<u8>,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    for entry in parse_tree(&odb.read(&tree)?.data)? {
        let len = prefix.len();
        if len > 0 {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(&entry.name);
        if entry.mode == TREE_MODE {
            // A directory above a path is walked: files directly in it are
            // in the view, but its other directories are not.
            let selected = paths.iter().map(String::as_bytes).any(|p| {
                prefix.as_slice() == p
                    || (prefix.starts_with(p) && prefix.get(p.len()) == Some(&b'/'))
            });
            if !selected {
                outside_files(odb, entry.oid, paths, prefix, out)?;
            }
        } else if !in_view(prefix, paths) {
            out.push(prefix.clone());
        }
        prefix.truncate(len);
    }
    Ok(())
}

/// Project the server's branch `name` for `checkout` (see [`project`]),
/// record the new pairs in `known`, and point [`in_ref`] at the projection
/// for the checkout to fetch.
///
/// The commits hidden are those of the branch the checkout follows
/// ([`CheckoutInfo::branch`]), which refreshes bring as snapshots.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `checkout`: the checkout the branch is shown to.
/// - `known`: the repository's graft log
///   ([`super::CheckoutStore::graft_log`]).
/// - `name`: the branch, without `refs/heads/`. Where the repository ignores
///   case (`core.ignorecase`), a name that differs from one of its branches
///   only in case is that branch.
///
/// # Returns
///
/// What the checkout fetches, named as the server spells the branch
/// ([`BranchInfo::name`]), or `Ok(Err(..))` with why the branch can't be
/// shown: an invalid name, the branch the checkout follows (in any case), a
/// branch the server doesn't have, or the refusals of [`project`].
///
/// # Errors
///
/// Ref, walk, object and graft log failures.
pub fn project_branch(
    repo: &Repository,
    checkout: &CheckoutInfo,
    known: &GraftLog,
    name: &str,
) -> Result<std::result::Result<BranchInfo, ProjectError>> {
    if !valid_branch_name(name) {
        return Ok(Err(ProjectError::InvalidName));
    }
    if same_branch_name(name, &checkout.branch) {
        return Ok(Err(ProjectError::TargetBranch {
            target: checkout.branch.clone(),
        }));
    }
    let Some((name, tip)) = server_branch(repo, name)? else {
        return Ok(Err(ProjectError::NoSuchBranch));
    };
    let name = name.as_str();
    let targets: Vec<ObjectId> =
        crate::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{}", checkout.branch))
            .into_iter()
            .collect();
    let own: Vec<ObjectId> = crate::refs::list_refs(repo.git_dir(), &out_ref(&checkout.id, ""))?
        .into_iter()
        .map(|(_, tip)| tip)
        .collect();
    let projection = match project(
        repo,
        &checkout.roots,
        &checkout.paths,
        Some(known),
        &own,
        &targets,
        tip,
    )? {
        Ok(projection) => projection,
        Err(refusal) => return Ok(Err(refusal)),
    };
    known.record(&projection.made())?;
    crate::refs::write_ref(repo.git_dir(), &in_ref(&checkout.id, name), &projection.tip)?;
    Ok(Ok(BranchInfo {
        name: name.to_owned(),
        tip: tip.to_hex(),
        projected: projection.tip.to_hex(),
        commits: projection.commits.len(),
        made: projection.made().len(),
        reused: projection.reused(),
    }))
}

/// The server's branch `name` and its tip, under the server's spelling.
///
/// Where the repository ignores case ([`super::checkin::ignores_case`]), a
/// name that differs from one of its branches only in case is that branch:
/// a loose ref would resolve under either spelling, while a packed one would
/// not, and work checked in under the other spelling would be refused as a
/// case conflict.
fn server_branch(repo: &Repository, name: &str) -> Result<Option<(String, ObjectId)>> {
    let branches: Vec<(String, ObjectId)> = crate::refs::list_refs(repo.git_dir(), "refs/heads/")?
        .into_iter()
        .filter_map(|(refname, tip)| {
            refname
                .strip_prefix("refs/heads/")
                .map(|branch| (branch.to_owned(), tip))
        })
        .collect();
    if let Some(exact) = branches.iter().find(|(branch, _)| branch == name) {
        return Ok(Some(exact.clone()));
    }
    if !super::checkin::ignores_case(repo)? {
        return Ok(None);
    }
    Ok(branches
        .into_iter()
        .find(|(branch, _)| same_branch_name(branch, name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::CommitData;
    use crate::repo::InitOptions;
    use crate::snapshot_checkout::{graft, make_snapshot, Snapshot};

    fn repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    fn blob(repo: &Repository, text: &str) -> (u32, ObjectId) {
        (
            0o100644,
            repo.odb()
                .write(ObjectKind::Blob, text.as_bytes())
                .expect("blob"),
        )
    }

    /// `base` (or an empty tree) with each `(path, text)` set, or removed for
    /// `None`.
    fn tree(repo: &Repository, base: Option<ObjectId>, files: &[(&str, Option<&str>)]) -> ObjectId {
        let edits: Vec<_> = files
            .iter()
            .map(|(path, text)| (path.as_bytes().to_vec(), text.map(|t| blob(repo, t))))
            .collect();
        edit(repo.odb(), base, &edits).expect("tree")
    }

    /// A commit of `tree` on `parents`, with `extra` headers after the
    /// committer.
    fn commit(
        repo: &Repository,
        tree: ObjectId,
        parents: &[ObjectId],
        msg: &str,
        extra: &str,
    ) -> ObjectId {
        let mut s = format!("tree {}\n", tree.to_hex());
        for p in parents {
            s.push_str(&format!("parent {}\n", p.to_hex()));
        }
        s.push_str(&format!(
            "author A <a@e> 1700000000 +0000\ncommitter C <c@e> 1700000100 +0000\n{extra}\n{msg}\n"
        ));
        repo.odb()
            .write(ObjectKind::Commit, s.as_bytes())
            .expect("commit")
    }

    fn read(repo: &Repository, oid: ObjectId) -> CommitData {
        parse_commit(&repo.odb().read(&oid).expect("read").data).expect("parse")
    }

    /// `parent` with `files` changed, committed as `msg`.
    fn change(
        repo: &Repository,
        parent: ObjectId,
        files: &[(&str, Option<&str>)],
        msg: &str,
    ) -> ObjectId {
        let t = tree(repo, Some(read(repo, parent).tree), files);
        commit(repo, t, &[parent], msg, "")
    }

    fn file(repo: &Repository, commit: ObjectId, path: &str) -> Option<String> {
        let entry =
            lookup(repo.odb(), read(repo, commit).tree, path.as_bytes()).expect("lookup")?;
        Some(String::from_utf8_lossy(&repo.odb().read(&entry.1).expect("blob").data).into_owned())
    }

    fn roots(snaps: &[&Snapshot]) -> Vec<SnapshotRoot> {
        snaps
            .iter()
            .map(|s| SnapshotRoot {
                commit: s.commit.to_hex(),
                base: s.base.to_hex(),
            })
            .collect()
    }

    /// A target with `README`, `src/a`, `src/lib/x` and `docs/d`, and a
    /// snapshot of `src` of it.
    fn fixture(repo: &Repository) -> (ObjectId, Snapshot) {
        let full = tree(
            repo,
            None,
            &[
                ("README", Some("top")),
                ("src/a", Some("a")),
                ("src/lib/x", Some("x")),
                ("docs/d", Some("d")),
            ],
        );
        let base = commit(repo, full, &[], "base", "");
        let snap = make_snapshot(repo, base, &["src".to_owned()], None).expect("snapshot");
        (base, snap)
    }

    #[test]
    fn branches_are_cut_to_the_paths_and_commits_on_other_paths_come_out_empty() {
        let (_dir, repo) = repo();
        let (base, snap) = fixture(&repo);
        let paths = vec!["src".to_owned()];
        // A signed real commit changing src and docs, then one changing docs only.
        let t1 = tree(
            &repo,
            Some(read(&repo, base).tree),
            &[("src/a", Some("a by bob")), ("docs/d", Some("d by bob"))],
        );
        let r1 = commit(
            &repo,
            t1,
            &[base],
            "Change a and the docs",
            "gpgsig -----BEGIN PGP SIGNATURE-----\n abc\n -----END PGP SIGNATURE-----\n",
        );
        let r2 = change(&repo, r1, &[("docs/d", Some("more docs"))], "Docs only");

        let projection = project(&repo, &roots(&[&snap]), &paths, None, &[], &[base], r2)
            .expect("project")
            .expect("projected");
        assert_eq!(projection.commits.len(), 2);
        assert_eq!(projection.reused(), 0);
        let [first, second] = [projection.commits[0], projection.commits[1]];
        assert_eq!((first.real, second.real), (r1, r2));
        assert_eq!(projection.tip, second.projected);
        assert_eq!(
            projection.made(),
            vec![(first.projected, r1), (second.projected, r2)]
        );

        let p1 = read(&repo, first.projected);
        assert_eq!(p1.parents, vec![snap.commit], "on the snapshot of its base");
        assert_eq!(
            file(&repo, first.projected, "src/a").as_deref(),
            Some("a by bob")
        );
        assert_eq!(
            file(&repo, first.projected, "README").as_deref(),
            Some("top")
        );
        assert_eq!(
            file(&repo, first.projected, "docs/d"),
            None,
            "outside the paths"
        );
        assert_eq!(p1.message, "Change a and the docs\n");
        assert_eq!(p1.author, "A <a@e> 1700000000 +0000");
        assert_eq!(p1.committer, "C <c@e> 1700000100 +0000");
        let text = String::from_utf8_lossy(&repo.odb().read(&first.projected).expect("read").data)
            .into_owned();
        assert!(!text.contains("gpgsig"), "{text}");
        assert_eq!(
            change_id(&p1),
            Some(ChangeId::derived_from_commit_id(&r1)),
            "the id the real commit already reports"
        );
        // The docs-only commit shows as an empty one.
        let p2 = read(&repo, second.projected);
        assert_eq!(p2.parents, vec![first.projected]);
        assert_eq!(p2.tree, p1.tree);
        assert_eq!(p2.message, "Docs only\n");

        // Deterministic.
        assert_eq!(
            project(&repo, &roots(&[&snap]), &paths, None, &[], &[base], r2).expect("project"),
            Ok(projection)
        );
    }

    #[test]
    fn a_checkouts_own_commits_show_as_they_are_with_their_files_outside_the_paths() {
        let (dir, repo) = repo();
        let (base, snap) = fixture(&repo);
        let paths = vec!["src".to_owned()];
        let log = GraftLog::at(dir.path().join("grafts"));
        let roots = roots(&[&snap]);
        // The checkout changes src/a and adds docs/new, outside its paths, and
        // checks in.
        let own = change(
            &repo,
            snap.commit,
            &[("src/a", Some("mine")), ("docs/new", Some("new"))],
            "Mine",
        );
        let grafted = graft(&repo, &roots, &paths, Some(&log), &[base], own)
            .expect("graft")
            .expect("grafted");
        log.record(&grafted.commits).expect("record");
        // Another checkout of the same snapshot isn't shown docs/new: the
        // commit is not its own, so it is cut to the paths afresh.
        let elsewhere = project(&repo, &roots, &paths, Some(&log), &[], &[base], grafted.tip)
            .expect("project")
            .expect("projected");
        assert_eq!(elsewhere.reused(), 0);
        assert_ne!(elsewhere.tip, own);
        assert_eq!(file(&repo, elsewhere.tip, "docs/new"), None);
        assert_eq!(file(&repo, elsewhere.tip, "src/a").as_deref(), Some("mine"));
        // A teammate builds on it with a full clone.
        let theirs = change(&repo, grafted.tip, &[("src/lib/x", Some("x2"))], "Theirs");

        let projection = project(&repo, &roots, &paths, Some(&log), &[own], &[base], theirs)
            .expect("project")
            .expect("projected");
        assert_eq!(projection.reused(), 1);
        assert_eq!(
            projection.commits[0],
            ProjectedCommit {
                real: grafted.tip,
                projected: own,
                reused: true,
            },
            "the checkout's own commit"
        );
        let shown = projection.tip;
        assert_eq!(read(&repo, shown).parents, vec![own]);
        assert_eq!(file(&repo, shown, "src/lib/x").as_deref(), Some("x2"));
        assert_eq!(
            file(&repo, shown, "docs/new").as_deref(),
            Some("new"),
            "a file outside the paths its parent carries is kept"
        );
        assert_eq!(file(&repo, shown, "docs/d"), None);
        assert_eq!(projection.made(), vec![(shown, theirs)]);

        // Once recorded, the projection is reused too; a commit removing the
        // outside file drops it.
        log.record(&projection.made()).expect("record");
        let gone = change(&repo, theirs, &[("docs/new", None)], "Drop new");
        let again = project(&repo, &roots, &paths, Some(&log), &[own], &[base], gone)
            .expect("project")
            .expect("projected");
        assert_eq!(again.reused(), 2);
        assert_eq!(again.commits[1].projected, shown);
        assert_eq!(file(&repo, again.tip, "docs/new"), None);
        assert_eq!(read(&repo, again.tip).parents, vec![shown]);
    }

    #[test]
    fn an_outside_file_that_became_a_directory_is_not_carried() {
        let (dir, repo) = repo();
        let (base, snap) = fixture(&repo);
        let paths = vec!["src".to_owned()];
        let log = GraftLog::at(dir.path().join("grafts"));
        let roots = roots(&[&snap]);
        let own = change(&repo, snap.commit, &[("other/x", Some("file"))], "Mine");
        let grafted = graft(&repo, &roots, &paths, Some(&log), &[base], own)
            .expect("graft")
            .expect("grafted");
        log.record(&grafted.commits).expect("record");
        // On the server, other/x becomes a directory.
        let theirs = change(
            &repo,
            grafted.tip,
            &[
                ("other/x", None),
                ("other/x/s1", Some("s1")),
                ("src/a", Some("theirs")),
            ],
            "Theirs",
        );

        let shown = project(&repo, &roots, &paths, Some(&log), &[own], &[base], theirs)
            .expect("project")
            .expect("projected");
        assert_eq!(read(&repo, shown.tip).parents, vec![own]);
        assert_eq!(file(&repo, shown.tip, "src/a").as_deref(), Some("theirs"));
        assert_eq!(
            lookup(repo.odb(), read(&repo, shown.tip).tree, b"other").expect("lookup"),
            None,
            "neither the file nor the directory that replaced it"
        );
    }

    #[test]
    fn another_checkouts_commit_on_the_same_snapshot_is_cut_to_these_paths() {
        let (dir, repo) = repo();
        let full = tree(
            &repo,
            None,
            &[("README", Some("top")), ("src/deep/y", Some("y"))],
        );
        let base = commit(&repo, full, &[], "base", "");
        let alice_paths = vec!["src".to_owned()];
        let bob_paths = vec!["src/deep".to_owned()];
        let alice = make_snapshot(&repo, base, &alice_paths, None).expect("snapshot");
        let bob = make_snapshot(&repo, base, &bob_paths, None).expect("snapshot");
        assert_eq!(alice.commit, bob.commit, "src holds only src/deep");
        let log = GraftLog::at(dir.path().join("grafts"));
        let a = change(&repo, alice.commit, &[("src/other/x", Some("x"))], "Alice");
        let ga = graft(
            &repo,
            &roots(&[&alice]),
            &alice_paths,
            Some(&log),
            &[base],
            a,
        )
        .expect("graft")
        .expect("grafted");
        log.record(&ga.commits).expect("record");

        let to_bob = project(
            &repo,
            &roots(&[&bob]),
            &bob_paths,
            Some(&log),
            &[],
            &[base],
            ga.tip,
        )
        .expect("project")
        .expect("projected");
        assert_eq!((to_bob.made().len(), to_bob.reused()), (1, 0));
        assert_eq!(file(&repo, to_bob.tip, "src/other/x"), None);
        assert_eq!(read(&repo, to_bob.tip).parents, vec![bob.commit]);
        // Alice is still shown hers: its tree is what her projection makes.
        let to_alice = project(
            &repo,
            &roots(&[&alice]),
            &alice_paths,
            Some(&log),
            &[],
            &[base],
            ga.tip,
        )
        .expect("project")
        .expect("projected");
        assert_eq!(to_alice.tip, a);
        assert_eq!(to_alice.reused(), 1);
    }

    #[test]
    fn two_checkouts_of_different_paths_extend_one_branch() {
        let (dir, repo) = repo();
        let full = tree(
            &repo,
            None,
            &[
                ("README", Some("top")),
                ("builtin/blame.c", Some("blame")),
                ("Documentation/blame.txt", Some("docs")),
                ("t/blame.sh", Some("test")),
            ],
        );
        let base = commit(&repo, full, &[], "base", "");
        let alice_paths = vec!["Documentation".to_owned(), "builtin".to_owned()];
        let bob_paths = vec!["Documentation".to_owned(), "t".to_owned()];
        let alice = make_snapshot(&repo, base, &alice_paths, None).expect("snapshot");
        let bob = make_snapshot(&repo, base, &bob_paths, None).expect("snapshot");
        let log = GraftLog::at(dir.path().join("grafts"));

        // 1. Alice changes builtin/ and Documentation/ and checks in.
        let a = change(
            &repo,
            alice.commit,
            &[
                ("builtin/blame.c", Some("blame by alice")),
                ("Documentation/blame.txt", Some("docs by alice")),
            ],
            "Alice",
        );
        let ga = graft(
            &repo,
            &roots(&[&alice]),
            &alice_paths,
            Some(&log),
            &[base],
            a,
        )
        .expect("graft")
        .expect("grafted");
        log.record(&ga.commits).expect("record");

        // 2. Bob is shown her commit: her Documentation/ change, not builtin/.
        let shown = project(
            &repo,
            &roots(&[&bob]),
            &bob_paths,
            Some(&log),
            &[],
            &[base],
            ga.tip,
        )
        .expect("project")
        .expect("projected");
        log.record(&shown.made()).expect("record");
        assert_eq!(
            file(&repo, shown.tip, "Documentation/blame.txt").as_deref(),
            Some("docs by alice")
        );
        assert_eq!(file(&repo, shown.tip, "builtin/blame.c"), None);

        // 3. Bob builds on it and checks in: his commit lands on her real one,
        // which keeps her builtin/ change.
        let b = change(
            &repo,
            shown.tip,
            &[
                ("Documentation/blame.txt", Some("docs by bob")),
                ("t/blame.sh", Some("test by bob")),
            ],
            "Bob",
        );
        let gb = graft(&repo, &roots(&[&bob]), &bob_paths, Some(&log), &[base], b)
            .expect("graft")
            .expect("grafted");
        assert_eq!(gb.commits.len(), 1, "only Bob's commit is replayed");
        assert_eq!(read(&repo, gb.tip).parents, vec![ga.tip]);
        assert_eq!(
            file(&repo, gb.tip, "builtin/blame.c").as_deref(),
            Some("blame by alice")
        );
        assert_eq!(
            file(&repo, gb.tip, "Documentation/blame.txt").as_deref(),
            Some("docs by bob")
        );
        assert_eq!(
            file(&repo, gb.tip, "t/blame.sh").as_deref(),
            Some("test by bob")
        );
        log.record(&gb.commits).expect("record");

        // 4. Alice is shown his commit on top of her own one, without t/.
        let back = project(
            &repo,
            &roots(&[&alice]),
            &alice_paths,
            Some(&log),
            &[],
            &[base],
            gb.tip,
        )
        .expect("project")
        .expect("projected");
        assert_eq!(back.commits[0].projected, a, "her own commit, as it is");
        assert_eq!(read(&repo, back.tip).parents, vec![a]);
        assert_eq!(
            file(&repo, back.tip, "Documentation/blame.txt").as_deref(),
            Some("docs by bob")
        );
        assert_eq!(file(&repo, back.tip, "t/blame.sh"), None);
        log.record(&back.made()).expect("record");
        // Grafting what she is shown answers from the log without a walk.
        assert_eq!(
            graft(
                &repo,
                &roots(&[&alice]),
                &alice_paths,
                Some(&log),
                &[base],
                back.tip
            )
            .expect("graft"),
            Ok(crate::snapshot_checkout::Grafted {
                tip: gb.tip,
                commits: Vec::new(),
            })
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn branches_the_checkout_cant_be_shown_are_refused() {
        let (dir, repo) = repo();
        let (base, snap) = fixture(&repo);
        let paths = vec!["src".to_owned()];
        let roots = roots(&[&snap]);
        // At the commit a snapshot stands for: that snapshot.
        assert_eq!(
            project(&repo, &roots, &paths, None, &[], &[base], base).expect("project"),
            Ok(Projection {
                tip: snap.commit,
                commits: Vec::new(),
            })
        );
        // Every commit already on the target.
        let moved = change(&repo, base, &[("README", Some("top2"))], "main moves");
        assert_eq!(
            project(&repo, &roots, &paths, None, &[], &[moved], moved).expect("project"),
            Err(ProjectError::OnTarget)
        );
        // A branch from before the first snapshot.
        let newer = make_snapshot(&repo, moved, &paths, Some(snap.commit)).expect("snapshot");
        let old = change(&repo, base, &[("src/a", Some("old work"))], "Old work");
        let newer_only = super::super::SnapshotRoot {
            commit: newer.commit.to_hex(),
            base: moved.to_hex(),
        };
        assert_eq!(
            project(
                &repo,
                std::slice::from_ref(&newer_only),
                &paths,
                None,
                &[],
                &[moved],
                old
            )
            .expect("project"),
            Err(ProjectError::BeforeFirstSnapshot {
                commit: base.to_hex()[..12].to_owned()
            })
        );
        // A parent on the target maps to the newest snapshot under it.
        let both = vec![roots[0].clone(), newer_only];
        let on_moved = change(&repo, moved, &[("src/a", Some("new work"))], "New work");
        let shown = project(&repo, &both, &paths, None, &[], &[moved], on_moved)
            .expect("project")
            .expect("projected");
        assert_eq!(read(&repo, shown.tip).parents, vec![newer.commit]);
        let shown_old = project(&repo, &both, &paths, None, &[], &[moved], old)
            .expect("project")
            .expect("projected");
        assert_eq!(read(&repo, shown_old.tip).parents, vec![snap.commit]);

        // project_branch checks the name, and puts the projection at in_ref.
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &moved).expect("main");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/feature", &on_moved).expect("ref");
        let checkout = CheckoutInfo {
            id: "0123456789abcdef0123".to_owned(),
            name: "demo".to_owned(),
            branch: "main".to_owned(),
            paths,
            base: moved.to_hex(),
            root: newer.commit.to_hex(),
            root_object: String::new(),
            tree: newer.tree.to_hex(),
            roots: both,
            files: 0,
            bytes: 0,
            bundle_bytes: 0,
            work: None,
        };
        let log = GraftLog::at(dir.path().join("grafts"));
        let shown_to = |name: &str| project_branch(&repo, &checkout, &log, name).expect("project");
        assert_eq!(shown_to("a..b"), Err(ProjectError::InvalidName));
        assert_eq!(
            shown_to("MAIN"),
            Err(ProjectError::TargetBranch {
                target: "main".to_owned()
            })
        );
        assert_eq!(shown_to("nope"), Err(ProjectError::NoSuchBranch));
        let info = shown_to("feature").expect("shown");
        assert_eq!(
            info,
            BranchInfo {
                name: "feature".to_owned(),
                tip: on_moved.to_hex(),
                projected: shown.tip.to_hex(),
                commits: 1,
                made: 1,
                reused: 0,
            }
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), &in_ref(&checkout.id, "feature")).ok(),
            Some(shown.tip)
        );
        assert_eq!(log.grafted(&shown.tip).expect("lookup"), Some(on_moved));
        // Shown again, the projection is reused.
        assert_eq!(shown_to("feature").map(|i| (i.made, i.reused)), Ok((0, 1)));
        // Another spelling is another branch where case matters, and the
        // server's branch, under its own name, where it doesn't, whether the
        // ref is loose or packed.
        let ignore_case = |value: &str| {
            let config = repo.git_dir().join("config");
            let text = std::fs::read_to_string(&config).expect("config");
            let mut file =
                crate::config::ConfigFile::parse(&config, &text, crate::config::ConfigScope::Local)
                    .expect("parse");
            file.set("core.ignorecase", value).expect("set");
            file.write().expect("write");
            repo.reload_config().expect("reload");
        };
        ignore_case("false");
        assert_eq!(shown_to("FEATURE"), Err(ProjectError::NoSuchBranch));
        ignore_case("true");
        let named = |name: &str| shown_to(name).map(|i| (i.name, i.projected));
        let feature = Ok(("feature".to_owned(), shown.tip.to_hex()));
        assert_eq!(named("FEATURE"), feature);
        grit_test_support::git(dir.path(), &["pack-refs", "--all"]);
        assert!(
            !repo.git_dir().join("refs/heads/feature").exists(),
            "packed"
        );
        assert_eq!(named("Feature"), feature);
        assert_eq!(named("feature"), feature);
        assert_eq!(shown_to("nope"), Err(ProjectError::NoSuchBranch));
        for (refusal, kind) in [
            (ProjectError::InvalidName, "invalid_name"),
            (ProjectError::NoSuchBranch, "no_such_branch"),
            (ProjectError::OnTarget, "on_target"),
        ] {
            assert_eq!(refusal.kind(), kind);
            assert_eq!(serde_json::to_value(&refusal).expect("json")["kind"], kind);
        }
    }
}
