//! Commit staging on deep trees: untracked-cache fast path and no double refresh (issue #1016).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, stage_for_commit, StageMode, StageOptions};
use grit_lib::porcelain::commit::{
    commit_with_scope, commit_worktree, create_commit, CommitRequest, CommitScope, HookPolicy,
};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::{reset_worktree_metadata_probe, worktree_metadata_probe_count};
use grit_test_support::git;

fn init_repo(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

fn init_windows_like_repo(root: &std::path::Path) {
    init_repo(root);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "core.autocrlf", "true"]);
}

/// Shallow wide layout: many leaf directories with a few tracked files each.
fn write_deep_tree(root: &std::path::Path, branches: usize, depth: usize, files_per_leaf: usize) {
    for b in 0..branches {
        for d in 0..depth {
            for f in 0..files_per_leaf {
                let rel = format!("b{b:02}/d{d:02}/f{f}.txt");
                let path = root.join(&rel);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("mkdir");
                }
                fs::write(&path, format!("{rel}\n")).expect("write");
            }
        }
    }
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

#[test]
fn stage_for_commit_stages_single_edit_on_deep_tree_without_new_untracked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 8, 8, 3);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let repo = open_repo(root);
    status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("warm untracked cache");

    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n# edit\n").expect("attrs");

    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage for commit");
    assert_eq!(outcome.modified.len(), 1, "expected one staged path");
    assert_eq!(
        outcome.added.len(),
        0,
        "clean tree should not stage untracked paths"
    );
}

#[test]
fn create_commit_nothing_to_commit_without_second_stat_refresh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 4, 4, 2);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage clean tree");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    let err = create_commit(
        &repo,
        &CommitRequest {
            message: "noop".into(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: true,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
    )
    .expect_err("clean tree");
    assert!(
        matches!(err, grit_lib::error::Error::NothingToCommit { .. }),
        "expected NothingToCommit, got {err:?}"
    );
}

/// Exercises UNTR auto-threshold (4096+ index entries) with a wider tree than the smoke layout.
#[test]
fn stage_for_commit_untracked_cache_eligible_tree_stages_one_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 25, 25, 7);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    assert!(
        git(root, &["ls-files"]).lines().count() >= 4096,
        "fixture should exceed UNTR auto-cache entry threshold"
    );

    let repo = open_repo(root);
    status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("warm untracked cache");
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n# edit\n").expect("edit");

    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage");
    assert_eq!(outcome.modified.len(), 1);
    assert_eq!(outcome.added.len(), 0);
}

#[test]
fn grit_commit_on_deep_tree_leaves_clean_tree_for_git() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 6, 6, 3);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n# x\n").expect("edit");

    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "t".into(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: true,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
    )
    .expect("grit commit");

    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git should see a clean tree after grit commit, got:\n{porcelain}"
    );
    let out = Command::new("git")
        .args(["fsck", "--no-progress"])
        .current_dir(root)
        .output()
        .expect("git fsck");
    assert!(out.status.success(), "git fsck after grit commit");
}

/// Windows NTFS defaults: `core.ignorecase` + `core.autocrlf` must not force a full index diff (issue #1230).
#[test]
fn stage_for_commit_ignorecase_autocrlf_uses_tracked_scan_on_deep_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_windows_like_repo(root);
    write_deep_tree(root, 25, 25, 7);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    assert!(
        git(root, &["ls-files"]).lines().count() >= 4096,
        "fixture should exceed UNTR auto-cache entry threshold"
    );

    let repo = open_repo(root);
    status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("warm untracked cache");
    fs::write(root.join("b00/d00/f0.txt"), "edited\n").expect("edit");

    let _probe = reset_worktree_metadata_probe(root);
    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage");
    assert_eq!(outcome.modified.len(), 1);
    assert_eq!(outcome.added.len(), 0);

    let tracked = git(root, &["ls-files"]).lines().count();
    let dir_budget = 25 * 25 + 64;
    let meta_calls = worktree_metadata_probe_count();
    assert!(
        meta_calls <= tracked + dir_budget,
        "ignorecase staging must use stat-trust tracked scan, not full diff (metadata probes {meta_calls}, tracked {tracked})"
    );
}

