//! Change ids in snapshot checkouts: telling a rebased copy of a server
//! branch's commits from new work, and giving commits made with plain Git an
//! id before they are sent.
//!
//! A commit's **change key** is its `change-id` header, or the id derived from
//! its own commit id when it has none ([`resolved_change_id`]). Rebases keep
//! the header, so the keys of a branch's commits say whether a branch the
//! server is asked to replace still has every commit it had
//! ([`keeps_every_commit`]), however it was rebased. A copy counts only when
//! it also holds what its original changed outside the checkout's paths,
//! which the checkout never sees.
//!
//! Plain Git writes no header and drops one when it rewrites a commit. So
//! before a check-in or a refresh, the checkout gives each commit above its
//! snapshots that lacks one the id derived from its own commit id
//! ([`give_change_ids`]): the id it already reports, and the one the server's
//! graft gives a commit that reaches it without one. Only the header is added
//! (and a signature that no longer holds dropped), so trees stay the same and
//! the index and worktree are untouched.

use std::collections::HashMap;

use time::OffsetDateTime;

use crate::change_id::{change_id, resolved_change_id, set_change_id, ChangeId, ChangeIdPolicy};
use crate::ident_resolve::resolve_loose_committer_identity_with;
use crate::merge_base::{walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit};
use crate::objects::{parse_commit, serialize_commit, ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::state::{detect_in_progress, resolve_head_for_repository, HeadState};

use super::graft::{parents_first, without_signatures};
use super::refs::{log_head_move_as, swap_ref_as, Swap};
use super::tree::{diff, lookup, Change};
use super::{branches_with_work, in_view, CheckoutMarker, Result};

/// The reflog message of a branch [`give_change_ids`] moves.
const REFLOG_MESSAGE: &str = "change ids: give commits made with git a change id";

/// The change key of every commit reachable from `include` but not from
/// `exclude`, oldest first (each commit after its parents among them).
///
/// A commit's key is its `change-id` header, or, when it has none, the id
/// [`ChangeId::derived_from_commit_id`] gives it ([`resolved_change_id`]).
///
/// # Parameters
///
/// - `repo`: the repository holding the commits.
/// - `include`: the tips to walk from.
/// - `exclude`: tips whose history is left out.
///
/// # Returns
///
/// Each commit with its key.
///
/// # Errors
///
/// Walk and object read failures.
pub fn change_keys(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
) -> Result<Vec<(ObjectId, ChangeId)>> {
    let (_, newest_first) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        include,
        exclude,
        ReachableWalkLimit::Unlimited,
    )?;
    let odb = repo.odb();
    parents_first(repo, &newest_first)?
        .into_iter()
        .map(|oid| {
            let data = parse_commit(&odb.read(&oid)?.data)?;
            Ok((oid, resolved_change_id(&oid, &data).id))
        })
        .collect()
}

