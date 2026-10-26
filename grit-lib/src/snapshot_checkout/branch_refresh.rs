//! Bringing a local branch onto the server's branch of its name, as the
//! checkout sees it: what `grit pull <branch>` does in a checkout once the
//! server has projected its branch ([`super::project_branch`]) and the
//! projection has been fetched ([`super::fetch_branch`]).
//!
//! A branch the checkout doesn't have is created at the projection. One that
//! already has it is left alone. Otherwise only the branch's own commits move
//! onto the projection: those above the newer of where it meets the
//! projection and the newest snapshot it builds on, without the copies of
//! the server's commits (rebased by a refresh onto a newer snapshot, say)
//! that it has, recognized by change id or by patch. A branch with nothing
//! else moves to the projection; one with commits of its own is rebased, in
//! one line, and a rebase that conflicts stops in a resumable rebase session
//! (`grit rebase --continue`, `--skip` or `--abort`), as a refresh's does.

use std::collections::HashSet;

use crate::change_id::ChangeId;
use crate::fetch::NoProgress;
use crate::merge_base::{
    count_symmetric_ahead_behind, is_ancestor, merge_bases_first_vs_rest,
    walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::objects::{parse_commit, ObjectId};
use crate::patch_ids::{compute_patch_id, PatchIdMode};
use crate::porcelain::checkout_reflog::checkout_reflog_identity;
use crate::porcelain::merge::MergeConflictDetail;
use crate::porcelain::pull::{pull, FetchedUpstream, PullMode, PullOptions, PullResult};
use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use crate::progress::NeverCancel;
use crate::remote::transfer::FetchOptions;
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

use super::ids::change_keys;
use super::refresh::ensure_refreshable;
use super::refs::{swap_ref_as, Swap};
use super::{
    give_change_ids, upstream_ref, valid_branch_name, BranchInfo, CheckoutError, CheckoutMarker,
    Result,
};

/// What [`refresh_branch`] did to the local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchRefreshResult {
    /// The checkout had no such branch; it is created at the projection
    /// (and not checked out).
    Created,
    /// The branch already has every commit of the projection.
    UpToDate,
    /// The branch moved to the projection: it had nothing above it but the
    /// server's commits (or copies of them). Uncommitted changes are kept
    /// when it is the current branch.
    FastForwarded,
    /// The branch's own commits were replayed onto the projection.
    Rebased {
        /// How many.
        own: usize,
    },
    /// The rebase stopped on a conflict and is in progress: the branch
    /// still points at its old tip until it is continued.
    Stopped {
        /// The conflicted paths.
        conflicts: Vec<MergeConflictDetail>,
    },
    /// The rebase would conflict, but the index or worktree has changes of
    /// their own, so it couldn't stop there: nothing changed.
    Conflicts {
        /// The conflicted paths.
        conflicts: Vec<MergeConflictDetail>,
    },
    /// The branch was rebased, but putting back the changes stashed for it
    /// (`rebase.autoStash`) conflicted; the stash entry was kept.
    AutostashConflict,
}

/// What [`refresh_branch`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRefresh {
    /// The branch, without `refs/heads/`.
    pub name: String,
    /// The local ref holding the projection, `refs/remotes/origin/<name>`.
    pub upstream_ref: String,
    /// The projection.
    pub projected: ObjectId,
    /// Where the local branch was; `None` when it was created.
    pub previous: Option<ObjectId>,
    /// Where it is now (still its old tip while a rebase is stopped, and
    /// with `check`).
    pub tip: ObjectId,
    /// What happened (with `check`, what would).
    pub result: BranchRefreshResult,
    /// Whether `HEAD` was moved onto the branch to rebase it, as
    /// `git rebase <upstream> <branch>` does.
    pub switched: bool,
    /// The branch `HEAD` was on before, when it was on one.
    pub was_on: Option<String>,
    /// How many of the branch's commits made with plain Git were given a
    /// change id first ([`give_change_ids`]).
    pub gave_change_ids: usize,
    /// Whether nothing was changed (`check`).
    pub check: bool,
    /// With `check`, the branch's commits the projection lacks and the
    /// projection's commits the branch lacks.
    pub ahead_behind: Option<(usize, usize)>,
}

