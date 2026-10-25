//! Round-trip `grit reset` with system `git fsck` / `git status`.

mod support;

use std::path::Path;

use grit_lib::error::Error;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit_without_index_dwim;
use grit_lib::{objects, refs, RefStorageFormat};
use grit_test_support::git_cmd;
use support::{
    assert_porcelain_v2_matches_git, git_status_porcelain_v2_lines, grit_status_porcelain_v2_lines,
    open_grit_repo,
};
use tempfile::TempDir;

fn identity() -> grit_lib::ident::Signature {
    support::parse_reflog_signature("T <t@t> 0 +0000")
}

fn commit_file(
    repo: &Repository,
    path: &str,
    contents: &str,
    parent: Option<ObjectId>,
) -> ObjectId {
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join(path), contents).expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let oid = repo
        .odb()
        .write(objects::ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    let entry = grit_lib::index::entry_from_stat(
        &wt.join(path),
        path.as_bytes(),
        oid,
        grit_lib::index::MODE_REGULAR,
    )
    .expect("entry");
    index.add_or_replace(entry);
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = objects::CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: "T <t@t> 0 +0000".into(),
        committer: "T <t@t> 0 +0000".into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let co = repo
        .odb()
        .write(
            objects::ObjectKind::Commit,
            &objects::serialize_commit(&commit),
        )
        .expect("commit");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &co).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
    co
}

#[test]
fn mixed_reset_readable_by_git() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let c1 = commit_file(&repo, "f", "1\n", None);
    commit_file(&repo, "f", "2\n", Some(c1));

    reset_to_commit(
        &repo,
        &ResetOptions::new(c1, ResetMode::Mixed, identity(), "reset"),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset");

    git_cmd(&["fsck"]).in_dir(dir.path()).suc();
    git_cmd(&["status", "--porcelain"]).in_dir(dir.path()).suc();
}

fn init_git_repo(root: &Path) {
    git_cmd(&["init", "-b", "main"]).in_dir(root).suc();
    git_cmd(&["config", "user.email", "t@t.com"])
        .in_dir(root)
        .suc();
    git_cmd(&["config", "user.name", "T"]).in_dir(root).suc();
}

fn two_commit_history(root: &Path) -> ObjectId {
    std::fs::write(root.join("a"), "v1\n").expect("write");
    git_cmd(&["add", "a"]).in_dir(root).suc();
    git_cmd(&["commit", "-m", "c1"]).in_dir(root).suc();
    let c1_hex = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(root)
        .suc()
        .stdout
        .trim()
        .to_owned();
    std::fs::write(root.join("a"), "v2\n").expect("write");
    git_cmd(&["commit", "-am", "c2"]).in_dir(root).suc();
    ObjectId::from_hex(&c1_hex).expect("c1 oid")
}

