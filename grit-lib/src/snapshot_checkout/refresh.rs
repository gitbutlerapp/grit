//! Refreshing a snapshot checkout: bring it onto the server's newest snapshot.
//!
//! The server first makes the snapshot ([`super::CheckoutClient::update`]);
//! [`refresh`] then fetches it into [`upstream_ref`] ([`fetch_snapshot`],
//! which records it in the marker only once it has arrived), brings the
//! current branch onto it the way [`pull`] does (a rebase by default, which
//! stops in a resumable rebase session on a conflict), moves a local branch
//! named after the target that has no commits of its own along with it, and
//! reports every other branch still building on an older snapshot.

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

use super::refs::{swap_ref_as, Swap};
use super::{
    branches_with_work, fetch_snapshot, upstream_ref, CheckoutError, CheckoutInfo, CheckoutMarker,
    Result, SnapshotRoot,
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

/// A local branch named after the target, with no commits of its own, that a
/// refresh moved to the new snapshot.
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
    /// A local branch named after the target that followed the snapshot.
    pub followed: Vec<FollowedBranch>,
    /// Other branches with work still on an older snapshot, which only a
    /// refresh (or rebase) on each of them moves.
    pub behind: Vec<BranchOnOldSnapshot>,
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
/// 1. Fetches the snapshot from the marker's URL into [`upstream_ref`] and
///    records it in the marker ([`fetch_snapshot`]).
/// 2. With `HEAD` on a branch, integrates the snapshot as [`pull`] does with
///    `options` (a rebase unless `options.mode` says otherwise). A rebase that
///    conflicts stops in a resumable rebase session
///    ([`PullOptions::stop_on_conflict`]). With `HEAD` detached, nothing else
///    moves.
/// 3. Moves `refs/heads/<target>`, when it is not the current branch and has
///    no commits of its own, to the new snapshot (not with `options.check`).
/// 4. Lists the other branches with work that still build on an older
///    snapshot.
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
    let current_branch = match resolve_head_for_repository(repo)? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        HeadState::Detached { .. } => None,
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
            integrate.upstream = Some(FetchedUpstream {
                label: format!("origin/{}", info.branch),
                oid: root,
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
        follow_target_branch(repo, &fetched.marker, current_branch.as_deref(), root)?
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
    use crate::bundle::{write_bundle, BundleSpec};
    use crate::config::environment::RepositoryOptions;
    use crate::porcelain::pull::PullResult;
    use crate::progress::NeverCancel;
    use crate::remote::fetch::NoProgress;
    use crate::snapshot_checkout::{make_snapshot, materialize, push_for_checkin, target_ref};
    use crate::state::InProgressOperation;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const ID: &str = "0123456789abcdef0123";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn open(dir: &Path) -> Repository {
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex).expect("oid")
    }

    fn commit(dir: &Path, path: &str, text: &str, message: &str) -> ObjectId {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(file, text).expect("write");
        git(dir, &["add", path]);
        git(dir, &["commit", "-qm", message]);
        oid(&git(dir, &["rev-parse", "HEAD"]))
    }

    /// A served repository and the snapshots of `src` it made for one checkout.
    struct Served {
        dir: PathBuf,
        roots: Vec<SnapshotRoot>,
    }

    impl Served {
        fn new(root: &Path) -> Self {
            let dir = root.join("served");
            std::fs::create_dir_all(&dir).expect("mkdir");
            git(&dir, &["init", "-q", "-b", "main"]);
            for (path, text) in [
                ("README", "top\n"),
                ("src/a.rs", "a\n"),
                ("src/b.rs", "b\n"),
                ("docs/d.md", "d\n"),
            ] {
                commit(&dir, path, text, path);
            }
            Self {
                dir,
                roots: Vec::new(),
            }
        }

        fn url(&self) -> String {
            self.dir.display().to_string()
        }

        /// A snapshot of `main` on top of the last one, as the server's update makes it.
        fn snapshot(&mut self) -> CheckoutInfo {
            let repo = open(&self.dir);
            let base = oid(&git(&self.dir, &["rev-parse", "main"]));
            let parent = self.roots.last().map(|r| oid(&r.commit));
            let paths = vec!["src".to_owned()];
            let snap = make_snapshot(&repo, base, &paths, parent).expect("snapshot");
            crate::refs::write_ref(repo.git_dir(), &target_ref(ID), &snap.commit)
                .expect("target ref");
            self.roots.push(SnapshotRoot {
                commit: snap.commit.to_hex(),
                base: base.to_hex(),
            });
            CheckoutInfo {
                id: ID.to_owned(),
                name: "served".to_owned(),
                branch: "main".to_owned(),
                paths,
                base: base.to_hex(),
                root: snap.commit.to_hex(),
                root_object: String::from_utf8_lossy(&snap.object).into_owned(),
                tree: snap.tree.to_hex(),
                roots: self.roots.clone(),
                files: snap.files,
                bytes: snap.bytes,
                bundle_bytes: 0,
                work: Some("work".to_owned()),
            }
        }

        /// Commit on the served `main` and snapshot it.
        fn moves(&mut self, path: &str, text: &str) -> CheckoutInfo {
            commit(&self.dir, path, text, &format!("server changes {path}"));
            self.snapshot()
        }
    }

    /// A checkout of `served`'s first snapshot at `at/co`, on branch `work`.
    fn check_out(served: &mut Served, at: &Path) -> (PathBuf, Repository) {
        let info = served.snapshot();
        let bundle = at.join("snapshot.bundle");
        let mut out = std::fs::File::create(&bundle).expect("bundle file");
        write_bundle(
            &open(&served.dir),
            &BundleSpec {
                include: vec![(oid(&info.root), "refs/heads/main".to_owned())],
                exclude: Vec::new(),
                filter: None,
            },
            &mut out,
        )
        .expect("bundle");
        let dest = at.join("co");
        let made = materialize(
            &dest,
            &info,
            &bundle,
            &served.url(),
            "work",
            &mut NoProgress,
            &NeverCancel,
        )
        .expect("materialize");
        git(&dest, &["config", "user.name", "T"]);
        git(&dest, &["config", "user.email", "t@example.com"]);
        (dest, made.repo)
    }

    fn marker(repo: &Repository) -> CheckoutMarker {
        CheckoutMarker::read(repo.git_dir()).expect("a checkout")
    }

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
        // The snapshot came down and is recorded; the branch waits for the rebase.
        assert_eq!(git(&co, &["rev-parse", "origin/main"]), info.root);
        assert_eq!(marker(&repo).roots, info.roots);
        assert_eq!(git(&co, &["rev-parse", "work"]), work.to_hex());
        assert!(matches!(
            refreshed(&repo, &info),
            Err(CheckoutError::OperationInProgress(
                InProgressOperation::Rebase
            ))
        ));

        let identity = "T <t@example.com> 0 +0000".parse().expect("signature");
        crate::revwalk::history::abort_rebase(&repo, &identity).expect("abort");
        assert!(ensure_refreshable(&repo).is_ok());
        assert_eq!(git(&co, &["rev-parse", "HEAD"]), work.to_hex());
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
}