/// Which of a branch's commits are its own, against the projection.
struct OwnCommits {
    /// Where the branch's own commits start: the newer of where it meets the
    /// projection and its newest snapshot, past the copies right above it.
    from: ObjectId,
    /// The copies above the first own commit, left out of the rebase.
    copies: Vec<ObjectId>,
    /// How many commits are the branch's own (merges, which a linear rebase
    /// drops, not counted).
    own: usize,
}

/// Bring the local branch `info.name` onto the server's branch as the
/// checkout sees it, `info.projected`, fetched already (see the module
/// docs).
///
/// First the branch's commits made with plain Git are given a change id
/// ([`give_change_ids`], not with `check`), logged at the repository
/// handle's clock ([`Repository::identity_now`]). The branch's own commits
/// are then those above `from`, the newer of its merge base with the
/// projection and the newest snapshot it builds on, past the copies of the
/// server's commits right above it; other copies among them (same change key
/// as one of the projection's commits above the snapshots, or same patch-id
/// as one of the projection's commits the branch lacks) are left out of the
/// rebase too.
///
/// When the branch is not the current one, it is moved by compare-and-swap
/// to the projection when it has nothing of its own, and otherwise `HEAD` is
/// switched to it first (as `git rebase <upstream> <branch>` does) and it is
/// rebased in place, with the worktree. When that rebase fails or refuses
/// (local changes it would overwrite, say), `HEAD` is switched back.
///
/// # Parameters
///
/// - `repo`: the checkout.
/// - `marker`: its marker.
/// - `info`: the server's answer to [`super::CheckoutClient::branch`], whose
///   projection must be at [`upstream_ref`]`(<name>)` here.
/// - `check`: say what would happen without changing anything.
///
/// # Errors
///
/// [`CheckoutError::OperationInProgress`] while a rebase or merge is in
/// progress; [`CheckoutError::BranchMoved`] when the projection is not at
/// [`upstream_ref`]`(<name>)`; [`CheckoutError::UnrelatedBranch`] for a local
/// branch that shares nothing with the projection;
/// [`CheckoutError::RefKeptMoving`] when the branch moved meanwhile;
/// [`CheckoutError::Library`] for a branch checked out (or being rebased or
/// bisected) in another worktree, or a switch that would overwrite local
/// changes; [`CheckoutError::Pull`] when the rebase fails;
/// [`CheckoutError::StillSwitched`] when it fails and `HEAD` can't be
/// switched back; and ref, walk and object failures.
pub fn refresh_branch(
    repo: &Repository,
    marker: &CheckoutMarker,
    info: &BranchInfo,
    check: bool,
) -> Result<BranchRefresh> {
    ensure_refreshable(repo)?;
    let name = info.name.as_str();
    if !valid_branch_name(name) {
        return Err(CheckoutError::InvalidBranchName(name.to_owned()));
    }
    let projected = ObjectId::from_hex(&info.projected).map_err(|e| CheckoutError::Protocol {
        url: marker.url.clone(),
        detail: format!(
            "the projection {:?} is not a commit id: {e}",
            info.projected
        ),
    })?;
    let fetched = crate::refs::resolve_ref(repo.git_dir(), &upstream_ref(name)).ok();
    if fetched != Some(projected) {
        return Err(CheckoutError::BranchMoved {
            name: name.to_owned(),
            expected: info.projected.clone(),
            got: fetched
                .map(|g| g.to_hex())
                .unwrap_or_else(|| "nothing".to_owned()),
        });
    }
    let refname = format!("refs/heads/{name}");
    let gave_change_ids = if check {
        0
    } else {
        give_change_ids(repo, marker, Some(&[name.to_owned()]), repo.identity_now())?
    };
    let head = resolve_head_for_repository(repo)?;
    let was_on = match &head {
        HeadState::Branch { short_name, .. } => Some(short_name.clone()),
        _ => None,
    };
    let current = was_on.as_deref() == Some(name);
    let done = |previous, tip, result| BranchRefresh {
        name: name.to_owned(),
        upstream_ref: upstream_ref(name),
        projected,
        previous,
        tip,
        result,
        switched: false,
        was_on: was_on.clone(),
        gave_change_ids,
        check,
        ahead_behind: None,
    };
    let message = format!("pull: move to {}", upstream_label(name));
    let Ok(tip) = crate::refs::resolve_ref(repo.git_dir(), &refname) else {
        if !check {
            let created = swap_ref_as(
                repo,
                &refname,
                None,
                projected,
                &checkout_reflog_identity(repo),
                &format!("branch: Created from {}", upstream_label(name)),
            )?;
            if created == Swap::Moved {
                return Err(CheckoutError::RefKeptMoving { refname });
            }
        }
        return Ok(BranchRefresh {
            ahead_behind: check.then_some((0, info.commits)),
            ..done(None, projected, BranchRefreshResult::Created)
        });
    };
    if tip == projected || is_ancestor(repo, projected, tip)? {
        return Ok(BranchRefresh {
            ahead_behind: check
                .then(|| count_symmetric_ahead_behind(repo, tip, projected))
                .transpose()?,
            ..done(Some(tip), tip, BranchRefreshResult::UpToDate)
        });
    }
    let own = own_commits(repo, marker, name, tip, projected)?;
    if own.own == 0 {
        if check {
            return Ok(BranchRefresh {
                ahead_behind: Some(count_symmetric_ahead_behind(repo, tip, projected)?),
                ..done(Some(tip), tip, BranchRefreshResult::FastForwarded)
            });
        }
        if !current {
            ensure_not_checked_out_elsewhere(repo, name)?;
            let moved = swap_ref_as(
                repo,
                &refname,
                Some(tip),
                projected,
                &checkout_reflog_identity(repo),
                &message,
            )?;
            if moved == Swap::Moved {
                return Err(CheckoutError::RefKeptMoving { refname });
            }
            return Ok(done(
                Some(tip),
                projected,
                BranchRefreshResult::FastForwarded,
            ));
        }
    }
    if check {
        return Ok(BranchRefresh {
            ahead_behind: Some(count_symmetric_ahead_behind(repo, tip, projected)?),
            ..done(
                Some(tip),
                tip,
                BranchRefreshResult::Rebased { own: own.own },
            )
        });
    }
    let mut switched = false;
    if !current {
        ensure_not_checked_out_elsewhere(repo, name)?;
        switch_repository(
            repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: name.to_owned(),
                    oid: tip,
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &NeverCancel,
        )?;
        switched = true;
    }
    let options = PullOptions {
        mode: Some(PullMode::Rebase),
        stop_on_conflict: true,
        upstream: Some(FetchedUpstream {
            label: upstream_label(name),
            oid: projected,
            landed: Some(own.from),
            copies: own.copies,
        }),
        fetch: FetchOptions {
            thread_policy: repo.thread_policy(),
            ..Default::default()
        },
        ..Default::default()
    };
    let pulled = pull(repo, &options, &mut NoProgress, &NeverCancel, None);
    // A rebase that failed or refused changed nothing, so HEAD goes back
    // where it was, as if the branch had never been switched to.
    let nothing_done = match &pulled {
        Err(_) => true,
        Ok(outcome) => matches!(
            outcome.result,
            PullResult::Conflicts | PullResult::RefusedNotFf
        ),
    };
    if switched && nothing_done && switch_back(repo, &head) {
        switched = false;
    }
    let pulled = match pulled {
        Ok(pulled) => pulled,
        Err(failed) if switched => {
            return Err(CheckoutError::StillSwitched {
                branch: name.to_owned(),
                failed: Box::new(failed.into()),
            })
        }
        Err(failed) => return Err(failed.into()),
    };
    let after = pulled.oid.unwrap_or(tip);
    // A rebase pull never merges, refuses or adopts an upstream; those are
    // reported as what they come closest to.
    let result = match pulled.result {
        PullResult::UpToDate => BranchRefreshResult::UpToDate,
        PullResult::FastForward | PullResult::Landed | PullResult::SetUpstream => {
            BranchRefreshResult::FastForwarded
        }
        PullResult::Rebased | PullResult::Merged => BranchRefreshResult::Rebased { own: own.own },
        PullResult::RebaseStopped | PullResult::RebaseConflicts => BranchRefreshResult::Stopped {
            conflicts: pulled.conflicts.unwrap_or_default(),
        },
        PullResult::Conflicts | PullResult::RefusedNotFf => BranchRefreshResult::Conflicts {
            conflicts: pulled.conflicts.unwrap_or_default(),
        },
        PullResult::AutostashConflict => BranchRefreshResult::AutostashConflict,
    };
    Ok(BranchRefresh {
        switched,
        ..done(Some(tip), after, result)
    })
}

