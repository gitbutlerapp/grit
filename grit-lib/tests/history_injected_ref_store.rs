//! History rewrites read and move `HEAD`, branches and reflogs through the repository's ref
//! store (issue #1888): with memory-only backends, with an on-disk repository wrapped by
//! [`Repository::with_ref_store`], during `rebase --exec`, and with `HEAD` moving between plan
//! and apply.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use grit_lib::command_runner::system_command_runner;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::branch::{create_branch, CreateBranchOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefStore};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::revwalk::history::{
    apply_rewrite, drop, read_history_last, reword, squash, start_rebase, undo_last_rewrite,
    verify_rebase_with_exec, AppliedRewrite, ApplyOptions, ExecSpec, HistoryError,
    RebaseOntoOptions, RebaseOutcome, RebaseRequest, RefScope, RewriteOptions, RewritePlan,
    SquashMessage, SquashOptions, StartRebaseOptions,
};
use grit_lib::state::{resolve_head_for_repository, HeadState};

const TS: &str = "T <t@e.x> 1700000000 +0000";

fn rewrite_opts() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: Default::default(),
        committer: TS.parse().expect("signature"),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_opts(message: &str, record: Option<&str>) -> ApplyOptions {
    ApplyOptions {
        identity: TS.parse().expect("signature"),
        reflog_message: message.into(),
        record_operation: record.map(Into::into),
        hooks: Default::default(),
    }
}

fn apply(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
) -> Result<AppliedRewrite, HistoryError> {
    apply_rewrite(repo, plan, opts, &mut NullProgress, &NeverCancel)
}

/// Repository with memory object and ref stores and an on-disk work tree only.
fn open_memory_repo(root: &Path) -> (Repository, Arc<MemoryRefStore>, std::path::PathBuf) {
    let (git_dir, wt) = (root.join("mem.git"), root.join("wt"));
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&wt).expect("work tree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n",
    )
    .expect("config");
    let refs = Arc::new(MemoryRefStore::new());
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(refs.clone())
        .initial_branch("main")
        .open(&git_dir, Some(&wt))
        .expect("open");
    (repo, refs, wt)
}

/// On-disk files repository whose refs live in an injected memory store.
fn open_disk_repo_with_memory_refs(root: &Path) -> (Repository, Arc<MemoryRefStore>) {
    Repository::init(root, &InitOptions::new().with_initial_branch("main")).expect("init");
    let config = root.join(".git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str("[user]\n\tname = T\n\temail = t@e.x\n");
    fs::write(&config, text).expect("write config");
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(root),
    )
    .expect("open");
    let refs = Arc::new(MemoryRefStore::new());
    refs.set_ref("HEAD", RawRef::Symbolic("refs/heads/main".into()))
        .expect("HEAD");
    (repo.with_ref_store(refs.clone()), refs)
}

fn commit_file(repo: &Repository, wt: &Path, name: &str, message: &str) -> ObjectId {
    fs::write(wt.join(name), format!("{name}\n")).expect("write");
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    commit_with_scope(
        repo,
        &CommitRequest {
            message: message.into(),
            author: Some(TS.parse().expect("signature")),
            committer: Some(TS.parse().expect("signature")),
            ..Default::default()
        },
        CommitScope::Staged,
        &mut NullProgress,
    )
    .expect("commit")
    .oid
}

fn three_commits(repo: &Repository, wt: &Path) -> Vec<ObjectId> {
    (1..=3)
        .map(|i| commit_file(repo, wt, &format!("f{i}"), &format!("c{i}")))
        .collect()
}

fn main_tip(refs: &MemoryRefStore) -> ObjectId {
    match refs.read_raw("refs/heads/main").expect("main") {
        Some(RawRef::Direct(oid)) => oid,
        other => panic!("refs/heads/main should be direct, got {other:?}"),
    }
}

fn head_commit(repo: &Repository) -> ObjectId {
    match resolve_head_for_repository(repo).expect("HEAD") {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => {
            assert_eq!(refname, "refs/heads/main");
            oid
        }
        other => panic!("HEAD should be on main, got {other:?}"),
    }
}

fn reflog_messages(refs: &dyn RefStore, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    refs.for_each_reflog_entry(name, false, &mut |entry| {
        out.push(entry.message.clone());
        ControlFlow::Continue(())
    })
    .expect("reflog");
    out
}

