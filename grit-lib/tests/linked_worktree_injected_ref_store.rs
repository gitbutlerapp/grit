//! A linked worktree over injected ref and object stores (issue #2127): commits made through
//! a handle for the linked worktree move the branch its own `HEAD` (`worktrees/<id>/HEAD`)
//! names and log in its own `HEAD` reflog (`worktrees/<id>/logs/HEAD`), leaving the main
//! worktree's branch, `HEAD` and `HEAD` reflog in the shared store alone.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefStore};
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::state::{resolve_head_for_repository, HeadState};
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch};
use grit_lib::PostOperationHooks;

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";
const USER_CONFIG: &str = "[user]\n\tname = T\n\temail = t@e.x\n";

struct Fixture {
    _tmp: tempfile::TempDir,
    git_dir: PathBuf,
    linked: PathBuf,
    admin: PathBuf,
    refs: Arc<MemoryRefStore>,
    odb: Arc<MemoryStore>,
    main: Repository,
    first: ObjectId,
}

fn options() -> RepositoryOptions {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    RepositoryOptions::with_environment(env)
}

fn open_handle(fixture: &Fixture, git_dir: &Path, work_tree: &Path) -> Repository {
    open_on(
        &fixture.git_dir,
        git_dir,
        work_tree,
        &fixture.refs,
        &fixture.odb,
    )
}

fn open_on(
    common: &Path,
    git_dir: &Path,
    work_tree: &Path,
    refs: &Arc<MemoryRefStore>,
    odb: &Arc<MemoryStore>,
) -> Repository {
    RepositoryBuilder::new(options())
        .odb_builder(
            OdbBuilder::files(common.join("objects"))
                .primary(odb.clone())
                .alternates(false),
        )
        .ref_store(refs.clone())
        .initial_branch("main")
        .open(git_dir, Some(work_tree))
        .expect("open over injected stores")
}

fn commit_change(repo: &Repository, file: &Path, message: &str) -> ObjectId {
    fs::write(file, message).expect("write");
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    commit_with_scope(
        repo,
        &grit_test_support::commit_request(message, IDENT_TS),
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit")
    .oid
}

/// Main worktree with one commit on `main` and a linked worktree on a new branch `feat`.
fn two_worktrees() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = tmp.path().join("g.git");
    let wt = tmp.path().join("wt");
    let linked = tmp.path().join("linked");
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&wt).expect("work tree");
    fs::write(git_dir.join("config"), USER_CONFIG).expect("config");
    let refs = Arc::new(MemoryRefStore::new());
    let odb = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let main = open_on(&git_dir, &git_dir, &wt, &refs, &odb);
    let first = commit_change(&main, &wt.join("a.txt"), "one");
    let added = main
        .worktrees()
        .add(&AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some(WorktreeBranch::Create("feat".into())),
            start_point: None,
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .expect("worktrees().add");
    assert_eq!(
        fs::read_to_string(added.admin_dir.join("HEAD")).expect("linked HEAD"),
        "ref: refs/heads/feat\n"
    );
    Fixture {
        _tmp: tmp,
        admin: added.admin_dir,
        git_dir,
        linked,
        refs,
        odb,
        main,
        first,
    }
}

fn stored(refs: &MemoryRefStore, name: &str) -> Option<RawRef> {
    refs.read_raw(name).expect("read")
}

fn shared_head_reflog(refs: &MemoryRefStore) -> Vec<(ObjectId, ObjectId)> {
    let mut entries = Vec::new();
    refs.for_each_reflog_entry("HEAD", false, &mut |entry| {
        entries.push((entry.old_oid, entry.new_oid));
        ControlFlow::Continue(())
    })
    .expect("shared HEAD reflog");
    entries
}

/// `(old, new)` of each line of `<admin>/logs/HEAD`, which Git reads as the linked
/// worktree's `HEAD` reflog.
fn admin_head_reflog(admin: &Path) -> Vec<(ObjectId, ObjectId)> {
    let Ok(text) = fs::read_to_string(admin.join("logs/HEAD")) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| {
            let mut fields = line.split(' ');
            let mut next = || ObjectId::from_hex(fields.next().expect("oid")).expect("hex oid");
            let old = next();
            (old, next())
        })
        .collect()
}