/// Switch `HEAD` back to `head`, the branch or commit it was at before
/// [`refresh_branch`] switched to the branch to rebase. Returns whether it
/// is back: an unborn or unreadable `HEAD` can't be gone back to, and a
/// switch that fails leaves `HEAD` where it is.
fn switch_back(repo: &Repository, head: &HeadState) -> bool {
    let mode = match head {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => SwitchMode::Existing {
            branch: short_name.clone(),
            oid: *oid,
        },
        HeadState::Detached { oid } => SwitchMode::Detached {
            commit: *oid,
            reflog_to_label: oid.to_hex(),
        },
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => return false,
    };
    let options = SwitchOptions {
        mode,
        dry_run: false,
        hooks: crate::hooks::PostOperationHooks::Run,
    };
    switch_repository(
        repo,
        &options,
        &mut crate::progress::NullProgress,
        &NeverCancel,
    )
    .is_ok()
}

/// `origin/<name>`, what the projection is called in messages.
fn upstream_label(name: &str) -> String {
    format!("origin/{name}")
}

/// Refuse to move `name` when another worktree has it checked out.
fn ensure_not_checked_out_elsewhere(repo: &Repository, name: &str) -> Result<()> {
    Ok(crate::worktree::ensure_branch_not_checked_out_in_any_worktree(repo, name)?)
}

