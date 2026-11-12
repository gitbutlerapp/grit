//! `remove_paths`, `move_paths`, and `clean_untracked` dry-run and filter options vs system `git`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::clean::{CleanOptions, CleanScope};
use grit_lib::porcelain::paths::{MoveOptions, RemoveOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn grit_repo(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_rm_preview(root: &Path, args: &[&str]) -> BTreeSet<String> {
    let mut full = vec!["rm", "-n", "-r"];
    full.extend_from_slice(args);
    git_out(root, &full)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.strip_prefix("rm ")
                .and_then(|rest| rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(l)
                .replace('\\', "/")
        })
        .collect()
}

fn git_clean_preview(root: &Path, extra: &[&str]) -> BTreeSet<String> {
    let mut args = vec!["clean", "-n"];
    args.extend_from_slice(extra);
    parse_git_clean_lines(&git_out(root, &args))
}

fn parse_git_clean_lines(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("Would remove ")
                .or_else(|| line.strip_prefix("Removing "))
        })
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect()
}

fn index_bytes(path: &Path) -> Vec<u8> {
    fs::read(path.join(".git/index")).expect("read index")
}

#[test]
fn rm_dry_run_reports_same_paths_and_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("dir")).unwrap();
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    fs::write(root.join("dir/b.txt"), b"b\n").unwrap();
    git(root, &["add", "a.txt", "dir/b.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let index_before = index_bytes(root);
    let wt_before = fs::read(root.join("a.txt")).unwrap();

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .remove_paths(
            &RemoveOptions::default()
                .with_pathspecs(vec!["a.txt".into(), "dir/".into()])
                .with_pathspec_sources(vec!["a.txt".into(), "dir/".into()])
                .with_dry_run(true),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("dry rm");

    let grit_set: BTreeSet<String> = outcome.removed.into_iter().collect();
    let git_set = git_rm_preview(root, &["a.txt", "dir/"]);
    assert_eq!(grit_set, git_set, "dry-run paths must match git rm -n");

    assert_eq!(index_bytes(root), index_before, "index must be unchanged");
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), wt_before);
    assert!(root.join("dir/b.txt").is_file());
}

#[test]
fn rm_ignore_unmatch_succeeds_on_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("keep.txt"), b"k\n").unwrap();
    git(root, &["add", "keep.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .remove_paths(
            &RemoveOptions::default()
                .with_pathspecs(vec!["missing.txt".into()])
                .with_pathspec_sources(vec!["missing.txt".into()])
                .with_ignore_unmatch(true),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("ignore unmatch");
    assert!(outcome.removed.is_empty());
    assert!(root.join("keep.txt").is_file());
}

#[test]
fn move_paths_multi_source_requires_existing_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("one.txt"), b"1\n").unwrap();
    fs::write(root.join("two.txt"), b"2\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let err = repo
        .checkout()
        .move_paths(
            &MoveOptions::new(vec!["one.txt".into(), "two.txt".into()], "missing-dir"),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("multi-source into missing dir");
    assert!(
        err.to_string().contains("not a directory"),
        "unexpected error: {err}"
    );
}

#[test]
fn move_paths_multi_source_into_dir_git_status_agrees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("dest")).unwrap();
    fs::write(root.join("one.txt"), b"1\n").unwrap();
    fs::write(root.join("two.txt"), b"2\n").unwrap();
    git(root, &["add", "one.txt", "two.txt", "dest"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    repo.checkout()
        .move_paths(
            &MoveOptions::new(vec!["one.txt".into(), "two.txt".into()], "dest"),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("mv");

    let grit_status = git_out(root, &["status", "--porcelain"]);
    let git_root = tempfile::tempdir().expect("git copy");
    git(git_root.path(), &["init", "-q", "-b", "main"]);
    fs::create_dir_all(git_root.path().join("dest")).unwrap();
    fs::write(git_root.path().join("one.txt"), b"1\n").unwrap();
    fs::write(git_root.path().join("two.txt"), b"2\n").unwrap();
    git(git_root.path(), &["add", "."]);
    git(git_root.path(), &["commit", "-q", "-m", "init"]);
    git(git_root.path(), &["mv", "one.txt", "two.txt", "dest/"]);
    let git_status = git_out(git_root.path(), &["status", "--porcelain"]);
    assert_eq!(grit_status, git_status, "index state must match git mv");
}

#[test]
fn move_paths_dry_run_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("src.txt"), b"x\n").unwrap();
    git(root, &["add", "src.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let index_before = index_bytes(root);
    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .move_paths(
            &MoveOptions::new(vec!["src.txt".into()], "dst.txt").with_dry_run(true),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("dry mv");
    assert_eq!(outcome.moves.len(), 1);
    assert_eq!(outcome.moves[0].from, "src.txt");
    assert_eq!(outcome.moves[0].to, "dst.txt");
    assert_eq!(index_bytes(root), index_before);
    assert!(root.join("src.txt").is_file());
    assert!(!root.join("dst.txt").exists());
}

#[test]
fn move_paths_validates_all_before_moving() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("dest")).unwrap();
    fs::write(root.join("good.txt"), b"g\n").unwrap();
    git(root, &["add", "good.txt", "dest"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let index_before = index_bytes(root);
    let repo = grit_repo(root);
    let err = repo
        .checkout()
        .move_paths(
            &MoveOptions::new(vec!["good.txt".into(), "missing.txt".into()], "dest"),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("untracked second source should block the batch");
    assert!(
        err.to_string().contains("not under version control"),
        "unexpected error: {err}"
    );
    assert_eq!(index_bytes(root), index_before);
    assert!(root.join("good.txt").is_file());
    assert!(!root.join("dest/good.txt").exists());
}

#[test]
#[allow(non_snake_case)]
fn clean_only_ignored_matches_git_clean_X() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), b"*.ignored\n").unwrap();
    fs::write(root.join("tracked.txt"), b"t\n").unwrap();
    fs::write(root.join("scratch.txt"), b"u\n").unwrap();
    fs::write(root.join("drop.ignored"), b"i\n").unwrap();
    git(root, &["add", ".gitignore", "tracked.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_scope(CleanScope::OnlyIgnored)
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("clean -X preview");
    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &["-X", "-d"]),
        "only-ignored preview must match git clean -nX"
    );
}

#[test]
fn clean_nested_exclude_matches_git_clean_e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("keep.txt"), b"k\n").unwrap();
    git(root, &["add", "keep.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);
    fs::create_dir_all(root.join("dir")).unwrap();
    fs::write(root.join("dir/keep.txt"), b"keep\n").unwrap();
    fs::write(root.join("dir/drop.txt"), b"drop\n").unwrap();

    let repo = grit_repo(root);
    let preview = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_excludes(vec!["dir/keep.txt".into()])
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("clean nested exclude preview");
    let grit_set: BTreeSet<String> = preview
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &["-e", "dir/keep.txt", "-d"]),
        "nested exclude preview must match git clean -nde"
    );
    assert!(!grit_set.contains("dir/keep.txt"));

    repo.checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_excludes(vec!["dir/keep.txt".into()]),
            &mut NullProgress,
        )
        .expect("clean nested exclude force");
    git(root, &["clean", "-fd", "-e", "dir/keep.txt"]);
    assert!(root.join("dir/keep.txt").is_file());
    assert!(!root.join("dir/drop.txt").exists());
}

