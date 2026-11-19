//! `stage` under the macOS path settings (`core.ignorecase`, `core.precomposeunicode`) on a
//! case-sensitive volume: it records the same changes as without them, respells case-only
//! renames, reports colliding tracked names in its outcome, and system `git` reads the index
//! it writes.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::IndexLoadOptions;
use grit_lib::porcelain::add::{StageMode, StageOptions, StageOutcome};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_repo(root: &Path, config: &[(&str, &str)]) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for (key, value) in config {
        git(root, &["config", key, value]);
    }
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn stage(root: &Path, mode: StageMode, dry_run: bool) -> StageOutcome {
    let opts = StageOptions {
        mode,
        dry_run,
        ..StageOptions::default()
    };
    open(root)
        .index()
        .stage(&opts, &mut NullProgress, &CancelToken::never())
        .expect("stage")
}

fn sorted_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

/// Whether `root` keeps `A` and `a` as two entries (the collision fixtures need both).
fn volume_is_case_sensitive(root: &Path) -> bool {
    let probe = root.join("CASE-PROBE");
    fs::write(&probe, b"x").expect("probe");
    let distinct = !root.join("case-probe").exists();
    fs::remove_file(&probe).expect("remove probe");
    distinct
}

#[test]
fn macos_path_settings_stage_the_same_changes_on_a_wide_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(
        root,
        &[
            ("core.ignorecase", "true"),
            ("core.precomposeunicode", "true"),
        ],
    );
    for d in 0..40 {
        let dir = root.join(format!("wide/w{d:03}"));
        fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..10 {
            fs::write(dir.join(format!("F{f:02}.txt")), format!("{d} {f}\n")).expect("write");
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let mut modified: Vec<String> = (0..40)
        .step_by(4)
        .map(|d| format!("wide/w{d:03}/F07.txt"))
        .collect();
    for path in &modified {
        fs::write(root.join(path), b"edited\n").expect("edit");
    }
    let mut added: Vec<String> = (0..5).map(|i| format!("wide/w001/new{i}.txt")).collect();
    added.push("top-new.txt".to_owned());
    for path in &added {
        fs::write(root.join(path), b"new\n").expect("new");
    }

    let mut outcome = stage(root, StageMode::All, false);
    outcome.modified.sort();
    outcome.added.sort();
    modified.sort();
    added.sort();
    assert_eq!(outcome.modified, modified);
    assert_eq!(outcome.added, added);
    assert!(outcome.removed.is_empty(), "{outcome:?}");
    assert!(outcome.worktree_case_collisions.is_empty(), "{outcome:?}");

    let expected: Vec<String> = modified
        .iter()
        .map(|p| format!("M\t{p}"))
        .chain(added.iter().map(|p| format!("A\t{p}")))
        .collect();
    assert_eq!(
        sorted_lines(&git(root, &["diff", "--cached", "--name-status"])),
        sorted_lines(&expected.join("\n"))
    );
    git(root, &["fsck", "--strict"]);
    assert_eq!(
        git(root, &["diff", "--name-only"]),
        "",
        "work tree matches the index"
    );
    assert_eq!(
        git(root, &["ls-files", "--others", "--exclude-standard"]),
        "",
        "nothing left untracked"
    );
}

#[test]
fn ignorecase_stage_respells_case_only_renames_and_git_commits_them() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    if !volume_is_case_sensitive(root) {
        return;
    }
    init_repo(root, &[("core.ignorecase", "true")]);
    for d in 0..40 {
        let dir = root.join(format!("Dir/s{d:02}"));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("a.txt"), format!("{d}\n")).expect("write");
    }
    fs::write(root.join("Readme.md"), b"readme\n").expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    fs::rename(root.join("Dir"), root.join("dir")).expect("rename dir");
    fs::rename(root.join("Readme.md"), root.join("README.md")).expect("rename file");

    let mut expected: Vec<String> = (0..40).map(|d| format!("dir/s{d:02}/a.txt")).collect();
    expected.push("README.md".to_owned());
    expected.sort();

    let mut preview = stage(root, StageMode::All, true);
    preview.modified.sort();
    assert_eq!(preview.modified, expected, "dry run lists the respellings");

    let mut outcome = stage(root, StageMode::All, false);
    outcome.modified.sort();
    assert_eq!(outcome.modified, expected);
    assert!(outcome.added.is_empty(), "{outcome:?}");
    assert_eq!(sorted_lines(&git(root, &["ls-files"])), expected);
    git(root, &["commit", "-qm", "respell"]);
    git(root, &["fsck", "--strict"]);
    assert_eq!(
        sorted_lines(&git(root, &["ls-tree", "-r", "--name-only", "HEAD"])),
        expected
    );
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn stage_outcome_lists_the_collisions_status_finds_after_staging() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    if !volume_is_case_sensitive(root) {
        return;
    }
    init_repo(root, &[]);
    fs::write(root.join("Readme.TXT"), b"UPPER\n").expect("upper");
    fs::write(root.join("readme.txt"), b"lower\n").expect("lower");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "both spellings"]);
    git(root, &["config", "core.ignorecase", "true"]);
    // One dentry is left, as a case-folding checkout would leave it.
    fs::remove_file(root.join("Readme.TXT")).expect("remove loser");
    fs::write(root.join("new.txt"), b"new\n").expect("untracked");

    for dry_run in [true, false] {
        let outcome = stage(root, StageMode::All, dry_run);
        let repo = open(root);
        let index = repo
            .index()
            .load(&IndexLoadOptions::default())
            .expect("index");
        let groups = repo
            .status()
            .worktree_case_collisions(&index)
            .expect("collisions");
        assert_eq!(
            groups.iter().map(|g| g.paths.clone()).collect::<Vec<_>>(),
            [["Readme.TXT".to_owned(), "readme.txt".to_owned()]],
            "dry run {dry_run}"
        );
        assert_eq!(
            outcome.worktree_case_collisions, groups,
            "dry run {dry_run}"
        );
        assert_eq!(outcome.added, ["new.txt"], "dry run {dry_run}: {outcome:?}");
    }
    assert_eq!(
        sorted_lines(&git(root, &["ls-files"])),
        ["Readme.TXT", "new.txt", "readme.txt"]
    );
    git(root, &["fsck", "--strict"]);
}

#[test]
fn stage_outcome_has_no_collisions_without_path_settings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    if !volume_is_case_sensitive(root) {
        return;
    }
    init_repo(root, &[]);
    fs::write(root.join("Readme.TXT"), b"UPPER\n").expect("upper");
    fs::write(root.join("readme.txt"), b"lower\n").expect("lower");
    let outcome = stage(root, StageMode::All, false);
    assert_eq!(outcome.added.len(), 2, "{outcome:?}");
    assert!(outcome.worktree_case_collisions.is_empty(), "{outcome:?}");
}
