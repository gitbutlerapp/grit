//! `core.symlinks=false` keeps symlink entries as `120000` when the checkout holds a plain file
//! with the link target (issue #1813), matching `git commit -a` / `git add` / `git status`.
//!
//! Each scenario clones a git-written repository with `core.symlinks=false` (so system `git`
//! checks the link out as a regular file), applies the same edit to a git twin and a grit
//! checkout, and compares the resulting trees and index modes with system `git`.

use std::fs;
use std::path::{Path, PathBuf};

use filetime::{set_file_mtime, FileTime};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::structured::StructuredLineKind;
use grit_lib::diff::{DiffOptions, DiffStatus, DiffTarget};
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::stage_tracked::{
    stage_tracked_modifications_with_thresholds, IndexStagingThresholds,
};
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::PARALLEL_STAT_MIN_ENTRIES;
use grit_test_support::git;

/// Worktree mtime well before the index write, so entries are not racily clean.
const OLD_MTIME: i64 = 1_600_000_000;
const LINK_TARGET: &str = "a.txt";

fn commit_req(message: &str) -> CommitRequest {
    let id = "Link Test <link@example.com> 1700000000 +0000".to_owned();
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    }
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo")
}

/// Source repository with `a.txt`, a symlink entry `link -> a.txt`, and `fillers` extra files.
fn source_repo(dir: &Path, fillers: usize) -> PathBuf {
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    git(&src, &["init", "-q", "-b", "main", "."]);
    fs::write(src.join("a.txt"), "a\n").expect("write a.txt");
    for i in 0..fillers {
        let rel = src.join(format!("d{}/f{i:04}.txt", i % 5));
        fs::create_dir_all(rel.parent().expect("parent")).expect("mkdir");
        fs::write(rel, format!("filler {i}\n")).expect("write filler");
    }
    git(&src, &["add", "."]);
    let link_blob = hash_link_target(&src, LINK_TARGET);
    git(
        &src,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{link_blob},link"),
        ],
    );
    git(&src, &["commit", "-qm", "base"]);
    src
}

fn hash_link_target(repo: &Path, target: &str) -> String {
    let tmp = repo.join(".git").join("link-target.tmp");
    fs::write(&tmp, target).expect("write target");
    let oid = git(
        repo,
        &["hash-object", "-w", tmp.to_str().expect("utf-8 path")],
    );
    fs::remove_file(&tmp).expect("remove target");
    oid.trim().to_owned()
}

/// Clone `src` into `dir/name` with `core.symlinks=false`, aged so the index stat cache is trusted.
fn clone_without_symlinks(src: &Path, dir: &Path, name: &str, filemode: bool) -> PathBuf {
    let dest = dir.join(name);
    git(
        dir,
        &[
            "-c",
            "core.symlinks=false",
            "clone",
            "-q",
            src.to_str().expect("utf-8 path"),
            name,
        ],
    );
    git(&dest, &["config", "core.symlinks", "false"]);
    git(
        &dest,
        &[
            "config",
            "core.filemode",
            if filemode { "true" } else { "false" },
        ],
    );
    let link = dest.join("link");
    let meta = fs::symlink_metadata(&link).expect("link meta");
    assert!(
        meta.is_file(),
        "git must check the link out as a plain file"
    );
    assert_eq!(fs::read_to_string(&link).expect("read link"), LINK_TARGET);
    age_worktree(&dest);
    dest
}

fn age_worktree(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                set_file_mtime(&path, FileTime::from_unix_time(OLD_MTIME, 0)).expect("mtime");
            }
        }
    }
    git(root, &["update-index", "-q", "--refresh"]);
}

fn ls_tree_entry(root: &Path, path: &str) -> String {
    git(root, &["ls-tree", "HEAD", "--", path])
        .trim()
        .to_owned()
}

fn ls_files_stage(root: &Path, path: &str) -> String {
    git(root, &["ls-files", "-s", "--", path]).trim().to_owned()
}

fn head_tree(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD^{tree}"]).trim().to_owned()
}

fn assert_git_clean_and_valid(root: &Path) {
    assert_eq!(
        git(root, &["status", "--porcelain"]),
        "",
        "git status clean"
    );
    git(root, &["fsck", "--strict", "--no-progress"]);
}

fn append(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    let mut body = fs::read(&path).expect("read");
    body.extend_from_slice(text.as_bytes());
    fs::write(&path, body).expect("write");
}