fn subjects(repo: &Repository, tip: ObjectId) -> Vec<String> {
    let mut out = Vec::new();
    let mut next = Some(tip);
    while let Some(oid) = next {
        let commit = repo.odb().read_commit(&oid).expect("commit");
        out.push(commit.message.lines().next().unwrap_or_default().to_owned());
        next = commit.parents.first().copied();
    }
    out
}

#[test]
fn reword_on_memory_backends_moves_refs_in_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, refs, wt) = open_memory_repo(dir.path());
    let tips = three_commits(&repo, &wt);
    assert_eq!(main_tip(&refs), tips[2]);

    let plan = reword(&repo, tips[1], "reworded\n".into(), &rewrite_opts()).expect("plan");
    assert_eq!(
        plan.head.old_commit,
        Some(tips[2]),
        "plan reads HEAD from the store"
    );
    assert_eq!(plan.head.head_ref.as_deref(), Some("refs/heads/main"));
    let applied =
        apply(&repo, &plan, &apply_opts("reword", Some("reword"))).expect("apply_rewrite");

    let new_tip = main_tip(&refs);
    assert_ne!(new_tip, tips[2]);
    assert_eq!(applied.head, new_tip);
    assert_eq!(head_commit(&repo), new_tip);
    assert_eq!(subjects(&repo, new_tip), ["c3", "reworded", "c1"]);
    assert_eq!(
        reflog_messages(refs.as_ref(), "refs/heads/main").last(),
        Some(&"reword".to_owned())
    );
    assert_eq!(
        reflog_messages(refs.as_ref(), "HEAD").last(),
        Some(&"reword".to_owned()),
        "the HEAD reflog follows the branch in the store"
    );

    let record = read_history_last(&repo).expect("read").expect("record");
    assert_eq!(record.head_new_commit, Some(new_tip));
    let undo = undo_last_rewrite(&repo, &apply_opts("undo", None), false).expect("undo");
    assert_eq!(undo.head, tips[2]);
    assert_eq!(main_tip(&refs), tips[2]);
    assert_eq!(head_commit(&repo), tips[2]);
}

#[test]
fn drop_and_squash_on_memory_backends_move_refs_and_worktree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, refs, wt) = open_memory_repo(dir.path());
    let tips = three_commits(&repo, &wt);

    let plan = drop(&repo, tips[1], &rewrite_opts()).expect("drop plan");
    let applied = apply(&repo, &plan, &apply_opts("drop", Some("drop"))).expect("drop");
    let dropped = main_tip(&refs);
    assert_eq!(applied.head, dropped);
    assert_eq!(head_commit(&repo), dropped);
    assert_eq!(subjects(&repo, dropped), ["c3", "c1"]);
    assert!(!wt.join("f2").exists(), "drop removes the dropped file");

    let c4 = commit_file(&repo, &wt, "f4", "c4");
    let base = tips[0].to_hex();
    let range = format!("{base}..{}", c4.to_hex());
    let plan = squash(
        &repo,
        &[range.as_str()],
        &SquashOptions {
            message: SquashMessage::Custom("squashed\n".into()),
            rewrite: rewrite_opts(),
        },
    )
    .expect("squash plan");
    let applied = apply(&repo, &plan, &apply_opts("squash", None)).expect("squash");
    let squashed = main_tip(&refs);
    assert_eq!(applied.head, squashed);
    assert_eq!(head_commit(&repo), squashed);
    assert_eq!(subjects(&repo, squashed), ["squashed", "c1"]);
    assert!(wt.join("f3").exists() && wt.join("f4").exists());
}

