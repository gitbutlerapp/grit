//! `References`, `RepositoryReflogs` and `Operations` against the system `git`, both ways and
//! on both ref backends: refs grit writes through the handles are what `git for-each-ref`,
//! `git symbolic-ref`, `git reflog` and `git fsck` see, and the handles read what git writes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::{Expected, RawRef, RefName, RefUpdate};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::ReflogCreate;
use grit_lib::state::{HeadState, InProgressOperation};

use support::{
    append_repo_config, assert_git_fsck_strict, each_backend, git, git_for_each_ref,
    git_interop_available, git_reflog_lines, grit_reflog_identity, two_commit_oids, TestRepo,
    AUTHOR_EMAIL, AUTHOR_NAME,
};

fn open(repo: &TestRepo) -> Repository {
    Repository::open(
        &repo.git_dir(),
        &RepositoryOptions::empty().with_work_tree(repo.worktree()),
    )
    .expect("open repository")
}

fn grit_listing(repo: &Repository, prefix: &str) -> Vec<(String, ObjectId)> {
    repo.references()
        .list(prefix)
        .expect("list refs")
        .into_iter()
        .map(|(name, oid)| (name.into(), oid))
        .collect()
}

fn with_identity(repo: &TestRepo) {
    append_repo_config(
        repo.worktree(),
        &format!("[user]\n\tname = {AUTHOR_NAME}\n\temail = {AUTHOR_EMAIL}\n"),
    );
}