#[test]
fn clean_exclude_pattern_matches_git_clean_e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("keep.txt"), b"k\n").unwrap();
    fs::write(root.join("drop.txt"), b"d\n").unwrap();
    git(root, &["add", "keep.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_excludes(vec!["drop.txt".into()])
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("clean -e preview");
    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &["-e", "drop.txt", "-d"]),
        "exclude preview must match git clean -ne"
    );
    assert!(!grit_set.contains("drop.txt"));
}

#[test]
fn move_paths_rejects_duplicate_destinations_even_with_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("dir1")).unwrap();
    fs::create_dir_all(root.join("dir2")).unwrap();
    fs::create_dir_all(root.join("dest")).unwrap();
    fs::write(root.join("dir1/x"), b"1\n").unwrap();
    fs::write(root.join("dir2/x"), b"2\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);

    let index_before = index_bytes(root);
    let repo = grit_repo(root);
    let err = repo
        .checkout()
        .move_paths(
            &MoveOptions::new(vec!["dir1/x".into(), "dir2/x".into()], "dest").with_force(true),
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("duplicate basename targets must fail");
    assert!(
        err.to_string()
            .contains("multiple sources for the same target"),
        "unexpected error: {err}"
    );
    assert_eq!(index_bytes(root), index_before);
    assert!(root.join("dir1/x").is_file());
    assert!(root.join("dir2/x").is_file());
}

#[test]
fn clean_exclude_inside_untracked_dir_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("dir")).unwrap();
    fs::write(root.join("dir/keep.special"), b"k\n").unwrap();
    fs::write(root.join("dir/drop.tmp"), b"d\n").unwrap();
    git(root, &["commit", "-q", "--allow-empty", "-m", "empty"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_excludes(vec!["*.special".into()])
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("clean -e dir");
    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &["-e", "*.special", "-d"]),
        "exclude must apply inside collapsed untracked directories"
    );
}

#[test]
fn clean_nested_untracked_dir_without_d_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("untracked/sub")).unwrap();
    fs::write(root.join("untracked/sub/file"), b"u\n").unwrap();
    fs::create_dir_all(root.join("tracked-dir/untracked-sub")).unwrap();
    fs::write(root.join("tracked-dir/untracked-sub/file"), b"v\n").unwrap();
    fs::write(root.join("tracked-dir/tracked.txt"), b"t\n").unwrap();
    git(root, &["add", "tracked-dir/tracked.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_files_only(true)
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("files-only preview");
    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &[]),
        "must not enumerate files inside untracked directories without -d"
    );
}

#[test]
#[allow(non_snake_case)]
fn clean_only_ignored_with_exclude_matches_git_clean_Xe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), b"*.ignored\n").unwrap();
    fs::write(root.join("drop.ignored"), b"i\n").unwrap();
    fs::write(root.join("extra.special"), b"s\n").unwrap();
    git(root, &["add", ".gitignore"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_scope(CleanScope::OnlyIgnored)
                .with_excludes(vec!["*.special".into()])
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("clean -Xe preview");
    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &["-X", "-e", "*.special", "-d"]),
        "only-ignored -e patterns add removal targets"
    );
}

#[test]
fn clean_files_only_matches_git_clean_without_d() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("tracked.txt"), b"t\n").unwrap();
    fs::create_dir_all(root.join("empty_dir")).unwrap();
    fs::write(root.join("loose.txt"), b"u\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let repo = grit_repo(root);
    let outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_directories(true)
                .with_files_only(true)
                .with_dry_run(true),
            &mut NullProgress,
        )
        .expect("files-only preview");
    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| p.trim_end_matches('/').replace('\\', "/"))
        .collect();
    assert_eq!(
        grit_set,
        git_clean_preview(root, &[]),
        "files-only preview must match git clean -n without -d"
    );
}