fn autostage_commit(root: &Path, message: &str) -> grit_lib::porcelain::commit::CommitOutcome {
    let repo = open(root);
    commit_with_scope(
        &repo,
        &commit_req(message),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("grit commit")
}

/// `rewrite_link` rewrites the link file with identical content, so its stat cache no longer
/// matches and the content has to be compared (the state after a checkout by another tool).
fn unrelated_commit_keeps_link(filemode: bool, rewrite_link: bool) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), 0);
    let base_link = ls_tree_entry(&src, "link");
    assert!(base_link.starts_with("120000 blob "), "{base_link}");

    let twin = clone_without_symlinks(&src, tmp.path(), "git-twin", filemode);
    let grit = clone_without_symlinks(&src, tmp.path(), "grit", filemode);
    for root in [&twin, &grit] {
        append(root, "a.txt", "edit\n");
        if rewrite_link {
            fs::write(root.join("link"), LINK_TARGET).expect("rewrite link");
        }
    }
    git(&twin, &["commit", "-qam", "git edit"]);
    let outcome = autostage_commit(&grit, "only a");

    assert_eq!(
        outcome.changes, 1,
        "only a.txt changed: {:?}",
        outcome.files
    );
    assert_eq!(ls_tree_entry(&grit, "link"), base_link);
    assert_eq!(
        ls_files_stage(&grit, "link").split_whitespace().next(),
        Some("120000")
    );
    assert_eq!(
        head_tree(&grit),
        head_tree(&twin),
        "same tree as git commit -a"
    );
    assert_git_clean_and_valid(&grit);
}

#[test]
fn commit_of_unrelated_edit_leaves_link_out_of_commit() {
    unrelated_commit_keeps_link(true, false);
}

#[test]
fn commit_of_unrelated_edit_leaves_rewritten_link_out_of_commit() {
    unrelated_commit_keeps_link(true, true);
}

#[test]
fn commit_of_unrelated_edit_leaves_link_out_of_commit_with_filemode_false() {
    unrelated_commit_keeps_link(false, false);
}

#[test]
fn commit_of_unrelated_edit_leaves_rewritten_link_out_of_commit_with_filemode_false() {
    unrelated_commit_keeps_link(false, true);
}

#[test]
fn commit_of_link_content_edit_stages_new_target_as_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), 0);
    let new_blob = hash_link_target(&src, "b.txt");

    let twin = clone_without_symlinks(&src, tmp.path(), "git-twin", true);
    fs::write(twin.join("link"), "b.txt").expect("write twin link");
    git(&twin, &["commit", "-qam", "retarget"]);

    let grit = clone_without_symlinks(&src, tmp.path(), "grit", true);
    fs::write(grit.join("link"), "b.txt").expect("write grit link");
    let outcome = autostage_commit(&grit, "retarget");

    assert_eq!(outcome.changes, 1, "{:?}", outcome.files);
    assert_eq!(
        ls_tree_entry(&grit, "link"),
        format!("120000 blob {new_blob}\tlink")
    );
    assert_eq!(
        head_tree(&grit),
        head_tree(&twin),
        "same tree as git commit -a"
    );
    assert_git_clean_and_valid(&grit);
}

#[test]
fn file_over_link_is_a_typechange_when_symlinks_are_enabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), 0);

    let twin = clone_without_symlinks(&src, tmp.path(), "git-twin", true);
    git(&twin, &["config", "core.symlinks", "true"]);
    git(&twin, &["commit", "-qam", "typechange"]);
    let twin_link = ls_tree_entry(&twin, "link");
    assert!(twin_link.starts_with("100644 blob "), "git: {twin_link}");

    let grit = clone_without_symlinks(&src, tmp.path(), "grit", true);
    git(&grit, &["config", "core.symlinks", "true"]);
    let outcome = autostage_commit(&grit, "typechange");

    assert_eq!(outcome.changes, 1, "{:?}", outcome.files);
    assert_eq!(ls_tree_entry(&grit, "link"), twin_link);
    assert_eq!(head_tree(&grit), head_tree(&twin));
    git(&grit, &["fsck", "--strict", "--no-progress"]);
}

fn unstaged_status(root: &Path, threads: usize) -> Vec<(String, DiffStatus, String, String)> {
    let repo = open(root);
    let options = StatusOptions {
        untracked: UntrackedMode::No,
        stat_parallel_threads: Some(threads),
        ..StatusOptions::default()
    };
    let model = status(&repo, &options, &mut NullProgress, &NeverCancel).expect("status");
    model
        .unstaged
        .iter()
        .map(|e| {
            (
                e.path().to_owned(),
                e.status,
                e.old_mode.to_string(),
                e.new_mode.to_string(),
            )
        })
        .collect()
}

