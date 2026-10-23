//! Typed repository handles (remote, worktree, stash, notes, bundle) vs system `git`.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::notes::NotesRef;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::ListRefsOptions;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{
    LockWorktreeOptions, NotesWriteOptions, StashApplyOptions, StashPushOptions,
};
use grit_test_support::git;

fn git_cmd(root: &Path, args: &[&str]) -> String {
    git(root, args)
}

fn init_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn assert_fsck_strict(root: &Path) {
    let out = git_cmd(root, &["fsck", "--strict"]);
    assert!(
        !out.to_lowercase().contains("error"),
        "fsck --strict must be clean:\n{out}"
    );
}

const IDENT: &str = "Test <t@example.com> 1000000000 +0000";

#[test]
fn handle_stash_roundtrip_with_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "base\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("f"), "dirty\n").unwrap();
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&StashPushOptions {
            message: Some("handle stash".into()),
            include_untracked: false,
            identity: IDENT.into(),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
        })
        .expect("push")
        .expect("changes");

    let list = git_cmd(dir.path(), &["stash", "list"]);
    assert!(
        list.contains("handle stash") || list.contains("On main: handle stash"),
        "git stash list:\n{list}"
    );

    git_cmd(dir.path(), &["stash", "pop"]);
    assert_fsck_strict(dir.path());
}

#[test]
fn git_stash_applies_via_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "1\n").unwrap();
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "c"]);
    std::fs::write(dir.path().join("a"), "2\n").unwrap();
    git_cmd(dir.path(), &["stash", "push", "-m", "from git"]);

    let repo = open_grit(dir.path());
    let entries = repo.stash().list().expect("list");
    assert_eq!(entries.len(), 1);
    let conflicts = repo
        .stash()
        .apply(
            dir.path(),
            &StashApplyOptions {
                index: 0,
                restore_index: true,
                identity: IDENT.into(),
            },
        )
        .expect("apply");
    assert!(!conflicts);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "2\n"
    );
}

#[test]
fn handle_notes_roundtrip_with_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "b\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    let commit: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let repo = open_grit(dir.path());
    let notes_ref = NotesRef::commits();
    repo.notes()
        .add(
            &notes_ref,
            &commit,
            b"grit handle note\n",
            &NotesWriteOptions {
                message: "add".into(),
                committer: IDENT.into(),
                author: None,
            },
            false,
        )
        .expect("add");

    let show = git_cmd(dir.path(), &["notes", "show", &commit.to_hex()]);
    assert_eq!(show.trim(), "grit handle note");

    git_cmd(
        dir.path(),
        &["notes", "add", "-f", "-m", "git note", &commit.to_hex()],
    );
    let read = repo
        .notes()
        .read(&notes_ref, &commit)
        .expect("read")
        .expect("note");
    assert_eq!(read, b"git note\n");

    let listed = git_cmd(dir.path(), &["notes", "list"]);
    assert!(listed.contains(&commit.to_hex()[..7]));
    assert_fsck_strict(dir.path());
}

#[test]
fn handle_bundle_verify_and_git_unbundle() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "x\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "only"]);
    let head: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let bundle_path = dir.path().join("x.bundle");
    let repo = open_grit(dir.path());
    repo.bundles()
        .create(
            &grit_lib::bundle::BundleSpec {
                include: vec![(head, "refs/heads/main".into())],
                ..Default::default()
            },
            &bundle_path,
        )
        .expect("create");

    let verify = git_cmd(
        dir.path(),
        &["bundle", "verify", bundle_path.to_str().unwrap()],
    );
    assert!(
        !verify.to_lowercase().contains("error"),
        "git bundle verify:\n{verify}"
    );

    let heads = repo.bundles().list_heads(&bundle_path).expect("heads");
    assert!(heads.iter().any(|h| h.name == "refs/heads/main"));

    let consumer = tempfile::tempdir().expect("consumer");
    init_repo(consumer.path());
    let consumer_repo = open_grit(consumer.path());
    consumer_repo
        .bundles()
        .unbundle(&bundle_path)
        .expect("unbundle");
    assert_fsck_strict(consumer.path());
}

#[test]
fn git_bundle_unbundles_via_handle() {
    let src = tempfile::tempdir().expect("src");
    init_repo(src.path());
    std::fs::write(src.path().join("f"), "z\n").unwrap();
    git_cmd(src.path(), &["add", "f"]);
    git_cmd(src.path(), &["commit", "-qm", "tip"]);
    let bundle = src.path().join("from-git.bundle");
    git_cmd(
        src.path(),
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/main",
        ],
    );

    let dst = tempfile::tempdir().expect("dst");
    init_repo(dst.path());
    let repo = open_grit(dst.path());
    let report = repo.bundles().verify(&bundle).expect("verify");
    assert!(report.is_ok(), "{report:?}");
    repo.bundles().unbundle(&bundle).expect("unbundle");
    assert_fsck_strict(dst.path());
}