/// Whether `grafted` has every commit of `previous` that the target lacks,
/// matched by change key ([`change_keys`]): a rebase of it, perhaps with more
/// on top.
///
/// It holds when every commit of `previous --not grafted <targets>` has a
/// copy with its key among `grafted --not previous <targets>`, so commits the
/// target already has never count as dropped. A copy counts only when its
/// tree holds what the original changed (against its first parent) outside
/// `paths`, the checkout's paths: the checkout never sees those files, so a
/// teammate's amend that also changed one must not be dropped unnoticed.
/// Inside the paths the copy's version wins, as the checkout's own edit of
/// the change.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `previous`: where the server's branch is.
/// - `grafted`: what a check-in would set it to.
/// - `targets`: the tips of the target branch.
/// - `paths`: the checking-in checkout's paths (empty: the whole tree, so
///   nothing is outside them).
///
/// # Errors
///
/// Walk and object read failures.
pub fn keeps_every_commit(
    repo: &Repository,
    previous: ObjectId,
    grafted: ObjectId,
    targets: &[ObjectId],
    paths: &[String],
) -> Result<bool> {
    let mut not_grafted = vec![grafted];
    not_grafted.extend_from_slice(targets);
    let dropped = change_keys(repo, &[previous], &not_grafted)?;
    if dropped.is_empty() {
        return Ok(true);
    }
    let mut not_previous = vec![previous];
    not_previous.extend_from_slice(targets);
    let mut copies: HashMap<ChangeId, Vec<ObjectId>> = HashMap::new();
    for (oid, key) in change_keys(repo, &[grafted], &not_previous)? {
        copies.entry(key).or_default().push(oid);
    }
    for (original, key) in &dropped {
        let Some(copies) = copies.get(key) else {
            return Ok(false);
        };
        if !one_holds(repo, copies, &changes_outside(repo, *original, paths)?)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What `commit` changes against its first parent outside `paths`.
fn changes_outside(repo: &Repository, commit: ObjectId, paths: &[String]) -> Result<Vec<Change>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let odb = repo.odb();
    let data = parse_commit(&odb.read(&commit)?.data)?;
    let parent_tree = match data.parents.first() {
        Some(parent) => Some(parse_commit(&odb.read(parent)?.data)?.tree),
        None => None,
    };
    Ok(diff(odb, parent_tree, Some(data.tree))?
        .into_iter()
        .filter(|change| !in_view(&change.path, paths))
        .collect())
}

/// Whether the tree of one of `commits` holds what each of `changes` made
/// its path.
fn one_holds(repo: &Repository, commits: &[ObjectId], changes: &[Change]) -> Result<bool> {
    if changes.is_empty() {
        return Ok(true);
    }
    let odb = repo.odb();
    'commits: for commit in commits {
        let tree = parse_commit(&odb.read(commit)?.data)?.tree;
        for change in changes {
            if lookup(odb, tree, &change.path)? != change.new {
                continue 'commits;
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// Give a change id to every commit made with plain Git on the checkout's
/// branches above its snapshots, so the server can tell a rebased copy of it
/// from new work later.
///
/// A commit without a `change-id` header is rewritten with the id
/// [`ChangeId::derived_from_commit_id`] gives its own commit id (what
/// [`resolved_change_id`] already reports for it, and what the server's graft
/// would give it). A commit whose parent was rewritten is rewritten onto the
/// new parent, keeping its id. Rewritten commits drop signatures, which no
/// longer hold. Trees are unchanged, so the index and worktree stay as they
/// are.
///
/// Only branches with work on a snapshot ([`branches_with_work`]) are
/// rewritten, never history under the snapshots, a remote-tracking ref or a
/// tip that landed in the target ([`CheckoutMarker::landed`]). Each branch
/// moves by compare-and-swap with a reflog entry (one that moved meanwhile
/// is left alone), and so does a detached `HEAD` on a rewritten commit. The
/// reflog entries are by the committer Git would log (from the repository's
/// environment and config) at `now`.
///
/// Nothing happens while an operation is in progress in any of the
/// checkout's worktrees (a rebase, merge, cherry-pick or revert, one paused
/// between the commits of a sequence too, am or bisect), whose state names
/// the commits as they are, or when `grit.changeIds` is off
/// ([`ChangeIdPolicy`]).
///
/// # Parameters
///
/// - `repo`: the checkout.
/// - `marker`: its marker.
/// - `only`: the branches (without `refs/heads/`) to give ids on; `None` for
///   every branch with work.
/// - `now`: the time the reflog entries record.
///
/// # Returns
///
/// How many commits were given an id (commits rewritten only because their
/// parent was are not counted).
///
/// # Errors
///
/// Config, worktree, walk, object and ref failures.
pub fn give_change_ids(
    repo: &Repository,
    marker: &CheckoutMarker,
    only: Option<&[String]>,
    now: OffsetDateTime,
) -> Result<usize> {
    if operation_in_progress(repo)? {
        return Ok(0);
    }
    let config = repo.config()?;
    if !ChangeIdPolicy::from_config(config.as_ref()).enabled() {
        return Ok(0);
    }
    let branches: Vec<(String, ObjectId)> = branches_with_work(repo, marker)?
        .into_iter()
        .filter(|(name, _)| only.is_none_or(|only| only.iter().any(|o| o == name)))
        .collect();
    if branches.is_empty() {
        return Ok(0);
    }
    let odb = repo.odb();
    let mut hide: Vec<ObjectId> = marker
        .roots
        .iter()
        .map(|root| root.commit.as_str())
        .chain(marker.landed.iter().map(String::as_str))
        .filter_map(|hex| ObjectId::from_hex(hex).ok())
        .collect();
    hide.extend(
        crate::refs::list_refs(repo.git_dir(), "refs/remotes/")?
            .into_iter()
            .map(|(_, oid)| oid)
            .filter(|oid| odb.read(oid).is_ok_and(|o| o.kind == ObjectKind::Commit)),
    );
    let tips: Vec<ObjectId> = branches.iter().map(|(_, tip)| *tip).collect();
    let (_, newest_first) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        &tips,
        &hide,
        ReachableWalkLimit::Unlimited,
    )?;
    let mut rewritten: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut given = 0;
    for commit in parents_first(repo, &newest_first)? {
        let mut data = parse_commit(&odb.read(&commit)?.data)?;
        let mut moved = false;
        for parent in &mut data.parents {
            if let Some(new) = rewritten.get(parent) {
                *parent = *new;
                moved = true;
            }
        }
        let has_id = change_id(&data).is_some();
        if has_id && !moved {
            continue;
        }
        data.extra_headers = without_signatures(&data.extra_headers);
        if !has_id {
            set_change_id(&mut data, &ChangeId::derived_from_commit_id(&commit));
            given += 1;
        }
        rewritten.insert(
            commit,
            odb.write(ObjectKind::Commit, &serialize_commit(&data))?,
        );
    }
    if rewritten.is_empty() {
        return Ok(0);
    }
    let identity = resolve_loose_committer_identity_with(repo.environment(), &config, now);
    move_rewritten(repo, &branches, &rewritten, &identity)?;
    Ok(given)
}

/// Whether any worktree of `repo` is in the middle of an operation whose
/// state names commits as they are: a rebase, merge, cherry-pick, revert, am
/// or bisect, or a sequence of picks or reverts paused between two of them
/// (`sequencer/todo` with no `CHERRY_PICK_HEAD`), which Git reports as a
/// cherry-pick or revert in progress too.
fn operation_in_progress(repo: &Repository) -> Result<bool> {
    let mut git_dirs = vec![repo.git_dir().to_path_buf()];
    git_dirs.extend(
        crate::worktree::list_worktrees(repo)?
            .into_iter()
            .map(|worktree| worktree.admin_dir),
    );
    Ok(git_dirs.iter().any(|git_dir| {
        !detect_in_progress(git_dir).is_empty() || git_dir.join("sequencer/todo").is_file()
    }))
}

/// Move each of `branches` whose tip was rewritten, and a detached `HEAD` on
/// a rewritten commit, to the new commit, logged as `identity` (a whole
/// reflog identity line).
fn move_rewritten(
    repo: &Repository,
    branches: &[(String, ObjectId)],
    rewritten: &HashMap<ObjectId, ObjectId>,
    identity: &str,
) -> Result<()> {
    let head = resolve_head_for_repository(repo)?;
    for (name, tip) in branches {
        let Some(new) = rewritten.get(tip) else {
            continue;
        };
        let refname = format!("refs/heads/{name}");
        let swapped = swap_ref_as(repo, &refname, Some(*tip), *new, identity, REFLOG_MESSAGE)?;
        if swapped == Swap::Done
            && matches!(&head, HeadState::Branch { refname: current, .. } if *current == refname)
        {
            log_head_move_as(repo, *tip, *new, identity, REFLOG_MESSAGE)?;
        }
    }
    if let HeadState::Detached { oid } = head {
        if let Some(new) = rewritten.get(&oid) {
            swap_ref_as(repo, "HEAD", Some(oid), *new, identity, REFLOG_MESSAGE)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::InitOptions;
    use crate::snapshot_checkout::tree::edit;
    use crate::snapshot_checkout::{graft, make_snapshot, SnapshotRoot};

    const X: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    const Y: &str = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy";
    const Z: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";

    /// The time the tests' reflog entries record.
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap_or(OffsetDateTime::UNIX_EPOCH)
    }

    /// A repository whose `main` has `src/a` and `docs/d`, a snapshot of
    /// `src` on it, and a marker naming that snapshot.
    struct Checkout {
        _dir: tempfile::TempDir,
        repo: Repository,
        snapshot: ObjectId,
        marker: CheckoutMarker,
    }

    fn checkout() -> Checkout {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();
        let blob = |text: &str| {
            (
                0o100644,
                odb.write(ObjectKind::Blob, text.as_bytes()).expect("blob"),
            )
        };
        let full = edit(
            odb,
            None,
            &[
                (b"src/a".to_vec(), Some(blob("a"))),
                (b"docs/d".to_vec(), Some(blob("d"))),
            ],
        )
        .expect("tree");
        let base = commit(&repo, full, &[], "base", None);
        let paths = vec!["src".to_owned()];
        let snap = make_snapshot(&repo, base, &paths, None).expect("snapshot");
        let marker = CheckoutMarker {
            url: "http://127.0.0.1/demo.git".to_owned(),
            id: "0123456789abcdef0123".to_owned(),
            branch: "main".to_owned(),
            paths,
            roots: vec![SnapshotRoot {
                commit: snap.commit.to_hex(),
                base: base.to_hex(),
            }],
            landed: Vec::new(),
        };
        crate::refs::write_ref(repo.git_dir(), "refs/remotes/origin/main", &snap.commit)
            .expect("upstream");
        Checkout {
            _dir: dir,
            repo,
            snapshot: snap.commit,
            marker,
        }
    }

    /// A commit of `tree` on `parents`, with a `change-id` header when `id`
    /// is given.
    fn commit(
        repo: &Repository,
        tree: ObjectId,
        parents: &[ObjectId],
        msg: &str,
        id: Option<&str>,
    ) -> ObjectId {
        let mut s = format!("tree {}\n", tree.to_hex());
        for p in parents {
            s.push_str(&format!("parent {}\n", p.to_hex()));
        }
        s.push_str("author A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n");
        if let Some(id) = id {
            s.push_str(&format!("change-id {id}\n"));
        }
        s.push_str(&format!("\n{msg}\n"));
        repo.odb()
            .write(ObjectKind::Commit, s.as_bytes())
            .expect("commit")
    }

    impl Checkout {
        /// A commit on `parent` with the snapshot's tree.
        fn on(&self, parent: ObjectId, msg: &str, id: Option<&str>) -> ObjectId {
            let tree = self.read(parent).tree;
            commit(&self.repo, tree, &[parent], msg, id)
        }

        fn read(&self, oid: ObjectId) -> crate::objects::CommitData {
            parse_commit(&self.repo.odb().read(&oid).expect("read").data).expect("parse")
        }

        fn branch(&self, name: &str, tip: ObjectId) {
            crate::refs::write_ref(self.repo.git_dir(), &format!("refs/heads/{name}"), &tip)
                .expect("branch");
        }

        fn tip(&self, refname: &str) -> ObjectId {
            crate::refs::resolve_ref(self.repo.git_dir(), refname).expect("ref")
        }

        fn give(&self, only: Option<&[String]>) -> usize {
            give_change_ids(&self.repo, &self.marker, only, now()).expect("give ids")
        }

        /// A commit on `parent` setting each `(path, text)` of `edits`, with
        /// the change id `id`.
        fn change(&self, parent: ObjectId, edits: &[(&str, &str)], id: &str) -> ObjectId {
            let odb = self.repo.odb();
            let edits: Vec<_> = edits
                .iter()
                .map(|(path, text)| {
                    let blob = odb.write(ObjectKind::Blob, text.as_bytes()).expect("blob");
                    (path.as_bytes().to_vec(), Some((0o100644, blob)))
                })
                .collect();
            let tree = edit(odb, Some(self.read(parent).tree), &edits).expect("tree");
            commit(&self.repo, tree, &[parent], "change", Some(id))
        }
    }

    #[test]
    fn commits_made_with_git_get_the_id_derived_from_them_and_children_follow() {
        let co = checkout();
        let plain = co.on(co.snapshot, "made with git", None);
        let named = co.on(plain, "made with grit", Some(X));
        co.branch("work", named);
        crate::refs::write_symbolic_ref(co.repo.git_dir(), "HEAD", "refs/heads/work")
            .expect("HEAD");

        assert_eq!(co.give(None), 1);
        let tip = co.tip("refs/heads/work");
        let top = co.read(tip);
        assert_eq!(change_id(&top).map(|id| id.to_string()), Some(X.to_owned()));
        assert_eq!(top.tree, co.read(named).tree, "trees are unchanged");
        let below = co.read(top.parents[0]);
        assert_eq!(
            change_id(&below),
            Some(ChangeId::derived_from_commit_id(&plain)),
            "the id the commit already reported"
        );
        assert_eq!(below.parents, vec![co.snapshot]);
        assert_eq!(below.message, "made with git\n");
        // The branch and HEAD logged the move, at the time given.
        for name in ["refs/heads/work", "HEAD"] {
            let log = crate::reflog::read_reflog(co.repo.git_dir(), name).expect("reflog");
            assert!(
                log.iter()
                    .any(|e| e.message == REFLOG_MESSAGE
                        && e.identity.ends_with(" 1700000000 +0000")),
                "{name}: {log:?}"
            );
        }

        // Everything has an id now: nothing more to do, and the same answer
        // again.
        assert_eq!(co.give(None), 0);
        assert_eq!(co.tip("refs/heads/work"), tip);
        assert_eq!(
            change_keys(&co.repo, &[tip], &[co.snapshot]).expect("keys"),
            vec![
                (top.parents[0], ChangeId::derived_from_commit_id(&plain)),
                (tip, change_id(&top).expect("id")),
            ]
        );
    }

    #[test]
    fn only_the_named_branches_change_and_signatures_are_dropped() {
        let co = checkout();
        let tree = co.read(co.snapshot).tree;
        let signed = format!(
            "tree {}\nparent {}\nauthor A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n\
             gpgsig -----BEGIN SSH SIGNATURE-----\n abc\n -----END SSH SIGNATURE-----\n\nSigned\n",
            tree.to_hex(),
            co.snapshot.to_hex()
        );
        let signed = co
            .repo
            .odb()
            .write(ObjectKind::Commit, signed.as_bytes())
            .expect("commit");
        let other = co.on(co.snapshot, "other", None);
        co.branch("work", signed);
        co.branch("other", other);
        // A detached HEAD on the branch's tip moves with it.
        crate::refs::write_ref(co.repo.git_dir(), "HEAD", &signed).expect("detach");

        assert_eq!(co.give(Some(&["work".to_owned()])), 1);
        assert_eq!(co.tip("refs/heads/other"), other, "not named");
        let tip = co.tip("refs/heads/work");
        let bytes = co.repo.odb().read(&tip).expect("read").data;
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("gpgsig"), "{text}");
        assert!(text.contains("\nchange-id "), "{text}");
        assert_eq!(
            crate::refs::read_head(co.repo.git_dir()).expect("HEAD"),
            None,
            "still detached"
        );
        assert_eq!(co.tip("HEAD"), tip);
    }

    #[test]
    fn remote_tracking_and_landed_history_is_left_alone() {
        let mut co = checkout();
        // A teammate's commit made with git, fetched; work on it.
        let theirs = co.on(co.snapshot, "theirs", None);
        crate::refs::write_ref(co.repo.git_dir(), "refs/remotes/origin/shared", &theirs)
            .expect("remote");
        let mine = co.on(theirs, "mine", None);
        co.branch("shared", mine);
        // A landed tip made with git, and work on it.
        let landed = co.on(co.snapshot, "landed", None);
        let more = co.on(landed, "more", None);
        co.branch("work", more);
        co.marker.landed.push(landed.to_hex());

        assert_eq!(co.give(None), 2);
        assert_eq!(co.tip("refs/remotes/origin/shared"), theirs);
        let shared = co.read(co.tip("refs/heads/shared"));
        assert_eq!(shared.parents, vec![theirs]);
        assert_eq!(
            change_id(&shared),
            Some(ChangeId::derived_from_commit_id(&mine))
        );
        let work = co.read(co.tip("refs/heads/work"));
        assert_eq!(work.parents, vec![landed]);
        assert_eq!(co.tip("refs/remotes/origin/main"), co.snapshot);
    }

    #[test]
    fn nothing_changes_mid_operation_or_with_change_ids_off() {
        let co = checkout();
        let plain = co.on(co.snapshot, "made with git", None);
        co.branch("work", plain);

        std::fs::create_dir_all(co.repo.git_dir().join("rebase-merge")).expect("rebase");
        assert_eq!(co.give(None), 0);
        assert_eq!(co.tip("refs/heads/work"), plain);
        std::fs::remove_dir_all(co.repo.git_dir().join("rebase-merge")).expect("done");
        std::fs::write(co.repo.git_dir().join("CHERRY_PICK_HEAD"), plain.to_hex()).expect("pick");
        assert_eq!(co.give(None), 0);
        std::fs::remove_file(co.repo.git_dir().join("CHERRY_PICK_HEAD")).expect("done");

        let config = co.repo.git_dir().join("config");
        let text = std::fs::read_to_string(&config).expect("config");
        let mut file =
            crate::config::ConfigFile::parse(&config, &text, crate::config::ConfigScope::Local)
                .expect("parse");
        file.set("grit.changeIds", "false").expect("set");
        file.write().expect("write");
        co.repo.reload_config().expect("reload");
        assert_eq!(co.give(None), 0);
        assert_eq!(co.tip("refs/heads/work"), plain);
    }

    #[test]
    fn nothing_changes_while_another_worktree_is_mid_operation_or_picks_are_paused() {
        use crate::config::environment::RepositoryOptions;
        use crate::worktree::{add_linked_worktree, AddLinkedWorktreeOptions, WorktreeBranch};

        let co = checkout();
        let plain = co.on(co.snapshot, "made with git", None);
        co.branch("work", plain);

        // A sequence of picks paused between two of them has a todo list
        // but no CHERRY_PICK_HEAD.
        let sequencer = co.repo.git_dir().join("sequencer");
        std::fs::create_dir_all(&sequencer).expect("sequencer");
        std::fs::write(
            sequencer.join("todo"),
            format!("pick {} next\n", plain.to_hex()),
        )
        .expect("todo");
        assert_eq!(co.give(None), 0);
        std::fs::remove_dir_all(&sequencer).expect("done");

        // A linked worktree in the middle of a rebase.
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let linked = add_linked_worktree(
            &co.repo,
            &AddLinkedWorktreeOptions {
                path: elsewhere.path().join("wt2"),
                branch: Some(WorktreeBranch::Detach),
                start_point: Some(co.snapshot.to_hex()),
                force: false,
                lock: None,
                no_checkout: true,
                track: false,
            },
        )
        .expect("worktree");
        std::fs::create_dir_all(linked.admin_dir.join("rebase-merge")).expect("rebase");
        assert_eq!(co.give(None), 0, "seen from the main worktree");
        std::fs::remove_dir_all(linked.admin_dir.join("rebase-merge")).expect("done");

        // The main worktree in the middle of a rebase, seen from the linked one.
        std::fs::create_dir_all(co.repo.git_dir().join("rebase-merge")).expect("rebase");
        let from_linked = Repository::discover(Some(&linked.path), &RepositoryOptions::empty())
            .expect("open linked");
        let give = || give_change_ids(&from_linked, &co.marker, None, now()).expect("give ids");
        assert_eq!(give(), 0, "seen from the linked worktree");
        assert_eq!(co.tip("refs/heads/work"), plain);
        std::fs::remove_dir_all(co.repo.git_dir().join("rebase-merge")).expect("done");

        // With nothing in progress anywhere, the ids are given.
        assert_eq!(give(), 1);
        assert_ne!(co.tip("refs/heads/work"), plain);
    }

    #[test]
    fn a_branch_whose_reflog_is_not_utf8_is_given_ids_and_its_log_kept() {
        let co = checkout();
        let plain = co.on(co.snapshot, "Caf\u{e9}", None);
        co.branch("work", plain);
        // Plain Git logs a subject in a legacy encoding (here latin-1) as it is.
        let log = co.repo.git_dir().join("logs/refs/heads/work");
        std::fs::create_dir_all(log.parent().expect("parent")).expect("logs");
        let mut written = format!(
            "{} {} A <a@e> 1700000000 +0000\tcommit: Caf",
            co.snapshot.to_hex(),
            plain.to_hex()
        )
        .into_bytes();
        written.extend_from_slice(b"\xe9\n");
        std::fs::write(&log, &written).expect("log");

        assert_eq!(co.give(None), 1);
        assert_ne!(co.tip("refs/heads/work"), plain);
        let bytes = std::fs::read(&log).expect("log");
        assert!(bytes.starts_with(&written), "the log keeps what it had");
        assert!(String::from_utf8_lossy(&bytes[written.len()..]).contains(REFLOG_MESSAGE));
    }

    #[test]
    fn a_copy_counts_only_with_what_its_original_changed_outside_the_paths() {
        use crate::snapshot_checkout::{decide_checkin, CheckinDecision, CheckinRule};

        let co = checkout();
        let base = ObjectId::from_hex(&co.marker.roots[0].base).expect("base");
        let paths = co.marker.paths.as_slice();
        let keeps = |previous, grafted, targets: &[ObjectId], paths: &[String]| {
            keeps_every_commit(&co.repo, previous, grafted, targets, paths).expect("keeps")
        };
        // A teammate amended the server's commit X to change docs/d (outside
        // the checkout's paths) as well; the checkout checks in its own copy
        // of X with a commit on top.
        let amended = co.change(base, &[("src/a", "a2"), ("docs/d", "by bob")], X);
        let copy = co.change(base, &[("src/a", "a2")], X);
        let more = co.change(copy, &[("src/c", "c")], Y);
        assert!(
            !keeps(amended, more, &[base], paths),
            "docs/d would go back to what the target has"
        );
        assert_eq!(
            decide_checkin(&co.repo, Some(amended), more, &[base], paths, None, false)
                .expect("decide"),
            CheckinDecision::Diverged
        );
        assert_eq!(
            decide_checkin(&co.repo, Some(amended), more, &[base], paths, None, true)
                .expect("decide"),
            CheckinDecision::Update {
                rule: CheckinRule::Forced
            }
        );
        // A checkout of the whole tree saw docs/d: its copy is the change now.
        assert!(keeps(amended, more, &[base], &[]));

        // A copy that holds the docs change keeps the commit.
        let carried = co.change(base, &[("src/a", "a2"), ("docs/d", "by bob")], X);
        let on_carried = co.change(carried, &[("src/c", "c")], Y);
        assert!(keeps(amended, on_carried, &[base], paths));
        // So does one on a target that has the same docs change since.
        let target = co.change(base, &[("docs/d", "by bob")], Z);
        let rebased = co.change(target, &[("src/a", "a2")], X);
        assert!(keeps(amended, rebased, &[target], paths));
        // Inside the paths the copy's own version of the change wins.
        let again = co.change(base, &[("src/a", "a3")], X);
        assert!(keeps(copy, again, &[base], paths));
    }

    #[test]
    fn the_server_grafts_a_commit_given_its_id_as_it_grafts_the_commit_without_one() {
        let co = checkout();
        let odb = co.repo.odb();
        let tree = edit(
            odb,
            Some(co.read(co.snapshot).tree),
            &[(
                b"src/a".to_vec(),
                Some((0o100644, odb.write(ObjectKind::Blob, b"a2").expect("blob"))),
            )],
        )
        .expect("tree");
        let plain = commit(&co.repo, tree, &[co.snapshot], "change a", None);
        co.branch("work", plain);
        let raw = graft(&co.repo, &co.marker.roots, &co.marker.paths, plain)
            .expect("graft")
            .expect("grafted");

        assert_eq!(co.give(None), 1);
        let given = co.tip("refs/heads/work");
        assert_ne!(given, plain);
        let grafted = graft(&co.repo, &co.marker.roots, &co.marker.paths, given)
            .expect("graft")
            .expect("grafted");
        assert_eq!(grafted.tip, raw.tip, "the same commit on the server");
        assert_eq!(
            change_id(&co.read(grafted.tip)),
            Some(resolved_change_id(&plain, &co.read(plain)).id)
        );
    }
}