/// Commit twice through `linked` and check where every ref and reflog entry went.
fn assert_commits_land_on_feat(fixture: &Fixture, linked: &Repository) {
    let shared_log_before = shared_head_reflog(&fixture.refs);
    let admin_log_before = admin_head_reflog(&fixture.admin);

    let second = commit_change(
        linked,
        &fixture.linked.join("b.txt"),
        "in the linked worktree",
    );
    let third = commit_change(linked, &fixture.linked.join("b.txt"), "and again");

    assert_eq!(
        stored(&fixture.refs, "refs/heads/feat"),
        Some(RawRef::Direct(third))
    );
    assert_eq!(
        stored(&fixture.refs, "refs/heads/main"),
        Some(RawRef::Direct(fixture.first))
    );
    assert_eq!(
        stored(&fixture.refs, "HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".into()))
    );
    assert_eq!(shared_head_reflog(&fixture.refs), shared_log_before);
    let mut expected_admin_log = admin_log_before;
    expected_admin_log.extend([(fixture.first, second), (second, third)]);
    assert_eq!(admin_head_reflog(&fixture.admin), expected_admin_log);
    assert_eq!(
        fs::read_to_string(fixture.admin.join("HEAD")).expect("linked HEAD"),
        "ref: refs/heads/feat\n"
    );
    assert_eq!(
        resolve_head_for_repository(linked).expect("linked HEAD"),
        HeadState::Branch {
            refname: "refs/heads/feat".into(),
            short_name: "feat".into(),
            oid: Some(third),
        }
    );
    assert_eq!(
        fixture.main.head_commit().expect("main HEAD"),
        fixture.first
    );
}

#[test]
fn builder_open_on_the_admin_dir_commits_onto_the_linked_branch() {
    let fixture = two_worktrees();
    let linked = open_handle(&fixture, &fixture.admin, &fixture.linked);
    assert_commits_land_on_feat(&fixture, &linked);
}

#[test]
fn open_linked_worktree_shares_the_injected_stores() {
    let fixture = two_worktrees();
    let linked = fixture
        .main
        .open_linked_worktree(&fixture.admin, &fixture.linked)
        .expect("open linked worktree");
    assert_commits_land_on_feat(&fixture, &linked);
}

#[test]
fn with_ref_store_on_a_linked_handle_keeps_the_worktree_head() {
    let fixture = two_worktrees();
    let replacement = fixture.refs.clone();
    let linked = open_handle(&fixture, &fixture.admin, &fixture.linked).with_ref_store(replacement);
    assert_commits_land_on_feat(&fixture, &linked);
}

#[test]
fn the_main_git_dir_on_the_same_store_still_commits_onto_main() {
    let fixture = two_worktrees();
    let main = open_handle(
        &fixture,
        &fixture.git_dir,
        &fixture.git_dir.with_file_name("wt"),
    );
    let shared_log_before = shared_head_reflog(&fixture.refs);
    let admin_log_before = admin_head_reflog(&fixture.admin);

    let second = commit_change(
        &main,
        &fixture.git_dir.with_file_name("wt").join("a.txt"),
        "two",
    );

    assert_eq!(
        stored(&fixture.refs, "refs/heads/main"),
        Some(RawRef::Direct(second))
    );
    assert_eq!(
        stored(&fixture.refs, "refs/heads/feat"),
        Some(RawRef::Direct(fixture.first))
    );
    let mut expected_shared_log = shared_log_before;
    expected_shared_log.push((fixture.first, second));
    assert_eq!(shared_head_reflog(&fixture.refs), expected_shared_log);
    assert_eq!(admin_head_reflog(&fixture.admin), admin_log_before);
}

fn switch(repo: &Repository, mode: SwitchMode) {
    switch_repository(
        repo,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("switch");
}

#[test]
fn detaching_and_switching_in_the_linked_worktree_leave_the_shared_head_alone() {
    let fixture = two_worktrees();
    let linked = open_handle(&fixture, &fixture.admin, &fixture.linked);
    let shared_log_before = shared_head_reflog(&fixture.refs);

    switch(
        &linked,
        SwitchMode::Detached {
            commit: fixture.first,
            reflog_to_label: fixture.first.to_hex(),
        },
    );
    assert_eq!(
        fs::read_to_string(fixture.admin.join("HEAD")).expect("linked HEAD"),
        format!("{}\n", fixture.first)
    );
    let detached = commit_change(&linked, &fixture.linked.join("c.txt"), "detached work");
    assert_eq!(
        resolve_head_for_repository(&linked).expect("linked HEAD"),
        HeadState::Detached { oid: detached }
    );
    assert_eq!(
        fs::read_to_string(fixture.admin.join("HEAD")).expect("linked HEAD"),
        format!("{detached}\n")
    );

    switch(
        &linked,
        SwitchMode::Existing {
            branch: "feat".into(),
            oid: fixture.first,
        },
    );
    assert_eq!(
        fs::read_to_string(fixture.admin.join("HEAD")).expect("linked HEAD"),
        "ref: refs/heads/feat\n"
    );
    for branch in ["refs/heads/main", "refs/heads/feat"] {
        assert_eq!(
            stored(&fixture.refs, branch),
            Some(RawRef::Direct(fixture.first)),
            "{branch}"
        );
    }
    assert_eq!(
        stored(&fixture.refs, "HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".into()))
    );
    assert_eq!(shared_head_reflog(&fixture.refs), shared_log_before);
    let admin_log = admin_head_reflog(&fixture.admin);
    assert_eq!(
        admin_log[admin_log.len() - 3..],
        [
            (fixture.first, fixture.first),
            (fixture.first, detached),
            (detached, fixture.first),
        ]
    );
    assert_eq!(
        fixture.main.head_commit().expect("main HEAD"),
        fixture.first
    );
}