#[test]
fn handle_worktree_add_matches_git_porcelain() {
    let dir = tempfile::tempdir().expect("dir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "1\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);

    let wt_path = dir.path().join("linked");
    let repo = open_grit(dir.path());
    repo.worktrees()
        .add(&grit_lib::worktree::AddLinkedWorktreeOptions {
            path: wt_path.clone(),
            branch: Some(WorktreeBranch::Create("feat".into())),
            start_point: None,
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .expect("add");

    let porcelain = git_cmd(dir.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains(wt_path.to_str().unwrap()),
        "porcelain:\n{porcelain}"
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn handle_worktree_lock_visible_to_git() {
    let dir = tempfile::tempdir().expect("dir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "1\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    let wt_path = dir.path().join("linked");
    let repo = open_grit(dir.path());
    repo.worktrees()
        .add(&grit_lib::worktree::AddLinkedWorktreeOptions {
            path: wt_path.clone(),
            branch: Some(WorktreeBranch::Create("side".into())),
            start_point: None,
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .expect("add");
    repo.worktrees()
        .lock(
            &wt_path,
            &LockWorktreeOptions {
                reason: Some("hold".into()),
            },
        )
        .expect("lock");
    let porcelain = git_cmd(dir.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("locked") && porcelain.contains("hold"),
        "porcelain:\n{porcelain}"
    );
}

#[test]
fn handle_worktree_unlock_visible_to_git() {
    let dir = tempfile::tempdir().expect("dir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "1\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    let wt_path = dir.path().join("linked");
    let repo = open_grit(dir.path());
    repo.worktrees()
        .add(&grit_lib::worktree::AddLinkedWorktreeOptions {
            path: wt_path.clone(),
            branch: Some(WorktreeBranch::Create("side".into())),
            start_point: None,
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .expect("add");
    repo.worktrees()
        .lock(
            &wt_path,
            &LockWorktreeOptions {
                reason: Some("hold".into()),
            },
        )
        .expect("lock");
    repo.worktrees().unlock(&wt_path).expect("unlock");
    let porcelain = git_cmd(dir.path(), &["worktree", "list", "--porcelain"]);
    assert!(!porcelain.contains("locked"), "porcelain:\n{porcelain}");
}

#[test]
fn handle_fetch_push_local_bare_remote() {
    let consumer = tempfile::tempdir().expect("consumer");
    init_repo(consumer.path());
    std::fs::write(consumer.path().join("f"), "sync\n").unwrap();
    git_cmd(consumer.path(), &["add", "f"]);
    git_cmd(consumer.path(), &["commit", "-qm", "local"]);

    let bare_parent = tempfile::tempdir().expect("bare parent");
    let bare = bare_parent.path().join("upstream.git");
    git_cmd(
        bare_parent.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );

    let repo = open_grit(consumer.path());
    git_cmd(
        consumer.path(),
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    repo.reload_config().expect("reload");

    let remote = repo.remotes().get("origin").expect("get remote");
    let mut progress = NoProgress;
    let head: ObjectId = git_cmd(consumer.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    repo.remotes()
        .push(
            &remote,
            &[PushRefSpec {
                src: Some(head),
                src_ref: None,
                dst: "refs/heads/main".into(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
                ..Default::default()
            }],
            PushOptions::default(),
            &mut progress,
            None,
        )
        .expect("push");

    let clone_parent = tempfile::tempdir().expect("clone parent");
    let clone_path = clone_parent.path().join("clone");
    Command::new("git")
        .args([
            "clone",
            "-q",
            bare.to_str().unwrap(),
            clone_path.to_str().unwrap(),
        ])
        .status()
        .expect("git clone");
    git_cmd(&clone_path, &["config", "user.email", "t@example.com"]);
    git_cmd(&clone_path, &["config", "user.name", "Test"]);
    std::fs::write(clone_path.join("f"), "remote edit\n").unwrap();
    git_cmd(&clone_path, &["add", "f"]);
    git_cmd(&clone_path, &["commit", "-qm", "remote"]);
    git_cmd(&clone_path, &["push", "-q", "origin", "main"]);

    let mut fetch_opts = FetchOptions::default();
    fetch_opts.remote_name = Some("origin".into());
    fetch_opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".into()];
    repo.remotes()
        .fetch(&remote, fetch_opts, &mut progress, None)
        .expect("fetch");

    let after: ObjectId = git_cmd(consumer.path(), &["rev-parse", "refs/remotes/origin/main"])
        .trim()
        .parse()
        .unwrap();
    assert_ne!(after, head);

    let ls = repo
        .remotes()
        .ls_remote(&remote, &ListRefsOptions::default(), None)
        .expect("ls-remote");
    assert!(ls.iter().any(|r| r.name == "refs/heads/main"));
}

#[test]
fn handle_remote_list_includes_dotted_remote_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let bare = tempfile::tempdir().expect("bare");
    git_cmd(
        bare.path(),
        &["init", "-q", "--bare", "-b", "main", "upstream.git"],
    );
    let upstream = bare.path().join("upstream.git");
    git_cmd(
        dir.path(),
        &["remote", "add", "team.prod", upstream.to_str().unwrap()],
    );
    let listed = git_cmd(dir.path(), &["remote"]);
    assert!(
        listed.lines().any(|l| l.trim() == "team.prod"),
        "git remote should list team.prod:\n{listed}"
    );

    let repo = open_grit(dir.path());
    repo.reload_config().expect("reload");
    let names = repo.remotes().list().expect("list");
    assert!(
        names.iter().any(|n| n == "team.prod"),
        "handle list should include team.prod: {names:?}"
    );
}

#[test]
fn handle_notes_add_rejects_invalid_existing_notes_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "b\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    let commit: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let repo = open_grit(dir.path());
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"not a commit\n")
        .expect("blob");
    refs::write_ref(repo.git_dir(), "refs/notes/commits", &blob).expect("seed bad notes ref");

    let notes_before = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    let err = repo
        .notes()
        .add(
            &NotesRef::commits(),
            &commit,
            b"note\n",
            &NotesWriteOptions {
                message: "add".into(),
                committer: IDENT.into(),
                author: None,
            },
            false,
        )
        .expect_err("must not write on corrupt notes ref");
    assert!(
        !err.to_string().is_empty(),
        "error should describe invalid notes ref"
    );

    let notes_after = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    assert_eq!(
        notes_before.trim(),
        notes_after.trim(),
        "notes ref must be unchanged after failed add"
    );
    assert_fsck_strict(dir.path());
}