#[test]
fn stage_for_commit_ignorecase_tracked_scan_updates_case_only_spelling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_windows_like_repo(root);
    write_deep_tree(root, 8, 8, 3);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let old = root.join("b00/d00/f0.txt");
    let new = root.join("b00/d00/F0.txt");
    fs::rename(&old, &new).expect("case-only rename");

    let repo = open_repo(root);
    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage case rename");
    assert_eq!(outcome.added.len(), 0);
    assert!(
        outcome.modified.len() >= 1,
        "case-only rename must refresh index spelling via tracked scan + icase update"
    );

    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"b00/d00/F0.txt", 0).is_some(),
        "index must store on-disk spelling after case-only rename"
    );
    assert!(
        index.get(b"b00/d00/f0.txt", 0).is_none(),
        "old spelling must not remain as a duplicate stage-0 entry"
    );
}

#[test]
fn add_update_mode_clean_tree_uses_tracked_scan_probe_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 25, 25, 7);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let repo = open_repo(root);
    let _probe = reset_worktree_metadata_probe(root);
    let outcome = stage(
        &repo,
        &StageOptions {
            mode: StageMode::Update,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("add -u noop");
    assert_eq!(outcome.total(), 0);

    let tracked = git(root, &["ls-files"]).lines().count();
    let meta_calls = worktree_metadata_probe_count();
    assert!(
        meta_calls <= tracked + 500,
        "add -u on clean deep tree must use tracked scan, not full diff (metadata {meta_calls}, tracked {tracked})"
    );
}

#[test]
fn add_explicit_paths_limits_worktree_stat_probes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 25, 25, 7);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let edited = "b00/d00/f0.txt";
    fs::write(root.join(edited), "edited\n").expect("edit");
    let repo = open_repo(root);

    let _probe = reset_worktree_metadata_probe(root);
    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: vec![edited.to_owned()],
            pathspec_sources: vec![edited.to_owned()],
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("add one path");
    assert_eq!(outcome.modified.len(), 1);

    let meta_calls = worktree_metadata_probe_count();
    assert!(
        meta_calls <= 8,
        "explicit add should stat only the named path (metadata probes {meta_calls})"
    );
}

#[test]
fn commit_staged_after_single_edit_avoids_full_tree_stat_refresh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 25, 25, 7);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("b00/d00/f0.txt"), "edited\n").expect("edit");
    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage");

    let tracked = git(root, &["ls-files"]).lines().count();
    let ident = "Test <t@example.com> 1 +0000".to_string();
    let req = CommitRequest {
        message: "t".into(),
        author: Some(ident.parse().expect("signature")),
        committer: Some(ident.parse().expect("signature")),
        hooks: HookPolicy::SkipVerify,
        ..Default::default()
    };

    let _probe = reset_worktree_metadata_probe(root);
    commit_with_scope(&repo, &req, CommitScope::Staged, &mut NullProgress).expect("commit staged");
    let meta_calls = worktree_metadata_probe_count();
    assert!(
        meta_calls <= 500,
        "commit --staged should not rescan every tracked path (metadata {meta_calls}, tracked {tracked})"
    );
}

#[test]
fn commit_worktree_allow_empty_clean_tree_single_staging_pass() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_windows_like_repo(root);
    write_deep_tree(root, 25, 25, 7);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let repo = open_repo(root);
    stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add");
    status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("warm UNTR");

    let ident = "Test <t@example.com> 1 +0000".to_string();
    let mut req = CommitRequest {
        message: "empty".into(),
        author: Some(ident.parse().expect("signature")),
        committer: Some(ident.parse().expect("signature")),
        allow_empty: true,
        sign_override: None,
        amend: false,
        assume_worktree_synced: true,
        skip_pre_commit_tree_match: true,
        staged_change_count: None,
        ..Default::default()
    };

    let _probe = reset_worktree_metadata_probe(root);
    commit_worktree(&repo, &req, &mut NullProgress).expect("allow-empty commit");

    let tracked = git(root, &["ls-files"]).lines().count();
    let meta_calls = worktree_metadata_probe_count();
    assert!(
        meta_calls <= tracked + 500,
        "clean allow-empty commit should not rescan every tracked blob twice (metadata {meta_calls}, tracked {tracked})"
    );

    req.message = "second empty".into();
    commit_worktree(&repo, &req, &mut NullProgress).expect("second allow-empty");
}