#[test]
fn clean_start_rebase_on_memory_backends_moves_the_branch_in_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, refs, wt) = open_memory_repo(dir.path());
    let tips = three_commits(&repo, &wt);
    create_branch(&repo, &CreateBranchOptions::at_head("side")).expect("side");
    let switch = |branch: &str, oid| {
        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: branch.into(),
                    oid,
                },
                dry_run: false,
                hooks: Default::default(),
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("switch");
    };
    switch("side", tips[2]);
    let side = commit_file(&repo, &wt, "s", "side");
    switch("main", tips[2]);
    let main_only = commit_file(&repo, &wt, "m", "main only");

    let outcome = start_rebase(
        &repo,
        &StartRebaseOptions {
            request: RebaseRequest {
                upstream: side,
                onto: None,
                branch_tip: main_only,
            },
            rebase: RebaseOntoOptions::new(RewriteOptions {
                scope: RefScope::Head,
                ..rewrite_opts()
            }),
            apply: apply_opts("rebase", Some("rebase")),
        },
    )
    .expect("start_rebase");
    let RebaseOutcome::Done { applied, .. } = outcome else {
        panic!("a clean replay finishes in one call");
    };
    let rebased = main_tip(&refs);
    assert_eq!(applied.head, rebased);
    assert_eq!(head_commit(&repo), rebased);
    assert_eq!(
        subjects(&repo, rebased),
        ["main only", "side", "c3", "c2", "c1"]
    );
    assert!(wt.join("s").exists() && wt.join("m").exists());

    undo_last_rewrite(&repo, &apply_opts("undo", None), false).expect("undo");
    assert_eq!(main_tip(&refs), main_only);
    assert!(!wt.join("s").exists());
}

#[test]
fn rewrite_on_disk_repository_with_injected_store_leaves_disk_refs_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, refs) = open_disk_repo_with_memory_refs(dir.path());
    let git_dir = dir.path().join(".git");
    let disk_head = fs::read_to_string(git_dir.join("HEAD")).expect("HEAD");
    let tips = three_commits(&repo, dir.path());

    let plan = reword(&repo, tips[0], "first\n".into(), &rewrite_opts()).expect("plan");
    let applied = apply(&repo, &plan, &apply_opts("reword", Some("reword"))).expect("apply");
    assert_eq!(applied.head, main_tip(&refs));
    assert_eq!(subjects(&repo, applied.head), ["c3", "c2", "first"]);
    undo_last_rewrite(&repo, &apply_opts("undo", None), false).expect("undo");
    assert_eq!(main_tip(&refs), tips[2]);

    assert_eq!(
        fs::read_to_string(git_dir.join("HEAD")).expect("HEAD"),
        disk_head,
        "on-disk HEAD must not move"
    );
    assert!(!git_dir.join("refs/heads/main").exists());
    assert!(!git_dir.join("logs").exists());
    assert!(!git_dir.join("packed-refs").exists());
}

#[test]
fn rebase_exec_detaches_and_restores_head_in_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, refs) = open_disk_repo_with_memory_refs(dir.path());
    let git_dir = dir.path().join(".git");
    let disk_head = fs::read_to_string(git_dir.join("HEAD")).expect("HEAD");
    let tips = three_commits(&repo, dir.path());

    let plan = reword(&repo, tips[1], "reworded\n".into(), &rewrite_opts()).expect("plan");
    let report = verify_rebase_with_exec(
        &repo,
        &plan,
        &ExecSpec {
            commands: vec!["test -f f2".into()],
        },
        system_command_runner().as_ref(),
    )
    .expect("exec");
    assert_eq!(report.runs.len(), 2);
    assert!(report.runs.iter().all(|run| run.exit_code == 0));
    assert_eq!(
        refs.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".into())),
        "exec restores the symbolic HEAD in the store"
    );
    assert_eq!(main_tip(&refs), tips[2], "exec moves no branch");
    assert_eq!(
        fs::read_to_string(git_dir.join("HEAD")).expect("HEAD"),
        disk_head,
        "on-disk HEAD must not move"
    );
}

#[test]
fn head_moved_in_store_after_plan_fails_before_any_ref_moves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, refs, wt) = open_memory_repo(dir.path());
    let tips = three_commits(&repo, &wt);
    let plan = reword(&repo, tips[1], "reworded\n".into(), &rewrite_opts()).expect("plan");

    refs.set_ref("HEAD", RawRef::Direct(tips[2]))
        .expect("detach");
    let err = apply(&repo, &plan, &apply_opts("reword", Some("reword"))).expect_err("HEAD moved");
    assert!(
        matches!(err, HistoryError::HeadMovedSincePlan { .. }),
        "{err:?}"
    );
    assert_eq!(main_tip(&refs), tips[2], "no ref may move");
    assert_eq!(
        refs.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Direct(tips[2]))
    );
    assert!(read_history_last(&repo).expect("read").is_none());
}
