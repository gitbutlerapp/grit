//! Refreshing a snapshot checkout: bring it onto the server's newest snapshot.
//!
//! The server first makes the snapshot ([`super::CheckoutClient::update`]).
//! [`refresh`] gives the current branch's commits made with plain Git a
//! change id ([`give_change_ids`]), fetches the snapshot into
//! [`upstream_ref`] ([`fetch_snapshot`], which records it in the marker only
//! once it has arrived), brings the current branch onto it the way [`pull`]
//! does (a rebase by default, which
//! stops in a resumable rebase session on a conflict), moves a local branch
//! named after the target that has no commits of its own along with it, and
//! reports every other branch still building on an older snapshot. Commits
//! that landed in the target ([`super::record_landed`]) are never replayed.

use crate::diff::{build_file_diffs, diff_trees, DiffDetail, DiffOptions, DiffTotals};
use crate::fetch::Progress;
use crate::merge_base::is_ancestor;
use crate::objects::ObjectId;
use crate::porcelain::checkout_reflog::checkout_reflog_identity;
use crate::porcelain::pull::{
    pull, FetchedUpstream, PullError, PullMode, PullOptions, PullOutcome,
};
use crate::progress::Cancel;
use crate::remote::HttpClientFactory;
use crate::repo::Repository;
use crate::state::{detect_in_progress, resolve_head_for_repository, HeadState};
use crate::transfer::TransferStats;

use super::landing::{landed_base, prune_landed};
use super::refs::{swap_ref_as, Swap};
use super::{
    branches_with_work, fetch_snapshot, give_change_ids, upstream_ref, CheckoutError, CheckoutInfo,
    CheckoutMarker, Result, SnapshotRoot,
};

/// What a refresh did with `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshedHead {
    /// `HEAD` is detached and was left alone: only [`upstream_ref`] moved, to
    /// the new snapshot.
    Detached,
    /// The current branch was brought onto the new snapshot, as [`pull`]
    /// brings a branch onto its upstream.
    Branch {
        /// The branch, without `refs/heads/`.
        name: String,
        /// What [`pull`] did to it; [`crate::porcelain::pull::PullResult::RebaseStopped`]
        /// when its rebase stopped on a conflict.
        pull: Box<PullOutcome>,
    },
}

/// A local branch with no commits of its own that a refresh moved to the new
/// snapshot: the one named after the target, or one whose commits all landed
/// in the target ([`CheckoutMarker::landed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowedBranch {
    /// The branch, without `refs/heads/`.
    pub name: String,
    /// The older snapshot it was at.
    pub from: ObjectId,
    /// The new snapshot it is at now.
    pub to: ObjectId,
}

/// A local branch with work of its own that still builds on an older
/// snapshot after a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchOnOldSnapshot {
    /// The branch, without `refs/heads/`.
    pub name: String,
    /// Its tip.
    pub tip: ObjectId,
}

/// What [`refresh`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// The server's target branch the checkout follows.
    pub target: String,
    /// The local ref holding the newest snapshot ([`upstream_ref`]).
    pub upstream_ref: String,
    /// The checkout's newest snapshot before the refresh.
    pub previous: Option<SnapshotRoot>,
    /// Its newest snapshot now.
    pub current: SnapshotRoot,
    /// Whether the target moved since the previous snapshot (a new snapshot
    /// came down).
    pub moved: bool,
    /// What changed in the checkout's paths from the previous snapshot to the
    /// new one; `None` when the target did not move.
    pub changes: Option<DiffTotals>,
    /// What the snapshot's fetch received.
    pub transfer: TransferStats,
    /// What happened to `HEAD`.
    pub head: RefreshedHead,
    /// Local branches with no commits of their own that followed the
    /// snapshot.
    pub followed: Vec<FollowedBranch>,
    /// Other branches with work still on an older snapshot, which only a
    /// refresh (or rebase) on each of them moves.
    pub behind: Vec<BranchOnOldSnapshot>,
    /// How many of the current branch's commits made with plain Git were
    /// given a change id first ([`give_change_ids`]).
    pub gave_change_ids: usize,
}