fn plain_commit_request(message: &str) -> CommitRequest {
    let ident = "Test <t@example.com> 1 +0000".to_string();
    CommitRequest {
        message: message.into(),
        author: Some(ident.parse().expect("signature")),
        committer: Some(ident.parse().expect("signature")),
        hooks: HookPolicy::SkipVerify,
        ..Default::default()
    }
}

/// Auto-stage commit with a default request (as `grit commit -m`) scans the work tree once (issue #1884).
#[test]
fn commit_auto_stage_default_request_stats_tracked_files_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 25, 25, 7);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("b00/d00/f0.txt"), "edited\n").expect("edit");
    let repo = open_repo(root);
    let tracked = git(root, &["ls-files"]).lines().count();

    let probe = reset_worktree_metadata_probe(root);
    let outcome = commit_with_scope(
        &repo,
        &plain_commit_request("one edit"),
        CommitScope::AutoStage,
        &mut NullProgress,
    )
    .expect("auto-stage commit");
    let meta_calls = worktree_metadata_probe_count();
    drop(probe);
    assert_eq!(outcome.changes, 1);
    let dir_budget = 25 * 25 + 64;
    assert!(
        meta_calls <= tracked + dir_budget,
        "auto-stage commit must stat each tracked path once (metadata probes {meta_calls}, tracked {tracked})"
    );

    let _probe = reset_worktree_metadata_probe(root);
    let mut req = plain_commit_request("empty");
    req.allow_empty = true;
    commit_with_scope(&repo, &req, CommitScope::AutoStage, &mut NullProgress)
        .expect("allow-empty auto-stage commit");
    let meta_calls = worktree_metadata_probe_count();
    assert!(
        meta_calls <= tracked + dir_budget,
        "clean allow-empty auto-stage commit must stat each tracked path once (metadata probes {meta_calls}, tracked {tracked})"
    );
}

/// Index stats recorded by the single auto-stage pass are fresh enough for Git (issue #1884).
#[test]
fn commit_auto_stage_leaves_index_stat_clean_for_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 4, 4, 3);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    // Same bytes, new stat: must be refreshed in the index without being reported as a change.
    let touched = root.join("b01/d01/f1.txt");
    let same = fs::read(&touched).expect("read");
    fs::remove_file(&touched).expect("unlink");
    fs::write(&touched, same).expect("rewrite identical");
    fs::write(root.join("b00/d00/f0.txt"), "edited\n").expect("edit");
    fs::write(root.join("new.txt"), "new\n").expect("untracked");

    let repo = open_repo(root);
    let outcome = commit_with_scope(
        &repo,
        &plain_commit_request("auto-stage"),
        CommitScope::AutoStage,
        &mut NullProgress,
    )
    .expect("auto-stage commit");
    assert_eq!(outcome.changes, 2, "files: {:?}", outcome.files);

    let diff_files = Command::new("git")
        .args(["diff-files", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git diff-files");
    assert!(
        diff_files.success(),
        "git diff-files (no refresh) must see stat-clean index entries"
    );
    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git should see a clean tree after grit commit, got:\n{porcelain}"
    );
    let out = Command::new("git")
        .args(["fsck", "--no-progress"])
        .current_dir(root)
        .output()
        .expect("git fsck");
    assert!(out.status.success(), "git fsck after grit commit");
}

/// An edit that keeps size and mtime after the commit wrote the index is still visible to Git.
#[test]
fn commit_auto_stage_racy_edit_after_commit_is_seen_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    git(root, &["config", "core.trustctime", "false"]);
    git(root, &["config", "core.checkStat", "minimal"]);
    write_deep_tree(root, 3, 3, 2);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let racy = root.join("b00/d00/f0.txt");
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    fs::write(&racy, "aaaa\n").expect("edit");
    fs::File::options()
        .write(true)
        .open(&racy)
        .and_then(|f| f.set_modified(future))
        .expect("set mtime");

    let repo = open_repo(root);
    commit_with_scope(
        &repo,
        &plain_commit_request("racy"),
        CommitScope::AutoStage,
        &mut NullProgress,
    )
    .expect("auto-stage commit");

    fs::write(&racy, "bbbb\n").expect("same-size edit");
    fs::File::options()
        .write(true)
        .open(&racy)
        .and_then(|f| f.set_modified(future))
        .expect("restore mtime");

    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.contains("b00/d00/f0.txt"),
        "git must notice the same-size, same-mtime edit, got:\n{porcelain}"
    );
}
