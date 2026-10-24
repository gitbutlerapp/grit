//! Porcelain operations on a [`Repository::with_ref_store`] handle keep every ref, `HEAD`
//! and reflog in the injected store (issue #1733), and the same sequence on a default files
//! repository stays readable by the system `git`.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::branch::{create_branch, delete_branch, CreateBranchOptions, DeleteMode};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::switch::{
    resolve_switch_target, switch_repository, SwitchMode, SwitchOptions,
};
use grit_lib::porcelain::tag::{create_tag, CreateTagRequest};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::stash::{StashApplyOptions, StashPushOptions};
use grit_lib::state::{resolve_head_for_repository, HeadState};
use grit_lib::RefStorageFormat;

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";

fn environment() -> Environment {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    env
}

fn init_repo(dir: &Path) -> Repository {
    init_repo_with(dir, RefStorageFormat::Files)
}

fn init_repo_with(dir: &Path, format: RefStorageFormat) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main")
            .with_ref_storage(format),
    )
    .expect("init");
    let config = dir.join(".git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str("[user]\n\tname = T\n\temail = t@e.x\n");
    fs::write(&config, text).expect("write config");
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::with_environment(environment()).with_work_tree(dir),
    )
    .expect("open")
}

fn commit_all(repo: &Repository, message: &str) -> ObjectId {
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    commit_with_scope(
        repo,
        &CommitRequest {
            message: message.into(),
            author: Some(IDENT_TS.parse().expect("signature")),
            committer: Some(IDENT_TS.parse().expect("signature")),
            ..Default::default()
        },
        CommitScope::Staged,
        &mut NullProgress,
    )
    .expect("commit")
    .oid
}

fn switch_to(repo: &Repository, branch: &str) {
    let oid = repo
        .rev_walk()
        .resolve(&format!("refs/heads/{branch}"))
        .expect("branch tip");
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: branch.into(),
                oid,
            },
            dry_run: false,
        },
    )
    .expect("switch");
}

fn head_branch(repo: &Repository) -> Option<String> {
    match resolve_head_for_repository(repo).expect("head") {
        HeadState::Branch { short_name, .. } => Some(short_name),
        _ => None,
    }
}

/// Tips recorded while running [`battery`].
struct BatteryResult {
    first: ObjectId,
    main_tip: ObjectId,
    topic_tip: ObjectId,
    merged: ObjectId,
}

/// commit, branch, switch, commit, switch, commit, merge, tag, branch delete, reset,
/// stash push/pop, detached commit and switch back.
fn battery(repo: &Repository, wt: &Path) -> BatteryResult {
    fs::write(wt.join("a.txt"), "hi\n").expect("write");
    let first = commit_all(repo, "first");
    assert_eq!(repo.rev_walk().resolve("HEAD").expect("HEAD"), first);
    assert_eq!(
        repo.rev_walk().resolve("refs/heads/main").expect("main"),
        first
    );

    create_branch(repo, &CreateBranchOptions::at_head("topic")).expect("create topic");
    switch_to(repo, "topic");
    assert_eq!(head_branch(repo).as_deref(), Some("topic"));
    match resolve_switch_target(repo, "-").expect("switch -") {
        SwitchMode::Existing { branch, oid } => {
            assert_eq!(branch, "main");
            assert_eq!(oid, first);
        }
        other => panic!("`switch -` should name main, got {other:?}"),
    }

    fs::write(wt.join("b.txt"), "topic\n").expect("write");
    let topic_tip = commit_all(repo, "on topic");
    assert_eq!(
        repo.rev_walk().resolve("refs/heads/topic").expect("topic"),
        topic_tip
    );
    assert_eq!(
        repo.rev_walk().resolve("refs/heads/main").expect("main"),
        first
    );

    switch_to(repo, "main");
    assert_eq!(head_branch(repo).as_deref(), Some("main"));
    assert!(
        !wt.join("b.txt").exists(),
        "switch must update the worktree"
    );

    fs::write(wt.join("c.txt"), "main\n").expect("write");
    let main_tip = commit_all(repo, "on main");

    let merged = match integrate_branch(repo, "refs/heads/main", main_tip, topic_tip, "topic")
        .expect("integrate")
    {
        IntegrateBranchOutcome::Merged { oid, .. } => oid,
        other => panic!("expected a merge commit, got {other:?}"),
    };
    assert_eq!(repo.rev_walk().resolve("HEAD").expect("HEAD"), merged);
    assert!(wt.join("b.txt").exists());

    create_tag(
        repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: merged,
            message: None,
            tagger: None,
            sign: None,
            signing_key: None,
            tree_sha256: None,
            force: false,
            dry_run: false,
        },
    )
    .expect("tag");

    delete_branch(repo, "topic", DeleteMode::Safe, false).expect("delete merged topic");

    let reset = reset_to_commit(
        repo,
        &ResetOptions::new(
            main_tip,
            ResetMode::Hard,
            IDENT_TS,
            "reset: moving to HEAD~1",
        ),
    )
    .expect("reset");
    assert_eq!(reset.target, main_tip);
    assert_eq!(repo.rev_walk().resolve("HEAD").expect("HEAD"), main_tip);
    assert!(!wt.join("b.txt").exists());

    reset_to_commit(
        repo,
        &ResetOptions::new(merged, ResetMode::Hard, IDENT_TS, "reset: moving to v1"),
    )
    .expect("reset back");

    fs::write(wt.join("a.txt"), "dirty\n").expect("write");
    let stash = repo
        .stash()
        .push(&StashPushOptions {
            message: None,
            include_untracked: false,
            identity: IDENT_TS.parse().expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: Default::default(),
        })
        .expect("stash push")
        .expect("stash created")
        .oid;
    assert_eq!(fs::read_to_string(wt.join("a.txt")).expect("read"), "hi\n");
    assert_eq!(repo.rev_walk().resolve("refs/stash").expect("stash"), stash);
    assert!(
        !repo
            .stash()
            .pop(0, &StashApplyOptions::default())
            .expect("stash pop")
            .has_conflicts(),
        "pop must apply without conflicts"
    );
    assert_eq!(
        fs::read_to_string(wt.join("a.txt")).expect("read"),
        "dirty\n"
    );
    assert!(repo.rev_walk().resolve("refs/stash").is_err());
    fs::write(wt.join("a.txt"), "hi\n").expect("write");

    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: main_tip,
                reflog_to_label: "HEAD~1".into(),
            },
            dry_run: false,
        },
    )
    .expect("detach");
    assert!(head_branch(repo).is_none());
    fs::write(wt.join("d.txt"), "detached\n").expect("write");
    let detached = commit_all(repo, "detached");
    assert_eq!(repo.rev_walk().resolve("HEAD").expect("HEAD"), detached);
    fs::remove_file(wt.join("d.txt")).expect("rm");
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage rm");
    reset_to_commit(
        repo,
        &ResetOptions::new(detached, ResetMode::Hard, IDENT_TS, "reset: moving to HEAD"),
    )
    .expect("clean detached");
    switch_to(repo, "main");

    BatteryResult {
        first,
        main_tip,
        topic_tip,
        merged,
    }
}