/// Refuse to refresh a checkout that is in the middle of an operation, such
/// as a refresh's rebase that stopped on a conflict: that has to be
/// continued or aborted first. Check this before asking the server for a new
/// snapshot.
///
/// # Errors
///
/// [`CheckoutError::OperationInProgress`] naming the operation.
pub fn ensure_refreshable(repo: &Repository) -> Result<()> {
    match detect_in_progress(repo.git_dir()).into_iter().next() {
        Some(operation) => Err(CheckoutError::OperationInProgress(operation)),
        None => Ok(()),
    }
}

/// Bring the checkout onto `info`, the snapshot the server answered
/// [`super::CheckoutClient::update`] with.
///
/// 0. With `HEAD` on a branch, gives that branch's commits made with plain
///    Git a change id ([`give_change_ids`]; not with `options.check`), so the
///    rebase below keeps a key the server can match them by. Those moves are
///    logged at the time of the repository handle's clock
///    ([`Repository::identity_now`]), as the rebase's commits and reflog
///    entries are.
/// 1. Fetches the snapshot from the marker's URL into [`upstream_ref`] and
///    records it in the marker ([`fetch_snapshot`]).
/// 2. With `HEAD` on a branch, integrates the snapshot as [`pull`] does with
///    `options` (a rebase unless `options.mode` says otherwise). A rebase that
///    conflicts stops in a resumable rebase session
///    ([`PullOptions::stop_on_conflict`]). Commits up to a tip that landed in
///    the target ([`CheckoutMarker::landed`]) are not replayed: a branch with
///    nothing above one moves to the snapshot keeping uncommitted changes
///    ([`crate::porcelain::pull::PullResult::Landed`]), and one with commits
///    above it rebases only those ([`FetchedUpstream::landed`]). With `HEAD`
///    detached, nothing else moves.
/// 3. Moves `refs/heads/<target>`, when it is not the current branch and has
///    no commits of its own, to the new snapshot, and every other branch but
///    the current one whose commits all landed (not with `options.check`).
/// 4. Lists the other branches with work that still build on an older
///    snapshot, and forgets landed tips no branch builds on any more (not
///    with `options.check`).
///
/// # Parameters
///
/// - `repo`: the checkout.
/// - `marker`: its marker as it is now.
/// - `info`: the checkout as the server has it after the update.
/// - `options`: how to integrate (mode, autostash, check) and the fetch's
///   environment and threads; its remote, branch, fetch refspecs and
///   upstream are replaced.
/// - `progress`, `cancel`, `http_factory`: as for [`pull`].
///
/// # Errors
///
/// [`CheckoutError::OperationInProgress`] before anything is fetched, the
/// errors of [`fetch_snapshot`], and [`CheckoutError::Pull`] when the branch
/// could not be integrated (the new snapshot is fetched and recorded by then).
pub fn refresh<P: Progress>(
    repo: &Repository,
    marker: &CheckoutMarker,
    info: &CheckoutInfo,
    options: &PullOptions,
    progress: &mut P,
    cancel: &dyn Cancel,
    http_factory: Option<&dyn HttpClientFactory>,
) -> Result<RefreshOutcome> {
    ensure_refreshable(repo)?;
    let gave_change_ids = match resolve_head_for_repository(repo)? {
        HeadState::Branch { short_name, .. } if !options.check => give_change_ids(
            repo,
            marker,
            Some(std::slice::from_ref(&short_name)),
            repo.identity_now(),
        )?,
        _ => 0,
    };
    let (current_branch, head_tip) = match resolve_head_for_repository(repo)? {
        HeadState::Branch {
            short_name, oid, ..
        } => (Some(short_name), oid),
        HeadState::Detached { oid } => (None, Some(oid)),
        HeadState::Invalid => return Err(PullError::InvalidHead.into()),
    };
    let fetched = fetch_snapshot(
        repo,
        marker,
        info,
        options.fetch.clone(),
        progress,
        cancel,
        http_factory,
    )?;
    let root =
        ObjectId::from_hex(&fetched.current.commit).map_err(|e| CheckoutError::Protocol {
            url: marker.url.clone(),
            detail: e.to_string(),
        })?;
    let moved = fetched.moved();
    let changes = match &fetched.previous {
        Some(previous) if moved => snapshot_changes(repo, &previous.commit, root)?,
        _ => None,
    };
    let head = match &current_branch {
        None => RefreshedHead::Detached,
        Some(name) => {
            let mut integrate = options.clone();
            integrate.remote = None;
            integrate.branch = None;
            integrate.explicit_fetch = false;
            let landed = match head_tip {
                Some(tip) => landed_base(repo, &fetched.marker, tip, root)?,
                None => None,
            };
            integrate.upstream = Some(FetchedUpstream {
                label: format!("origin/{}", info.branch),
                oid: root,
                landed,
                copies: Vec::new(),
            });
            integrate.stop_on_conflict = true;
            if integrate.mode.is_none() {
                integrate.mode = Some(PullMode::Rebase);
            }
            RefreshedHead::Branch {
                name: name.clone(),
                pull: Box::new(pull(repo, &integrate, progress, cancel, http_factory)?),
            }
        }
    };
    let followed = if options.check {
        Vec::new()
    } else {
        let mut followed =
            follow_target_branch(repo, &fetched.marker, current_branch.as_deref(), root)?;
        followed.extend(follow_landed_branches(
            repo,
            &fetched.marker,
            current_branch.as_deref(),
            root,
        )?);
        prune_landed(repo, root)?;
        followed
    };
    let behind = branches_on_old_snapshots(repo, &fetched.marker, current_branch.as_deref(), root)?;
    Ok(RefreshOutcome {
        target: info.branch.clone(),
        upstream_ref: upstream_ref(&info.branch),
        previous: fetched.previous,
        current: fetched.current,
        moved,
        changes,
        transfer: fetched.transfer,
        head,
        followed,
        behind,
        gave_change_ids,
    })
}