fn status_ignores_link_mode(fillers: usize, threads: usize) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), fillers);
    let root = clone_without_symlinks(&src, tmp.path(), "grit", true);
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
    assert_eq!(
        unstaged_status(&root, threads),
        Vec::new(),
        "clean checkout"
    );

    // Same size, new stat: content must be compared, not the file type.
    fs::write(root.join("link"), LINK_TARGET).expect("rewrite link");
    assert_eq!(
        unstaged_status(&root, threads),
        Vec::new(),
        "rewritten link"
    );

    fs::write(root.join("link"), "b.txt").expect("retarget link");
    assert_eq!(
        unstaged_status(&root, threads),
        vec![(
            "link".to_owned(),
            DiffStatus::Modified,
            "120000".to_owned(),
            "120000".to_owned()
        )],
        "edited link stays a symlink"
    );
}

#[test]
fn serial_status_reports_untouched_link_clean() {
    status_ignores_link_mode(0, 1);
}

#[test]
fn parallel_status_reports_untouched_link_clean() {
    status_ignores_link_mode(PARALLEL_STAT_MIN_ENTRIES + 16, 4);
}

#[test]
fn add_all_keeps_link_mode_and_matches_git_add() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), 0);

    let twin = clone_without_symlinks(&src, tmp.path(), "git-twin", true);
    fs::write(twin.join("link"), "b.txt").expect("write twin link");
    git(&twin, &["add", "-A"]);

    let grit = clone_without_symlinks(&src, tmp.path(), "grit", true);
    fs::write(grit.join("link"), "b.txt").expect("write grit link");
    let repo = open(&grit);
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec![".".to_owned()],
            pathspec_sources: vec![".".to_owned()],
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

    assert_eq!(ls_files_stage(&grit, "link"), ls_files_stage(&twin, "link"));
    assert!(ls_files_stage(&grit, "link").starts_with("120000 "));
    assert_eq!(git(&grit, &["status", "--porcelain"]), "M  link\n");
}

#[test]
fn parallel_deferred_restage_keeps_link_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), 8);
    let grit = clone_without_symlinks(&src, tmp.path(), "grit", true);
    fs::write(grit.join("link"), "b.txt").expect("retarget link");
    // Same content, fresh stat: a no-op for the index mode.
    fs::write(grit.join("d0/f0000.txt"), "filler 0\n").expect("rewrite filler");

    let repo = open(&grit);
    let summary = stage_tracked_modifications_with_thresholds(
        &repo,
        &grit,
        IndexStagingThresholds {
            large_index_restage: usize::MAX,
            parallel_tracked_restage: 0,
        },
    )
    .expect("stage tracked");
    assert!(summary.modified.contains(&b"link".to_vec()), "{summary:?}");

    let new_blob = hash_link_target(&src, "b.txt");
    assert_eq!(
        ls_files_stage(&grit, "link"),
        format!("120000 {new_blob} 0\tlink")
    );
    assert!(ls_files_stage(&grit, "d0/f0000.txt").starts_with("100644 "));
}

#[test]
fn worktree_diff_of_edited_link_is_a_symlink_patch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = source_repo(tmp.path(), 0);
    let root = clone_without_symlinks(&src, tmp.path(), "grit", true);
    fs::write(root.join("link"), "b.txt").expect("retarget link");

    let repo = open(&root);
    let mut opts = DiffOptions::default();
    opts.word_level_inline = false;
    let files = repo
        .diff()
        .files(&DiffTarget::WorktreeVsIndex, &[], &opts)
        .expect("worktree diff");
    assert_eq!(files.len(), 1, "{files:?}");
    let file = &files[0];
    assert_eq!(file.path, "link");
    assert_eq!(file.status, DiffStatus::Modified);
    assert_eq!(file.old_mode.as_deref(), Some("120000"));
    assert_eq!(file.new_mode.as_deref(), Some("120000"));
    let changed: Vec<(StructuredLineKind, String)> = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind != StructuredLineKind::Context)
        .map(|l| (l.kind, l.segments.iter().map(|s| s.text.as_str()).collect()))
        .collect();
    assert_eq!(
        changed,
        vec![
            (StructuredLineKind::Del, LINK_TARGET.to_owned()),
            (StructuredLineKind::Add, "b.txt".to_owned()),
        ]
    );

    let git_diff = git(&root, &["diff", "--", "link"]);
    assert!(
        git_diff.contains("-a.txt\n") && git_diff.contains("+b.txt\n"),
        "{git_diff}"
    );
}