fn reflog(store: &dyn RefStore, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    store
        .for_each_reflog_entry(name, false, &mut |entry| {
            out.push(entry.message.clone());
            ControlFlow::Continue(())
        })
        .expect("reflog");
    out
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path.display().to_string());
        }
    }
    out
}

#[test]
fn porcelain_battery_keeps_refs_head_and_reflogs_in_injected_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let disk_head = fs::read_to_string(dir.path().join(".git/HEAD")).expect("HEAD");

    let store = Arc::new(MemoryRefStore::new());
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "HEAD".into(),
                    new_value: Some(RawRef::Symbolic("refs/heads/main".into())),
                    expected: Expected::Any,
                    reflog: None,
                    flags: RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");
    let repo = repo.with_ref_store(store.clone());

    let result = battery(&repo, dir.path());

    assert_eq!(
        store.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".into()))
    );
    assert_eq!(
        store.read_raw("refs/heads/main").expect("main"),
        Some(RawRef::Direct(result.merged))
    );
    assert_eq!(store.read_raw("refs/heads/topic").expect("topic"), None);
    assert_eq!(
        store.read_raw("refs/tags/v1").expect("tag"),
        Some(RawRef::Direct(result.merged))
    );
    assert_eq!(store.read_raw("refs/stash").expect("stash"), None);
    assert_eq!(
        repo.rev_walk().resolve("HEAD").expect("HEAD"),
        result.merged
    );

    let main_log = reflog(store.as_ref(), "refs/heads/main");
    assert_eq!(
        main_log.first().map(String::as_str),
        Some("commit (initial): first")
    );
    assert!(main_log
        .iter()
        .any(|m| m.starts_with("commit (merge)") || m.starts_with("merge")));
    assert!(main_log.iter().any(|m| m == "reset: moving to HEAD~1"));
    let head_log = reflog(store.as_ref(), "HEAD");
    assert!(head_log
        .iter()
        .any(|m| m == "checkout: moving from main to topic"));
    assert!(head_log
        .iter()
        .any(|m| m == "checkout: moving from topic to main"));
    assert!(head_log.iter().any(|m| m == "commit: on topic"));
    assert!(head_log.iter().any(|m| m == "commit: detached"));
    assert!(head_log.len() >= main_log.len());
    let _ = (result.first, result.main_tip, result.topic_tip);

    let git_dir = dir.path().join(".git");
    assert_eq!(
        files_under(&git_dir.join("refs")),
        Vec::<String>::new(),
        "no loose refs may be written beside an injected store"
    );
    assert!(
        !git_dir.join("logs").exists(),
        "no reflogs may be written beside an injected store: {:?}",
        files_under(&git_dir.join("logs"))
    );
    assert!(!git_dir.join("packed-refs").exists());
    assert_eq!(
        fs::read_to_string(git_dir.join("HEAD")).expect("HEAD"),
        disk_head,
        "on-disk HEAD must not move"
    );
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn porcelain_battery_on_files_repository_is_git_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let result = battery(&repo, dir.path());

    git(dir.path(), &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(dir.path(), &["rev-parse", "HEAD", "refs/tags/v1"]),
        format!("{}\n{}\n", result.merged, result.merged)
    );
    assert_eq!(
        git(dir.path(), &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    let log = git(dir.path(), &["log", "--format=%H %s", "HEAD"]);
    for oid in [
        result.first,
        result.main_tip,
        result.topic_tip,
        result.merged,
    ] {
        assert!(log.contains(&oid.to_hex()), "{oid} missing from git log");
    }
    let main_reflog = git(
        dir.path(),
        &["reflog", "show", "--format=%gs", "refs/heads/main"],
    );
    assert!(main_reflog.lines().any(|l| l == "commit (initial): first"));
    assert!(main_reflog.lines().any(|l| l == "reset: moving to HEAD~1"));
    let head_reflog = git(dir.path(), &["reflog", "show", "--format=%gs", "HEAD"]);
    assert!(head_reflog
        .lines()
        .any(|l| l == "checkout: moving from main to topic"));
    assert!(head_reflog.lines().any(|l| l == "commit: detached"));
    assert!(git(dir.path(), &["for-each-ref", "refs/heads/topic"]).is_empty());
    git(dir.path(), &["status", "--porcelain"]);
}

#[test]
fn porcelain_battery_on_reftable_repository_keeps_refs_and_reflogs_in_reftable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo_with(dir.path(), RefStorageFormat::Reftable);
    let result = battery(&repo, dir.path());

    let store = repo.refs();
    assert_eq!(
        store.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".into()))
    );
    assert_eq!(
        store.read_raw("refs/heads/main").expect("main"),
        Some(RawRef::Direct(result.merged))
    );
    assert_eq!(store.read_raw("refs/heads/topic").expect("topic"), None);
    assert_eq!(store.read_raw("refs/stash").expect("stash"), None);
    let main_log = reflog(store, "refs/heads/main");
    assert_eq!(
        main_log.first().map(String::as_str),
        Some("commit (initial): first")
    );
    assert!(main_log.iter().any(|m| m == "reset: moving to HEAD~1"));
    let head_log = reflog(store, "HEAD");
    assert!(head_log.iter().any(|m| m == "commit: on topic"));
    assert!(head_log.iter().any(|m| m == "commit: detached"));
    assert!(head_log
        .iter()
        .any(|m| m == "checkout: moving from topic to main"));
    assert!(!dir.path().join(".git/logs").exists());
}