/// The snapshot commits `marker` lists.
fn roots(marker: &CheckoutMarker) -> Vec<ObjectId> {
    marker
        .roots
        .iter()
        .filter_map(|r| ObjectId::from_hex(&r.commit).ok())
        .collect()
}

/// Move `refs/heads/<target>` to `root` when it is not `current` and every
/// commit on it is a snapshot (it has no commits of its own).
fn follow_target_branch(
    repo: &Repository,
    marker: &CheckoutMarker,
    current: Option<&str>,
    root: ObjectId,
) -> Result<Vec<FollowedBranch>> {
    if current == Some(marker.branch.as_str()) {
        return Ok(Vec::new());
    }
    let refname = format!("refs/heads/{}", marker.branch);
    let Ok(tip) = crate::refs::resolve_ref(repo.git_dir(), &refname) else {
        return Ok(Vec::new());
    };
    if tip == root {
        return Ok(Vec::new());
    }
    let mut own_commits = true;
    for snapshot in roots(marker) {
        if is_ancestor(repo, tip, snapshot)? {
            own_commits = false;
            break;
        }
    }
    if own_commits {
        return Ok(Vec::new());
    }
    let message = format!("refresh: follow {}", upstream_ref(&marker.branch));
    match swap_ref_as(
        repo,
        &refname,
        Some(tip),
        root,
        &checkout_reflog_identity(repo),
        &message,
    )? {
        Swap::Done => Ok(vec![FollowedBranch {
            name: marker.branch.clone(),
            from: tip,
            to: root,
        }]),
        Swap::Moved => Ok(Vec::new()),
    }
}

/// Move every branch other than `current` whose commits all landed in the
/// target (its tip is in a landed tip's history) to `root`, which has them.
fn follow_landed_branches(
    repo: &Repository,
    marker: &CheckoutMarker,
    current: Option<&str>,
    root: ObjectId,
) -> Result<Vec<FollowedBranch>> {
    let mut followed = Vec::new();
    if marker.landed.is_empty() {
        return Ok(followed);
    }
    let message = format!(
        "refresh: follow {}, which has the branch's commits",
        upstream_ref(&marker.branch)
    );
    for (name, tip) in branches_with_work(repo, marker)? {
        if Some(name.as_str()) == current || landed_base(repo, marker, tip, root)? != Some(tip) {
            continue;
        }
        // A branch another worktree has checked out moves only with that worktree.
        match crate::worktree::ensure_branch_not_checked_out_in_any_worktree(repo, &name) {
            Ok(()) => {}
            Err(crate::error::Error::BranchCheckedOutInWorktree { .. }) => continue,
            Err(e) => return Err(e.into()),
        }
        let refname = format!("refs/heads/{name}");
        let swapped = swap_ref_as(
            repo,
            &refname,
            Some(tip),
            root,
            &checkout_reflog_identity(repo),
            &message,
        )?;
        if swapped == Swap::Done {
            followed.push(FollowedBranch {
                name,
                from: tip,
                to: root,
            });
        }
    }
    Ok(followed)
}