#[test]
fn reset_keep_preserves_unrelated_local_changes_like_git() {
    let dir = TempDir::new().expect("tempdir");
    init_git_repo(dir.path());
    two_commit_history(dir.path());
    std::fs::write(dir.path().join("b"), "local\n").expect("local b");
    git_cmd(&["reset", "--keep", "HEAD~1"])
        .in_dir(dir.path())
        .suc();
    let git_lines = git_status_porcelain_v2_lines(dir.path());

    let dir2 = TempDir::new().expect("tempdir2");
    init_git_repo(dir2.path());
    two_commit_history(dir2.path());
    std::fs::write(dir2.path().join("b"), "local\n").expect("local b");
    let repo = open_grit_repo(dir2.path());
    let parent = resolve_revision_as_commit_without_index_dwim(&repo, "HEAD~1").expect("parent");
    reset_to_commit(
        &repo,
        &ResetOptions::new(
            parent,
            ResetMode::Keep,
            support::parse_reflog_signature("T <t@t.com> 0 +0000"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("keep reset");
    assert_porcelain_v2_matches_git(dir2.path(), &repo);
    let grit_lines = grit_status_porcelain_v2_lines(&repo);
    assert_eq!(grit_lines, git_lines);
}

#[test]
fn reset_keep_refuses_when_changed_file_has_local_edits() {
    let dir = TempDir::new().expect("tempdir");
    init_git_repo(dir.path());
    two_commit_history(dir.path());
    std::fs::write(dir.path().join("a"), "edited\n").expect("edit a");
    git_cmd(&["reset", "--keep", "HEAD~1"])
        .in_dir(dir.path())
        .with_status(128);

    let dir2 = TempDir::new().expect("tempdir2");
    init_git_repo(dir2.path());
    two_commit_history(dir2.path());
    std::fs::write(dir2.path().join("a"), "edited\n").expect("edit a");
    let repo = open_grit_repo(dir2.path());
    let head_before = refs::resolve_ref(repo.git_dir(), "HEAD").expect("head");
    let parent = resolve_revision_as_commit_without_index_dwim(&repo, "HEAD~1").expect("parent");
    let err = reset_to_commit(
        &repo,
        &ResetOptions::new(
            parent,
            ResetMode::Keep,
            support::parse_reflog_signature("T <t@t.com> 0 +0000"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("grit should refuse");
    assert!(
        matches!(err, Error::WouldOverwrite { .. }),
        "expected WouldOverwrite, got {err:?}"
    );
    assert_eq!(
        refs::resolve_ref(repo.git_dir(), "HEAD").expect("head"),
        head_before
    );
    let git_lines = git_status_porcelain_v2_lines(dir.path());
    let grit_lines = grit_status_porcelain_v2_lines(&repo);
    assert_eq!(grit_lines, git_lines);
}

#[test]
fn reset_dry_run_hard_lists_overwritten_paths_and_changes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let c1 = {
        commit_file(&repo, "a", "1\n", None);
        refs::resolve_ref(repo.git_dir(), "HEAD").expect("head")
    };
    commit_file(&repo, "a", "2\n", Some(c1));
    let head_before = refs::resolve_ref(repo.git_dir(), "HEAD").expect("head");
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("a"), "dirty\n").expect("dirty");

    let mut opts = ResetOptions::new(
        c1,
        ResetMode::Hard,
        support::parse_reflog_signature("T <t@t.com> 0 +0000"),
        "reset",
    );
    opts.dry_run = true;
    let outcome = reset_to_commit(
        &repo,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("dry-run");
    assert!(outcome.dry_run);
    assert!(outcome.would_overwrite.iter().any(|p| p == "a"));
    assert_eq!(
        refs::resolve_ref(repo.git_dir(), "HEAD").expect("head"),
        head_before
    );
    assert_eq!(std::fs::read(wt.join("a")).expect("disk"), b"dirty\n");
}

#[test]
fn reset_orphaned_commits_main_only_git_init() {
    let dir = TempDir::new().expect("tempdir");
    init_git_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "1").expect("write");
    git_cmd(&["add", "-A"]).in_dir(dir.path()).suc();
    git_cmd(&["commit", "-m", "c1"]).in_dir(dir.path()).suc();
    let c1_hex = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(dir.path())
        .suc()
        .stdout
        .trim()
        .to_owned();
    for v in ["2", "3"] {
        std::fs::write(dir.path().join("f.txt"), v).expect("write");
        git_cmd(&["add", "-A"]).in_dir(dir.path()).suc();
        git_cmd(&["commit", "-m", &format!("c{v}")])
            .in_dir(dir.path())
            .suc();
    }
    let c2_hex = git_cmd(&["rev-parse", "HEAD~1"])
        .in_dir(dir.path())
        .suc()
        .stdout
        .trim()
        .to_owned();
    let c3_hex = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(dir.path())
        .suc()
        .stdout
        .trim()
        .to_owned();

    let repo = open_grit_repo(dir.path());
    let c1 = resolve_revision_as_commit_without_index_dwim(&repo, "HEAD~2").expect("c1");
    assert_eq!(c1, ObjectId::from_hex(&c1_hex).expect("c1 hex"));

    let mut opts = ResetOptions::new(
        c1,
        ResetMode::Hard,
        support::parse_reflog_signature("T <t@t.com> 0 +0000"),
        "reset",
    );
    opts.dry_run = true;
    let preview = reset_to_commit(
        &repo,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("dry-run reset");
    assert_eq!(preview.orphaned_total, 2, "c2 and c3 should orphan");
    assert_eq!(preview.orphaned_commits.len(), 2);
    assert_eq!(
        preview.orphaned_commits[0].oid,
        ObjectId::from_hex(&c3_hex).expect("c3")
    );
    assert_eq!(
        preview.orphaned_commits[1].oid,
        ObjectId::from_hex(&c2_hex).expect("c2")
    );
    assert_eq!(preview.orphaned_commits[0].subject, "c3");
    assert_eq!(preview.orphaned_commits[1].subject, "c2");
}

#[test]
fn reset_orphaned_commits_excludes_commits_held_by_other_refs() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let c1 = commit_file(&repo, "f", "1\n", None);
    let c2 = commit_file(&repo, "f", "2\n", Some(c1));
    let c3 = commit_file(&repo, "f", "3\n", Some(c2));
    refs::write_ref(repo.git_dir(), "refs/heads/keep", &c2).expect("branch keep");

    let outcome = reset_to_commit(
        &repo,
        &ResetOptions::new(
            c1,
            ResetMode::Mixed,
            support::parse_reflog_signature("T <t@t.com> 0 +0000"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset");
    assert_eq!(outcome.orphaned_total, 1);
    assert_eq!(outcome.orphaned_commits.len(), 1);
    assert_eq!(outcome.orphaned_commits[0].oid, c3);
    let _ = c2;
}

#[test]
fn reset_orphan_zero_when_other_ref_holds_old_tip() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let c1 = commit_file(&repo, "f", "1\n", None);
    let c2 = commit_file(&repo, "f", "2\n", Some(c1));
    let c3 = commit_file(&repo, "f", "3\n", Some(c2));
    refs::write_ref(repo.git_dir(), "refs/heads/hold", &c3).expect("hold");

    let outcome = reset_to_commit(
        &repo,
        &ResetOptions::new(
            c1,
            ResetMode::Mixed,
            support::parse_reflog_signature("T <t@t.com> 0 +0000"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset");
    assert_eq!(outcome.orphaned_total, 0);
}

#[test]
fn reset_keep_rejects_unmerged_index_like_git() {
    let dir = TempDir::new().expect("tempdir");
    init_git_repo(dir.path());
    two_commit_history(dir.path());
    let tip = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(dir.path())
        .suc()
        .stdout
        .trim()
        .to_owned();
    git_cmd(&["checkout", "-b", "side", "HEAD~1"])
        .in_dir(dir.path())
        .suc();
    std::fs::write(dir.path().join("a"), "side\n").expect("side");
    git_cmd(&["commit", "-am", "side"]).in_dir(dir.path()).suc();
    git_cmd(&["checkout", "main"]).in_dir(dir.path()).suc();
    std::fs::write(dir.path().join("a"), "main\n").expect("main");
    git_cmd(&["commit", "-am", "main"]).in_dir(dir.path()).suc();
    git_cmd(&["merge", "side", "--no-commit", "--no-ff"])
        .in_dir(dir.path())
        .with_status(1);

    let _ = git_cmd(&["reset", "--keep", "HEAD"])
        .in_dir(dir.path())
        .with_status(128);

    let repo = open_grit_repo(dir.path());
    let head = refs::resolve_ref(repo.git_dir(), "HEAD").expect("head");
    let err = reset_to_commit(
        &repo,
        &ResetOptions::new(
            head,
            ResetMode::Keep,
            support::parse_reflog_signature("T <t@t.com> 0 +0000"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("grit keep should refuse unmerged index");
    assert!(matches!(err, Error::IndexUnmerged));
    let _ = tip;
}

#[test]
fn reset_forward_has_no_orphan_walk() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let c1 = commit_file(&repo, "f", "1\n", None);
    let c2 = commit_file(&repo, "f", "2\n", Some(c1));
    let outcome = reset_to_commit(
        &repo,
        &ResetOptions::new(
            c2,
            ResetMode::Mixed,
            support::parse_reflog_signature("T <t@t.com> 0 +0000"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset");
    assert_eq!(outcome.orphaned_total, 0);
    assert!(outcome.orphaned_commits.is_empty());
}

#[test]
fn append_reflog_rejects_malformed_identity() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let zero = ObjectId::zero();
    let tip = commit_file(&repo, "f.txt", "x\n", None);
    for bad in ["garbage", "Evil <a@b> 1 +0000\nfoo"] {
        let err = refs::append_reflog(
            repo.git_dir(),
            "refs/heads/main",
            &zero,
            &tip,
            bad,
            "reset: test",
            true,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::InvalidSignature(_)),
            "expected InvalidSignature for {bad:?}, got {err:?}"
        );
    }
}