#[test]
fn handle_writes_are_what_git_reads() {
    each_backend(|backend, test_repo| {
        if !git_interop_available(backend) {
            return;
        }
        with_identity(test_repo);
        let (first, second) = two_commit_oids(test_repo);
        let worktree = test_repo.worktree();
        let repo = open(test_repo);
        let refs = repo.references();

        refs.update("refs/heads/topic", &first)
            .expect("create topic");
        refs.compare_and_swap("refs/heads/topic", &first, &second)
            .expect("move topic");
        let stale = refs.compare_and_swap("refs/heads/topic", &first, &second);
        assert!(stale.is_err(), "{backend:?}: stale compare-and-swap");
        refs.update("refs/tags/v1", &first).expect("tag");
        refs.update("refs/tags/v2", &second).expect("tag");
        refs.update_symbolic("refs/heads/alias", "refs/heads/topic")
            .expect("alias");
        refs.apply(&[RefUpdate::new(
            RefName::try_from("refs/heads/batch").expect("valid"),
            Some(RawRef::Direct(first)),
        )
        .with_expected(Expected::Missing)])
            .expect("batch");
        refs.compare_and_delete("refs/tags/v2", &second)
            .expect("delete v2");
        refs.update_symbolic("HEAD", "refs/heads/topic")
            .expect("switch HEAD");

        assert_eq!(
            git_for_each_ref(worktree, "refs/"),
            grit_listing(&repo, "refs/"),
            "{backend:?}: for-each-ref"
        );
        assert_eq!(
            git(worktree, &["symbolic-ref", "refs/heads/alias"]).trim(),
            "refs/heads/topic",
            "{backend:?}"
        );
        assert_eq!(
            git(worktree, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/topic",
            "{backend:?}"
        );
        assert_eq!(
            git(worktree, &["rev-parse", "HEAD"]).trim(),
            second.to_hex(),
            "{backend:?}"
        );

        let git_log: Vec<ObjectId> = git_reflog_lines(worktree, "refs/heads/topic")
            .into_iter()
            .map(|(_, oid, _)| oid)
            .collect();
        assert_eq!(git_log, vec![second, first], "{backend:?}: topic reflog");
        let grit_log: Vec<ObjectId> = refs
            .reflog("refs/heads/topic")
            .expect("reflog")
            .iter()
            .rev()
            .map(|entry| entry.new_oid)
            .collect();
        assert_eq!(grit_log, git_log, "{backend:?}: grit reads its reflog");

        let note = ReflogEntry {
            old_oid: second,
            new_oid: second,
            identity: grit_reflog_identity(),
            message: "handle note".to_owned(),
        };
        repo.reflogs()
            .append("refs/heads/topic", &note, ReflogCreate::IfAllowed)
            .expect("append");
        let newest = git_reflog_lines(worktree, "refs/heads/topic")
            .into_iter()
            .next()
            .expect("newest entry");
        assert_eq!(newest.2, "handle note", "{backend:?}");
        assert!(repo.reflogs().exists("refs/heads/topic").expect("exists"));
        assert!(git_ok_reflog_exists(worktree, "refs/heads/topic"));

        refs.delete("refs/heads/batch").expect("delete batch");
        assert!(
            !git_for_each_ref(worktree, "refs/heads/")
                .iter()
                .any(|(name, _)| name == "refs/heads/batch"),
            "{backend:?}"
        );
        assert_git_fsck_strict(worktree);
    });
}

fn git_ok_reflog_exists(worktree: &std::path::Path, refname: &str) -> bool {
    support::git_ok(worktree, &["reflog", "exists", refname])
}

#[test]
fn handles_read_what_git_writes() {
    each_backend(|backend, test_repo| {
        if !git_interop_available(backend) {
            return;
        }
        let worktree = test_repo.worktree();
        let (first, second) = two_commit_oids(test_repo);
        git(worktree, &["branch", "side", &first.to_hex()]);
        git(worktree, &["tag", "v1.0", &first.to_hex()]);
        git(
            worktree,
            &["symbolic-ref", "refs/heads/current", "refs/heads/main"],
        );
        git(
            worktree,
            &[
                "update-ref",
                "-m",
                "git moved it",
                "refs/heads/moved",
                &first.to_hex(),
            ],
        );
        git(
            worktree,
            &[
                "update-ref",
                "-m",
                "git moved it again",
                "refs/heads/moved",
                &second.to_hex(),
            ],
        );

        let repo = open(test_repo);
        let refs = repo.references();
        assert_eq!(
            grit_listing(&repo, "refs/"),
            git_for_each_ref(worktree, "refs/"),
            "{backend:?}: listing"
        );
        let tags: Vec<String> = refs
            .list_glob("refs/tags/v1*")
            .expect("glob")
            .into_iter()
            .map(|(name, _)| name.into())
            .collect();
        assert_eq!(tags, vec!["refs/tags/v1.0".to_owned()], "{backend:?}");
        assert_eq!(
            refs.symbolic("refs/heads/current")
                .expect("symbolic")
                .map(String::from),
            Some("refs/heads/main".to_owned()),
            "{backend:?}"
        );
        match refs.head().expect("head") {
            HeadState::Branch { refname, oid, .. } => {
                assert_eq!(refname, "refs/heads/main", "{backend:?}");
                assert_eq!(oid, Some(second), "{backend:?}");
            }
            other => panic!("{backend:?}: HEAD should be on main, got {other:?}"),
        }
        let side = refs.dwim("side").expect("dwim").expect("side matches");
        assert_eq!(side.name.as_str(), "refs/heads/side", "{backend:?}");
        assert_eq!(side.oid, first, "{backend:?}");
        assert_eq!(side.candidates, 1, "{backend:?}");
        assert!(refs.dwim("no-such-ref").expect("dwim").is_none());

        let messages: Vec<String> = refs
            .reflog("refs/heads/moved")
            .expect("reflog")
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        let git_messages: Vec<String> = git_reflog_lines(worktree, "refs/heads/moved")
            .into_iter()
            .rev()
            .map(|(_, _, message)| message)
            .collect();
        assert_eq!(messages, git_messages, "{backend:?}: reflog messages");

        git(worktree, &["checkout", "-q", "--detach", &first.to_hex()]);
        let repo = open(test_repo);
        assert_eq!(
            repo.references().head().expect("head"),
            HeadState::Detached { oid: first },
            "{backend:?}"
        );
        let status = repo.operation().status_state().expect("status state");
        assert!(
            !status.merge_in_progress && !status.rebase_in_progress,
            "{backend:?}"
        );
    });
}

#[test]
fn operation_state_reads_a_git_merge_in_progress() {
    each_backend(|backend, test_repo| {
        if !git_interop_available(backend) {
            return;
        }
        let worktree = test_repo.worktree();
        let (first, _second) = two_commit_oids(test_repo);
        git(worktree, &["checkout", "-q", "-b", "side", &first.to_hex()]);
        git(
            worktree,
            &["commit", "--allow-empty", "-q", "-m", "side work"],
        );
        let side: ObjectId = git(worktree, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("side tip");
        git(worktree, &["checkout", "-q", "main"]);
        let main: ObjectId = git(worktree, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("main tip");
        git(worktree, &["merge", "-q", "--no-ff", "--no-commit", "side"]);

        let repo = open(test_repo);
        let state = repo.operation().state().expect("state");
        assert_eq!(
            state.in_progress,
            vec![InProgressOperation::Merge],
            "{backend:?}"
        );
        assert_eq!(state.merge_heads, vec![side], "{backend:?}");
        assert!(state.merge_message.is_some(), "{backend:?}");
        assert_eq!(state.orig_head, Some(main), "{backend:?}");
        assert_eq!(
            repo.operation().blocking_switch().expect("blocking"),
            Some(InProgressOperation::Merge),
            "{backend:?}"
        );
        assert!(!state.is_clean());

        git(worktree, &["merge", "--abort"]);
        let state = repo.operation().state().expect("state after abort");
        assert!(state.is_clean(), "{backend:?}: {state:?}");
        assert!(state.merge_heads.is_empty(), "{backend:?}");
        assert!(repo
            .operation()
            .blocking_switch()
            .expect("blocking")
            .is_none());
    });
}

#[test]
fn handles_round_trip_through_a_reopened_store_on_both_backends() {
    for format in [
        grit_lib::refs::ref_storage::RefStorageFormat::Files,
        grit_lib::refs::ref_storage::RefStorageFormat::Reftable,
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
            dir.path(),
            &grit_lib::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(format),
        )
        .expect("init");
        append_repo_config(
            dir.path(),
            &format!("[user]\n\tname = {AUTHOR_NAME}\n\temail = {AUTHOR_EMAIL}\n"),
        );
        drop(repo);
        let git_dir = dir.path().join(".git");
        let first = support::stored_commit(&git_dir, 1);
        let second = support::stored_commit(&git_dir, 2);

        let repo = Repository::open(
            &git_dir,
            &RepositoryOptions::empty().with_work_tree(dir.path()),
        )
        .expect("open");
        let refs = repo.references();
        refs.update("HEAD", &first).expect("first commit on main");
        refs.update("refs/heads/topic", &first).expect("topic");
        refs.compare_and_swap("refs/heads/main", &first, &second)
            .expect("advance main");
        refs.update_symbolic("refs/remotes/origin/HEAD", "refs/heads/main")
            .expect("symref");
        refs.update("refs/tags/gone", &first).expect("tag");
        refs.delete("refs/tags/gone").expect("delete tag");
        refs.update("refs/tags/v1.0", &first).expect("tag v1.0");
        let outcomes = refs
            .apply_each(vec![
                RefUpdate::new(
                    RefName::try_from("refs/heads/ok").expect("valid"),
                    Some(RawRef::Direct(first)),
                )
                .with_expected(Expected::Missing),
                RefUpdate::new(
                    RefName::try_from("refs/heads/topic").expect("valid"),
                    Some(RawRef::Direct(second)),
                )
                .with_expected(Expected::Oid(second)),
            ])
            .expect("non-atomic batch");
        assert!(outcomes[0].result.is_ok(), "{format:?}: {outcomes:?}");
        assert!(outcomes[1].result.is_err(), "{format:?}: {outcomes:?}");
        drop(repo);

        let repo = Repository::open(
            &git_dir,
            &RepositoryOptions::empty().with_work_tree(dir.path()),
        )
        .expect("reopen");
        let refs = repo.references();
        assert_eq!(
            refs.head().expect("head").oid().copied(),
            Some(second),
            "{format:?}"
        );
        let names: Vec<String> = refs
            .list("refs/")
            .expect("list")
            .into_iter()
            .map(|(name, _)| name.into())
            .collect();
        assert_eq!(
            names,
            [
                "refs/heads/main",
                "refs/heads/ok",
                "refs/heads/topic",
                "refs/remotes/origin/HEAD",
                "refs/tags/v1.0",
            ],
            "{format:?}"
        );
        let tags: Vec<String> = refs
            .list_glob("refs/tags/v1*")
            .expect("glob")
            .into_iter()
            .map(|(name, _)| name.into())
            .collect();
        assert_eq!(
            tags,
            ["refs/tags/v1.0"],
            "{format:?}: glob prefix is not a directory"
        );
        assert_eq!(refs.resolve("refs/heads/topic").expect("topic"), first);
        assert_eq!(
            refs.symbolic("refs/remotes/origin/HEAD")
                .expect("symbolic")
                .map(String::from),
            Some("refs/heads/main".to_owned()),
            "{format:?}"
        );
        let origin = refs.dwim("origin").expect("dwim").expect("origin matches");
        assert_eq!(origin.name.as_str(), "refs/heads/main", "{format:?}");
        let main_log: Vec<ObjectId> = refs
            .reflog("refs/heads/main")
            .expect("reflog")
            .into_iter()
            .map(|entry| entry.new_oid)
            .collect();
        assert_eq!(main_log, vec![first, second], "{format:?}");
        assert!(
            repo.reflogs().exists("HEAD").expect("HEAD log"),
            "{format:?}"
        );
        assert!(repo.operation().state().expect("state").is_clean());
    }
}