/// Branches with work, other than `current`, whose tips don't have `root`.
fn branches_on_old_snapshots(
    repo: &Repository,
    marker: &CheckoutMarker,
    current: Option<&str>,
    root: ObjectId,
) -> Result<Vec<BranchOnOldSnapshot>> {
    let mut behind = Vec::new();
    for (name, tip) in branches_with_work(repo, marker)? {
        if Some(name.as_str()) == current || is_ancestor(repo, root, tip)? {
            continue;
        }
        behind.push(BranchOnOldSnapshot { name, tip });
    }
    Ok(behind)
}

/// What changed between the snapshots `previous` and `current`, as
/// `git diff --shortstat` counts it; `None` when `previous` is not here.
fn snapshot_changes(
    repo: &Repository,
    previous: &str,
    current: ObjectId,
) -> Result<Option<DiffTotals>> {
    let Ok(previous) = ObjectId::from_hex(previous) else {
        return Ok(None);
    };
    if !repo.odb().exists(&previous) {
        return Ok(None);
    }
    let odb = repo.odb();
    let old_tree = odb.commit_tree(&previous)?;
    let new_tree = odb.commit_tree(&current)?;
    let entries = diff_trees(odb, Some(&old_tree), Some(&new_tree), "")?;
    let config = repo.config()?;
    let files = build_file_diffs(
        odb,
        entries,
        DiffDetail::Stat,
        DiffOptions::default(),
        &config,
        &repo.thread_policy(),
    )?;
    Ok(Some(files.totals.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::porcelain::pull::PullResult;
    use crate::progress::NeverCancel;
    use crate::remote::fetch::NoProgress;
    use crate::snapshot_checkout::testing::*;
    use crate::snapshot_checkout::{
        land_integration, move_off_landed_branch, push_for_checkin, record_landed, target_ref,
        CheckoutClient, IntegrateResponse, IntegrateStatus, Landing, MovedOff,
    };
    use crate::state::InProgressOperation;

    fn refreshed(repo: &Repository, info: &CheckoutInfo) -> Result<RefreshOutcome> {
        refresh(
            repo,
            &marker(repo),
            info,
            &PullOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
    }

    fn pull_result(outcome: &RefreshOutcome) -> Option<PullResult> {
        match &outcome.head {
            RefreshedHead::Branch { pull, .. } => Some(pull.result),
            RefreshedHead::Detached => None,
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn plain_fetches_and_pushes_in_a_checkout_only_see_snapshots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        let snapshot = served.roots[0].commit.clone();
        assert_eq!(
            git(&co, &["config", "--get-all", "remote.origin.fetch"]),
            format!("+refs/checkouts/{ID}/target:refs/remotes/origin/main")
        );
        assert_eq!(
            git(&co, &["config", "remote.origin.push"]),
            format!("+refs/heads/*:refs/checkouts/{ID}/out/heads/*")
        );
        assert_eq!(git(&co, &["config", "remote.origin.tagOpt"]), "--no-tags");
        assert_eq!(
            git(&co, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
            "refs/remotes/origin/main"
        );

        // Git's own fetch brings neither the real history, other branches nor tags.
        commit(&served.dir, "docs/d.md", "d2\n", "more docs");
        git(&served.dir, &["branch", "other"]);
        git(&served.dir, &["tag", "v1"]);
        git(&co, &["fetch", "-q"]);
        assert_eq!(git(&co, &["rev-parse", "origin/main"]), snapshot);
        assert_eq!(
            git(&co, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/work\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main"
        );

        // Git's own push lands in the check-in area, never on a real branch.
        commit(&co, "src/a.rs", "a2\n", "work on a");
        git(&co, &["push", "-q"]);
        let pushed = format!("refs/checkouts/{ID}/out/heads/work");
        assert_eq!(
            git(&served.dir, &["rev-parse", &pushed]),
            git(&co, &["rev-parse", "work"])
        );
        assert_eq!(
            git(
                &served.dir,
                &["for-each-ref", "--format=%(refname)", "refs/heads"]
            ),
            "refs/heads/main\nrefs/heads/other"
        );

        // A check-in's push goes to the checkout's server whatever `origin` became.
        git(&co, &["remote", "rename", "origin", "server"]);
        commit(&co, "src/b.rs", "b2\n", "work on b");
        let tip = oid(&git(&co, &["rev-parse", "work"]));
        let outcome = push_for_checkin(
            &repo,
            &marker(&repo),
            &[("work".to_owned(), tip)],
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("push");
        assert!(outcome.results.iter().all(|r| !r.status.is_error()));
        assert_eq!(git(&served.dir, &["rev-parse", &pushed]), tip.to_hex());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn the_marker_is_shared_by_every_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        let linked = tmp.path().join("linked");
        git(
            &co,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().expect("utf-8 path"),
                "-b",
                "feature",
            ],
        );
        let linked_repo = open(&linked);
        assert_ne!(linked_repo.git_dir(), repo.git_dir());
        assert_eq!(
            CheckoutMarker::read(linked_repo.git_dir()),
            Some(marker(&repo))
        );

        let mut changed = marker(&repo);
        changed.roots.clear();
        changed.write(linked_repo.git_dir()).expect("write");
        assert!(!linked_repo
            .git_dir()
            .join(super::super::MARKER_FILE)
            .exists());
        assert_eq!(marker(&repo).roots, Vec::new());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_refresh_rebases_the_branch_follows_the_target_and_names_the_rest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        let first = served.roots[0].clone();
        git(&co, &["branch", "main", "origin/main"]);
        git(&co, &["switch", "-q", "-c", "side"]);
        let side = commit(&co, "src/s.rs", "s\n", "side work");
        git(&co, &["switch", "-q", "work"]);
        commit(&co, "src/w.rs", "w\n", "work");

        let info = served.moves("src/b.rs", "b2\n");
        let outcome = refreshed(&repo, &info).expect("refresh");
        let root = oid(&info.root);
        assert!(outcome.moved);
        assert_eq!(outcome.previous, Some(first.clone()));
        assert_eq!(outcome.current.commit, info.root);
        assert_eq!(outcome.upstream_ref, "refs/remotes/origin/main");
        assert_eq!(
            outcome.changes,
            Some(DiffTotals {
                files_changed: 1,
                insertions: 1,
                deletions: 1,
            })
        );
        assert_eq!(pull_result(&outcome), Some(PullResult::Rebased));
        assert_eq!(git(&co, &["rev-parse", "work^"]), info.root);
        assert_eq!(
            outcome.followed,
            [FollowedBranch {
                name: "main".to_owned(),
                from: oid(&first.commit),
                to: root,
            }]
        );
        assert_eq!(git(&co, &["rev-parse", "main"]), info.root);
        assert_eq!(
            outcome.behind,
            [BranchOnOldSnapshot {
                name: "side".to_owned(),
                tip: side,
            }]
        );
        assert_eq!(marker(&repo).roots, info.roots);

        // Nothing new on the server: nothing comes down and nothing moves.
        let again = refreshed(&repo, &info).expect("refresh");
        assert!(!again.moved);
        assert_eq!(again.changes, None);
        assert_eq!(again.transfer, TransferStats::default());
        assert_eq!(pull_result(&again), Some(PullResult::UpToDate));
        assert!(again.followed.is_empty());
        assert_eq!(again.behind.len(), 1);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_conflicting_refresh_stops_in_a_rebase_that_can_be_finished_or_aborted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        let work = commit(&co, "src/a.rs", "ours\n", "work on a");
        let info = served.moves("src/a.rs", "theirs\n");

        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(pull_result(&outcome), Some(PullResult::RebaseStopped));
        assert!(co.join(".git/rebase-merge/grit-plan").is_file());
        // The commit made with git was given a change id before the rebase.
        assert_eq!(outcome.gave_change_ids, 1);
        let given = oid(&git(&co, &["rev-parse", "work"]));
        assert_ne!(given, work);
        let data = crate::objects::parse_commit(&repo.odb().read(&given).expect("read").data)
            .expect("parse");
        assert_eq!(
            crate::change_id::change_id(&data),
            Some(crate::change_id::ChangeId::derived_from_commit_id(&work))
        );
        // The snapshot came down and is recorded; the branch waits for the rebase.
        assert_eq!(git(&co, &["rev-parse", "origin/main"]), info.root);
        assert_eq!(marker(&repo).roots, info.roots);
        assert!(matches!(
            refreshed(&repo, &info),
            Err(CheckoutError::OperationInProgress(
                InProgressOperation::Rebase
            ))
        ));

        let identity = "T <t@example.com> 0 +0000".parse().expect("signature");
        crate::revwalk::history::abort_rebase(&repo, &identity).expect("abort");
        assert!(ensure_refreshable(&repo).is_ok());
        assert_eq!(git(&co, &["rev-parse", "HEAD"]), given.to_hex());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_detached_refresh_moves_only_the_snapshot_and_a_moved_snapshot_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        git(&co, &["switch", "-q", "--detach"]);
        let head = git(&co, &["rev-parse", "HEAD"]);

        let info = served.moves("src/b.rs", "b2\n");
        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(outcome.head, RefreshedHead::Detached);
        assert_eq!(git(&co, &["rev-parse", "origin/main"]), info.root);
        assert_eq!(git(&co, &["rev-parse", "HEAD"]), head);

        // The server's snapshot moves on again while the fetch runs: nothing is recorded.
        let later = served.moves("src/b.rs", "b3\n");
        crate::refs::write_ref(
            open(&served.dir).git_dir(),
            &target_ref(ID),
            &oid(&info.root),
        )
        .expect("rewind");
        assert!(matches!(
            refreshed(&repo, &later),
            Err(CheckoutError::SnapshotMoved { .. })
        ));
        assert_eq!(marker(&repo).roots, info.roots);
    }

    /// A server that answers each `POST …/work` with the next of its names.
    struct WorkNames {
        names: std::sync::Mutex<Vec<&'static str>>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl crate::transport::http::HttpClient for WorkNames {
        fn get(&self, url: &str, _: Option<&str>) -> crate::error::Result<Vec<u8>> {
            Err(crate::error::Error::Message(format!(
                "unexpected GET {url}"
            )))
        }

        fn post(
            &self,
            url: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> crate::error::Result<Vec<u8>> {
            Err(crate::error::Error::Message(format!(
                "unexpected POST {url}"
            )))
        }

        fn send(
            &self,
            request: &crate::transport::http::HttpRequest<'_>,
        ) -> crate::error::Result<crate::transport::http::HttpResponse> {
            self.seen.lock().expect("lock").push(format!(
                "{} {} {}",
                request.method.as_str(),
                request.url,
                String::from_utf8_lossy(request.body)
            ));
            let name = self.names.lock().expect("lock").remove(0);
            let body = serde_json::to_vec(&super::super::WorkBranchResponse {
                work: name.to_owned(),
            })
            .expect("json");
            Ok(crate::transport::http::HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn landed_commits_are_never_replayed_and_the_user_moves_to_a_fresh_branch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        // Three commits appending at one place: replayed one by one onto a snapshot that has
        // all three, the first conflicts.
        let mut text = "a\n".to_owned();
        for (i, line) in ["one", "two", "three"].iter().enumerate() {
            text.push_str(line);
            text.push('\n');
            commit(&co, "src/a.rs", &text, line);
            if i == 1 {
                git(&co, &["branch", "side"]);
                git(&co, &["branch", "parked"]);
            }
        }
        let parked = oid(&git(&co, &["rev-parse", "parked"]));
        let landed = oid(&git(&co, &["rev-parse", "work"]));
        git(&co, &["switch", "-q", "side"]);
        let side = commit(&co, "src/s.rs", "s\n", "side work");
        git(&co, &["switch", "-q", "work"]);
        std::fs::write(co.join("src/draft.rs"), "not committed\n").expect("write");

        // The server merges them: its snapshot has their result.
        let info = served.moves("src/a.rs", &text);
        let recorded = record_landed(&repo, &marker(&repo), landed).expect("record");
        assert_eq!(recorded.landed, [landed.to_hex()]);
        assert_eq!(marker(&repo).landed, [landed.to_hex()]);

        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(pull_result(&outcome), Some(PullResult::Landed));
        assert_eq!(git(&co, &["rev-parse", "work"]), info.root);
        assert_eq!(git(&co, &["status", "--porcelain"]), "?? src/draft.rs");
        // A branch whose commits all landed follows; one with work of its own is named.
        assert_eq!(
            outcome.followed,
            [FollowedBranch {
                name: "parked".to_owned(),
                from: parked,
                to: oid(&info.root),
            }]
        );
        assert_eq!(git(&co, &["rev-parse", "parked"]), info.root);
        assert_eq!(
            outcome.behind,
            [BranchOnOldSnapshot {
                name: "side".to_owned(),
                tip: side,
            }]
        );
        // `side` still builds on two of the landed commits.
        assert_eq!(marker(&repo).landed, [landed.to_hex()]);

        // A fresh work branch from the server; one that is taken here is skipped.
        let names = WorkNames {
            names: std::sync::Mutex::new(vec!["side", "t/fresh"]),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let client = CheckoutClient::new(&names, "http://127.0.0.1:1/served.git").expect("client");
        let moved = move_off_landed_branch(&repo, &client, "work", "t/elm-oak")
            .expect("move off")
            .expect("moved");
        assert_eq!(
            moved,
            MovedOff {
                from: "work".to_owned(),
                to: "t/fresh".to_owned(),
                tip: oid(&info.root),
            }
        );
        assert_eq!(
            names.seen.lock().expect("lock").as_slice(),
            [
                format!(
                    "POST http://127.0.0.1:1/.grit/checkouts/served/{ID}/work {{\"work\":\"t/elm-oak\"}}"
                ),
                format!(
                    "POST http://127.0.0.1:1/.grit/checkouts/served/{ID}/work {{\"work\":\"t/elm-oak\"}}"
                ),
            ]
        );
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "t/fresh");
        assert!(git_output_fails(
            &co,
            &["rev-parse", "--verify", "-q", "refs/heads/work"]
        ));
        assert_eq!(git(&co, &["rev-parse", "HEAD"]), info.root);
        assert_eq!(git(&co, &["status", "--porcelain"]), "?? src/draft.rs");
        // Nothing to move off when HEAD is not on the branch.
        assert_eq!(
            move_off_landed_branch(&repo, &client, "work", "t/elm-oak").expect("no-op"),
            None
        );

        // The branch built on the landed commits keeps only its own when it is refreshed.
        git(&co, &["switch", "-q", "side"]);
        let again = refreshed(&repo, &info).expect("refresh side");
        assert_eq!(pull_result(&again), Some(PullResult::Rebased));
        assert_eq!(git(&co, &["rev-parse", "side^"]), info.root);
        assert_eq!(git(&co, &["log", "-1", "--format=%s", "side"]), "side work");
        // Nothing builds on the landed tip now, so it is forgotten.
        assert!(marker(&repo).landed.is_empty());
    }

    /// A server that is down: every request fails.
    struct Unreachable;

    impl crate::transport::http::HttpClient for Unreachable {
        fn get(&self, url: &str, _: Option<&str>) -> crate::error::Result<Vec<u8>> {
            Err(crate::error::Error::Message(format!("{url}: unreachable")))
        }

        fn post(
            &self,
            url: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> crate::error::Result<Vec<u8>> {
            Err(crate::error::Error::Message(format!("{url}: unreachable")))
        }
    }

    /// The server's answer to integrating `work` into `target`.
    fn integrated(target: &str, status: IntegrateStatus) -> IntegrateResponse {
        IntegrateResponse {
            status,
            name: "work".to_owned(),
            target: target.to_owned(),
            previous: "2222222222222222222222222222222222222222".to_owned(),
            head: "3333333333333333333333333333333333333333".to_owned(),
            commits: 2,
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn work_merged_into_another_branch_is_rebased_on_the_next_snapshot_not_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        commit(&co, "src/a.rs", "a\none\n", "one");
        let tip = commit(&co, "src/a.rs", "a\none\ntwo\n", "two");

        // The served HEAD names `develop` now, so that is where the merge went;
        // this checkout snapshots `main`, which doesn't get the commits.
        let landing = land_integration(
            &repo,
            &marker(&repo),
            &integrated("develop", IntegrateStatus::Merged),
            Some(tip),
        )
        .expect("land");
        assert_eq!(
            landing,
            Landing::Elsewhere {
                target: "develop".to_owned()
            }
        );
        assert!(marker(&repo).landed.is_empty());
        let names = WorkNames {
            names: std::sync::Mutex::new(vec!["t/fresh"]),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let client = CheckoutClient::new(&names, "http://127.0.0.1:1/served.git").expect("client");
        assert_eq!(
            landing
                .move_off(&repo, &client, "t/elm-oak")
                .expect("no-op"),
            None
        );
        assert!(names.seen.lock().expect("lock").is_empty());

        // `main` moves on without the work: the branch keeps its commits on top.
        let info = served.moves("README", "top2\n");
        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(pull_result(&outcome), Some(PullResult::Rebased));
        assert_eq!(git(&co, &["rev-parse", "work~2"]), info.root);
        assert_eq!(git(&co, &["log", "-2", "--format=%s", "work"]), "two\none");
        assert_eq!(
            std::fs::read_to_string(co.join("src/a.rs")).expect("read"),
            "a\none\ntwo\n"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn integrating_again_after_a_failed_move_off_finishes_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        let tip = commit(&co, "src/a.rs", "a\none\n", "one");
        let landing = land_integration(
            &repo,
            &marker(&repo),
            &integrated("main", IntegrateStatus::Merged),
            Some(tip),
        )
        .expect("land");
        let info = served.moves("src/a.rs", "a\none\n");
        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(pull_result(&outcome), Some(PullResult::Landed));

        // The server can't give out a new name: HEAD stays on the landed branch.
        let down =
            CheckoutClient::new(&Unreachable, "http://127.0.0.1:1/served.git").expect("client");
        assert!(landing.move_off(&repo, &down, "t/elm-oak").is_err());
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "work");
        assert_eq!(git(&co, &["rev-parse", "work"]), info.root);

        // Integrating again checks nothing in (the branch is at the snapshot)
        // and the server already has it; the move off still happens.
        let again = land_integration(
            &repo,
            &marker(&repo),
            &integrated("main", IntegrateStatus::AlreadyIntegrated),
            None,
        )
        .expect("land");
        assert_eq!(
            again,
            Landing::Here {
                name: "work".to_owned(),
                tip: None,
            }
        );
        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(pull_result(&outcome), Some(PullResult::UpToDate));
        let names = WorkNames {
            names: std::sync::Mutex::new(vec!["t/fresh"]),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let client = CheckoutClient::new(&names, "http://127.0.0.1:1/served.git").expect("client");
        assert_eq!(
            again
                .move_off(&repo, &client, "t/elm-oak")
                .expect("move off"),
            Some(MovedOff {
                from: "work".to_owned(),
                to: "t/fresh".to_owned(),
                tip: oid(&info.root),
            })
        );
        assert_eq!(git(&co, &["symbolic-ref", "--short", "HEAD"]), "t/fresh");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_landed_tip_another_worktree_is_detached_on_is_kept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut served = Served::new(tmp.path());
        let (co, repo) = check_out(&mut served, tmp.path());
        commit(&co, "src/a.rs", "a\none\n", "one");
        let tip = commit(&co, "src/a.rs", "a\none\ntwo\n", "two");
        let linked = tmp.path().join("linked");
        git(
            &co,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().expect("utf-8 path"),
                "work",
            ],
        );
        record_landed(&repo, &marker(&repo), tip).expect("record");

        // A refresh here moves `work` off the landed commits; the linked
        // worktree is still detached on them, so the tip is kept.
        let info = served.moves("src/a.rs", "a\none\ntwo\n");
        let outcome = refreshed(&repo, &info).expect("refresh");
        assert_eq!(pull_result(&outcome), Some(PullResult::Landed));
        assert_eq!(marker(&repo).landed, [tip.to_hex()]);

        // A branch started there later replays only its own commit.
        git(&linked, &["switch", "-q", "-c", "fromwt"]);
        commit(&linked, "src/c.rs", "c\n", "own commit");
        let linked_repo = open(&linked);
        let again = refreshed(&linked_repo, &info).expect("refresh linked");
        assert_eq!(pull_result(&again), Some(PullResult::Rebased));
        assert_eq!(git(&linked, &["rev-parse", "fromwt^"]), info.root);
        assert_eq!(
            git(&linked, &["log", "-1", "--format=%s", "fromwt"]),
            "own commit"
        );
        // Nothing builds on the landed tip any more.
        assert!(marker(&repo).landed.is_empty());
    }
}