/// Which of the commits of `tip` are the branch's own against `projected`
/// (see [`refresh_branch`]).
fn own_commits(
    repo: &Repository,
    marker: &CheckoutMarker,
    name: &str,
    tip: ObjectId,
    projected: ObjectId,
) -> Result<OwnCommits> {
    let roots: Vec<ObjectId> = marker
        .roots
        .iter()
        .filter_map(|r| ObjectId::from_hex(&r.commit).ok())
        .collect();
    let met = merge_bases_first_vs_rest(repo, tip, &[projected])?
        .into_iter()
        .next();
    let mut snapshot = None;
    for root in roots.iter().rev() {
        if is_ancestor(repo, *root, tip)? {
            snapshot = Some(*root);
            break;
        }
    }
    let mut from = match (met, snapshot) {
        (Some(m), Some(s)) if is_ancestor(repo, s, m)? => m,
        (_, Some(s)) => s,
        (Some(m), None) => m,
        (None, None) => {
            return Err(CheckoutError::UnrelatedBranch {
                name: name.to_owned(),
            })
        }
    };
    // The server's commits above the snapshots, by change key, and by patch
    // those the branch doesn't have.
    let keys: HashSet<ChangeId> = change_keys(repo, &[projected], &roots)?
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    let odb = repo.odb();
    let algo = odb.hash_algo();
    let (_, theirs) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        &[projected],
        &[tip],
        ReachableWalkLimit::Unlimited,
    )?;
    let mut patches = HashSet::new();
    for commit in theirs {
        if let Some(patch) = compute_patch_id(algo, odb, &commit, PatchIdMode::Stable)? {
            patches.insert(patch);
        }
    }
    let mut copies = Vec::new();
    let mut own = 0;
    let mut leading = true;
    for (commit, key) in change_keys(repo, &[tip], &[from])? {
        let data = parse_commit(&odb.read(&commit)?.data)?;
        let merge = data.parents.len() > 1;
        let copy = !merge
            && (keys.contains(&key)
                || compute_patch_id(algo, odb, &commit, PatchIdMode::Stable)?
                    .is_some_and(|patch| patches.contains(&patch)));
        // Copies right on top of `from`, in a line, are where the branch's
        // own commits start.
        if leading && copy && data.parents.first() == Some(&from) {
            from = commit;
            continue;
        }
        leading = false;
        if copy {
            copies.push(commit);
        } else if !merge {
            own += 1;
        }
    }
    Ok(OwnCommits { from, copies, own })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{serialize_commit, ObjectKind};
    use crate::snapshot_checkout::testing::*;
    use crate::snapshot_checkout::{
        branches_with_work, fetch_branch, project_branch, refresh, CheckoutStore,
    };
    use std::path::Path;

    /// The server shows its branch `name` to the checkout, which fetches it.
    fn fetched(served: &Served, repo: &Repository, name: &str) -> BranchInfo {
        let server = open(&served.dir);
        let log = CheckoutStore::for_repository(&server).graft_log();
        let info = project_branch(&server, &served.info(), &log, name)
            .expect("project")
            .expect("shown");
        fetch_branch(
            repo,
            &marker(repo),
            &info,
            FetchOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch");
        info
    }

    fn pulled(repo: &Repository, info: &BranchInfo) -> BranchRefresh {
        refresh_branch(repo, &marker(repo), info, false).expect("refresh branch")
    }

    fn files(co: &Path, rev: &str) -> String {
        git(co, &["ls-tree", "-r", "--name-only", rev])
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_server_branch_is_created_then_followed_then_rebased_under_own_commits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        // A teammate with a full clone starts feature, inside and outside src.
        served.commit_on("feature", "src/a.rs", "a by bob\n", "Bob changes a");
        served.commit_on(
            "feature",
            "docs/d.md",
            "docs by bob\n",
            "Bob changes the docs",
        );

        let info = fetched(&served, &repo, "feature");
        assert_eq!(info.commits, 2);
        let created = pulled(&repo, &info);
        assert_eq!(created.result, BranchRefreshResult::Created);
        assert_eq!(created.previous, None);
        assert!(!created.switched);
        assert_eq!(created.upstream_ref, "refs/remotes/origin/feature");
        assert_eq!(git(&co, &["rev-parse", "feature"]), info.projected);
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "work");
        assert!(
            !files(&co, "feature").contains("docs/"),
            "outside the paths"
        );
        assert_eq!(
            git(&co, &["log", "-1", "--format=%s", "feature"]),
            "Bob changes the docs"
        );
        assert!(
            branches_with_work(&repo, &marker(&repo))
                .expect("branches")
                .iter()
                .any(|(name, _)| name == "feature"),
            "a branch on projected commits is one to check in"
        );
        assert_eq!(pulled(&repo, &info).result, BranchRefreshResult::UpToDate);

        // More on the server's: a branch with nothing of its own moves.
        served.commit_on("feature", "src/b.rs", "b by bob\n", "Bob changes b");
        let info = fetched(&served, &repo, "feature");
        let moved = pulled(&repo, &info);
        assert_eq!(moved.result, BranchRefreshResult::FastForwarded);
        assert!(!moved.switched);
        assert_eq!(git(&co, &["rev-parse", "feature"]), info.projected);

        // A commit of our own, and more on the server's: ours goes on top.
        git(&co, &["switch", "-q", "feature"]);
        commit(&co, "src/c.rs", "c\n", "Add c");
        let up = pulled(&repo, &info);
        assert_eq!(up.result, BranchRefreshResult::UpToDate);
        assert_eq!(up.gave_change_ids, 1, "the commit made with git got an id");
        let ours = git(&co, &["rev-parse", "feature"]);
        served.commit_on("feature", "src/a.rs", "a by bob, again\n", "Bob again");
        let info = fetched(&served, &repo, "feature");
        let would = refresh_branch(&repo, &marker(&repo), &info, true).expect("check");
        assert_eq!(would.result, BranchRefreshResult::Rebased { own: 1 });
        assert!(would.check);
        assert_eq!(would.gave_change_ids, 0);
        assert_eq!(
            git(&co, &["rev-parse", "feature"]),
            ours,
            "a check changes nothing"
        );
        let rebased = pulled(&repo, &info);
        assert_eq!(rebased.gave_change_ids, 0);
        assert_eq!(rebased.result, BranchRefreshResult::Rebased { own: 1 });
        assert!(!rebased.switched);
        assert_eq!(git(&co, &["rev-parse", "feature~1"]), info.projected);
        assert_eq!(git(&co, &["log", "-1", "--format=%s", "feature"]), "Add c");
        assert_eq!(git(&co, &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(co.join("src/a.rs")).expect("read"),
            "a by bob, again\n"
        );
        assert!(!co.join("docs").exists());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn copies_of_the_servers_commits_are_left_out_by_change_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        served.commit_on("feature", "src/a.rs", "a by bob\n", "Bob changes a");
        let info = fetched(&served, &repo, "feature");
        pulled(&repo, &info);
        git(&co, &["switch", "-q", "feature"]);
        commit(&co, "src/c.rs", "c\n", "Add c");

        // The target moves; a refresh rebases feature onto the new snapshot,
        // so it holds a copy of Bob's commit, with its change id.
        let newer = served.moves("src/b.rs", "b2\n");
        let options = crate::porcelain::pull::PullOptions::default();
        refresh(
            &repo,
            &marker(&repo),
            &newer,
            &options,
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("refresh");
        assert_eq!(git(&co, &["rev-parse", "feature~2"]), newer.root);

        // Bob adds another commit; we have a copy of it with other content
        // (same change id), above our own.
        served.commit_on("feature", "src/a.rs", "a by bob, again\n", "Bob again");
        let info = fetched(&served, &repo, "feature");
        let theirs = oid(&info.projected);
        let key = crate::change_id::resolved_change_id(
            &theirs,
            &parse_commit(&repo.odb().read(&theirs).expect("read").data).expect("parse"),
        )
        .id;
        let tip = oid(&git(&co, &["rev-parse", "feature"]));
        let mut copy = parse_commit(&repo.odb().read(&tip).expect("read").data).expect("parse");
        copy.parents = vec![tip];
        copy.message = "My copy of Bob again\n".to_owned();
        crate::change_id::set_change_id(&mut copy, &key);
        let copy = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&copy))
            .expect("copy");
        git(&co, &["update-ref", "refs/heads/feature", &copy.to_hex()]);

        let rebased = pulled(&repo, &info);
        assert_eq!(rebased.result, BranchRefreshResult::Rebased { own: 1 });
        assert_eq!(git(&co, &["rev-parse", "feature~1"]), info.projected);
        // The copies by change id are gone (the refresh's copy of Bob's first
        // commit, and ours of his second); our own commit stays.
        let log = git(&co, &["log", "--format=%s", "feature"]);
        let subjects: Vec<&str> = log.lines().collect();
        assert_eq!(subjects.len(), 4, "{log}");
        assert_eq!(subjects[..3], ["Add c", "Bob again", "Bob changes a"]);
        assert!(subjects[3].starts_with("Snapshot of"), "{log}");
        assert_eq!(
            git(&co, &["rev-parse", "feature~3"]),
            served.roots[0].commit
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_conflicting_branch_refresh_stops_in_a_rebase() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        served.commit_on("feature", "src/b.rs", "b by bob\n", "Bob changes b");
        let info = fetched(&served, &repo, "feature");
        pulled(&repo, &info);
        git(&co, &["switch", "-q", "feature"]);
        commit(&co, "src/a.rs", "mine\n", "Mine");
        served.commit_on("feature", "src/a.rs", "theirs\n", "Theirs");
        let info = fetched(&served, &repo, "feature");

        let stopped = pulled(&repo, &info);
        let BranchRefreshResult::Stopped { conflicts } = &stopped.result else {
            panic!("stopped: {stopped:?}");
        };
        assert_eq!(
            conflicts
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            ["src/a.rs"]
        );
        assert!(co.join(".git/rebase-merge/grit-plan").is_file());
        assert!(matches!(
            refresh_branch(&repo, &marker(&repo), &info, false),
            Err(CheckoutError::OperationInProgress(_))
        ));
        assert_eq!(stopped.tip, stopped.previous.expect("previous"));
        let identity = "T <t@example.com> 0 +0000".parse().expect("signature");
        crate::revwalk::history::abort_rebase(&repo, &identity).expect("abort");
        assert_eq!(git(&co, &["rev-parse", "feature"]), stopped.tip.to_hex());
        assert_eq!(git(&co, &["log", "-1", "--format=%s", "feature"]), "Mine");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_branch_being_rebased_in_another_worktree_is_left_to_that_rebase() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        served.commit_on("feature", "src/b.rs", "b by bob\n", "Bob changes b");
        let info = fetched(&served, &repo, "feature");
        pulled(&repo, &info);
        git(&co, &["switch", "-q", "-c", "other", "feature"]);
        commit(&co, "src/a.rs", "other\n", "Other");
        git(&co, &["switch", "-q", "work"]);
        let wt = tmp.path().join("wt");
        git(
            &co,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().expect("path"),
                "feature",
            ],
        );
        commit(&wt, "src/a.rs", "mine\n", "Mine");
        let ours = git(&co, &["rev-parse", "feature"]);
        assert!(
            git_output_fails(
                &wt,
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@e",
                    "rebase",
                    "other"
                ]
            ),
            "the rebase stops on a conflict"
        );
        assert!(co
            .join(".git/worktrees/wt/rebase-merge/head-name")
            .is_file());
        served.commit_on("feature", "src/d.rs", "d by bob\n", "Bob adds d");
        let info = fetched(&served, &repo, "feature");

        let refused = refresh_branch(&repo, &marker(&repo), &info, false);
        assert!(
            matches!(
                &refused,
                Err(CheckoutError::Library(e))
                    if matches!(**e, crate::error::Error::BranchCheckedOutInWorktree { .. })
            ),
            "{refused:?}"
        );
        assert_eq!(git(&co, &["rev-parse", "feature"]), ours);
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "work");
        // So the rebase there still finishes.
        std::fs::write(wt.join("src/a.rs"), "mine\n").expect("write");
        git(&wt, &["add", "src/a.rs"]);
        git(&wt, &["-c", "core.editor=true", "rebase", "--continue"]);
        assert_eq!(git(&wt, &["symbolic-ref", "--short", "HEAD"]), "feature");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_branch_that_is_not_checked_out_is_switched_to_for_its_rebase() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        served.commit_on("feature", "src/a.rs", "a by bob\n", "Bob changes a");
        let info = fetched(&served, &repo, "feature");
        pulled(&repo, &info);
        git(&co, &["switch", "-q", "feature"]);
        commit(&co, "src/c.rs", "c\n", "Add c");
        git(&co, &["switch", "-q", "work"]);
        served.commit_on("feature", "src/b.rs", "b by bob\n", "Bob changes b");
        let info = fetched(&served, &repo, "feature");

        let rebased = pulled(&repo, &info);
        assert_eq!(rebased.result, BranchRefreshResult::Rebased { own: 1 });
        assert!(rebased.switched);
        assert_eq!(rebased.was_on.as_deref(), Some("work"));
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert_eq!(git(&co, &["rev-parse", "feature~1"]), info.projected);

        // A local branch that shares nothing with the server's is refused.
        let lone = git(&co, &["commit-tree", "-m", "lone", "HEAD^{tree}"]);
        git(&co, &["branch", "lone", &lone]);
        served.commit_on("lone", "src/l.rs", "l\n", "Server lone");
        let info = fetched(&served, &repo, "lone");
        assert!(matches!(
            refresh_branch(&repo, &marker(&repo), &info, false),
            Err(CheckoutError::UnrelatedBranch { .. })
        ));
        // Nor is a projection that isn't here.
        let mut stale = info.clone();
        stale.projected = "1".repeat(40);
        assert!(matches!(
            refresh_branch(&repo, &marker(&repo), &stale, false),
            Err(CheckoutError::BranchMoved { .. })
        ));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_rebase_that_refuses_after_the_switch_switches_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        served.commit_on("feature", "src/b.rs", "b by bob\n", "Bob changes b");
        let info = fetched(&served, &repo, "feature");
        pulled(&repo, &info);
        let before = info.projected.clone();
        git(&co, &["switch", "-q", "feature"]);
        commit(&co, "src/c.rs", "c\n", "Add c");
        git(&co, &["switch", "-q", "work"]);
        // A change of ours the switch keeps, which the rebase would overwrite.
        std::fs::write(co.join("src/a.rs"), "dirty\n").expect("write");
        served.commit_on("feature", "src/a.rs", "a by bob\n", "Bob changes a");
        let info = fetched(&served, &repo, "feature");

        let refused = refresh_branch(&repo, &marker(&repo), &info, false);
        assert!(
            matches!(refused, Err(CheckoutError::Pull(_))),
            "{refused:?}"
        );
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "work");
        assert_eq!(git(&co, &["rev-parse", "feature~1"]), before, "not rebased");
        assert_eq!(git(&co, &["log", "-1", "--format=%s", "feature"]), "Add c");
        assert_eq!(
            std::fs::read_to_string(co.join("src/a.rs")).expect("read"),
            "dirty\n"
        );
        assert_eq!(git(&co, &["status", "--porcelain"]), "M src/a.rs");
    }
}