#[test]
fn head_state_comes_only_from_the_injected_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    fs::write(dir.path().join("a.txt"), "hi\n").expect("write");
    let on_disk = commit_all(&repo, "on disk");

    let store = Arc::new(MemoryRefStore::new());
    let repo = repo.with_ref_store(store.clone());
    assert_eq!(
        resolve_head_for_repository(&repo).expect("head"),
        HeadState::Invalid,
        "an empty store has no HEAD, whatever .git/HEAD says"
    );

    store
        .set_ref("HEAD", RawRef::Symbolic("refs/heads/fresh".into()))
        .expect("HEAD");
    assert_eq!(
        resolve_head_for_repository(&repo).expect("head"),
        HeadState::Branch {
            refname: "refs/heads/fresh".into(),
            short_name: "fresh".into(),
            oid: None,
        }
    );

    store
        .set_ref("HEAD", RawRef::Direct(on_disk))
        .expect("HEAD");
    assert_eq!(
        resolve_head_for_repository(&repo).expect("head"),
        HeadState::Detached { oid: on_disk }
    );
}

/// Repository whose injected store has `main` and `topic` at the first commit but no `HEAD`.
fn headless_store_repo(dir: &Path) -> (Repository, Arc<MemoryRefStore>, ObjectId) {
    let repo = init_repo(dir);
    fs::write(dir.join("a.txt"), "hi\n").expect("write");
    let tip = commit_all(&repo, "first");
    let store = Arc::new(MemoryRefStore::new());
    for name in ["refs/heads/main", "refs/heads/topic"] {
        store.set_ref(name, RawRef::Direct(tip)).expect("seed");
    }
    (repo.with_ref_store(store.clone()), store, tip)
}

#[test]
fn switch_to_existing_branch_installs_head_when_store_has_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, store, tip) = headless_store_repo(dir.path());
    assert_eq!(
        resolve_head_for_repository(&repo).expect("head"),
        HeadState::Invalid
    );

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "topic".into(),
                oid: tip,
            },
            dry_run: false,
        },
    )
    .expect("switch");
    assert_eq!(outcome.commit, tip);
    assert_eq!(
        store.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/topic".into()))
    );
    assert_eq!(head_branch(&repo).as_deref(), Some("topic"));
}

#[test]
fn detached_switch_installs_head_when_store_has_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, store, tip) = headless_store_repo(dir.path());

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: tip,
                reflog_to_label: "main".into(),
            },
            dry_run: false,
        },
    )
    .expect("detach");
    assert_eq!(
        store.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Direct(tip))
    );
    assert_eq!(
        resolve_head_for_repository(&repo).expect("head"),
        HeadState::Detached { oid: tip }
    );
}
