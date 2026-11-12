//! Round trips through the consolidated handles (`repo.index()`, `repo.maintenance()`) against
//! the system `git`: what grit writes, git reads and fscks, with the same results git gets.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::split_index::WriteSplitIndexRequest;
use grit_lib::index::{IndexLoadOptions, IndexWriteOptions, SparseIndexLoad};
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::maintenance::GcOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repository")
}

fn index_version(dir: &Path) -> u32 {
    let bytes = fs::read(dir.join(".git/index")).expect("read index");
    u32::from_be_bytes(bytes[4..8].try_into().expect("header"))
}

fn git_fsck_strict(dir: &Path) {
    git(dir, &["fsck", "--strict", "--no-progress"]);
}

/// A work tree with nested directories, an executable, a symlink and an intent-to-add file.
fn seed_worktree(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    for (path, body) in [
        ("README", "top\n"),
        ("a/one.txt", "one\n"),
        ("a/two.txt", "two\n"),
        ("a/deep/three.txt", "three\n"),
        ("b/four.txt", "four\n"),
        ("c/five.txt", "five\n"),
    ] {
        let full = dir.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, body).expect("write");
    }
    fs::write(dir.join("run.sh"), "#!/bin/sh\n").expect("write");
    git(dir, &["update-index", "--add", "--chmod=+x", "run.sh"]);
    #[cfg(unix)]
    std::os::unix::fs::symlink("README", dir.join("link")).expect("symlink");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "seed"]);
}

/// `git ls-files --debug` after grit loads and rewrites the index matches what git wrote.
fn assert_rewrite_matches(dir: &Path, write: &IndexWriteOptions<'_>) {
    let before = git(dir, &["ls-files", "--debug", "-s"]);
    let repo = open(dir);
    let mut index = repo
        .index()
        .load(&IndexLoadOptions::default())
        .expect("load");
    repo.index().write(&mut index, write).expect("write");
    let after = git(dir, &["ls-files", "--debug", "-s"]);
    assert_eq!(
        before, after,
        "git ls-files --debug changed after a grit write"
    );
    git_fsck_strict(dir);
}

#[test]
fn index_write_round_trips_every_version() {
    for version in ["2", "3", "4"] {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        seed_worktree(dir);
        if version != "2" {
            fs::write(dir.join("later.txt"), "intent\n").expect("write");
            git(dir, &["add", "-N", "later.txt"]);
        }
        git(dir, &["update-index", "--index-version", version]);
        assert_eq!(index_version(dir).to_string(), version);

        assert_rewrite_matches(dir, &IndexWriteOptions::default());
        assert_eq!(index_version(dir).to_string(), version, "version kept");
    }
}

#[test]
fn index_write_to_explicit_path_matches_default_path() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    seed_worktree(dir);
    let repo = open(dir);
    let mut index = repo
        .index()
        .load(&IndexLoadOptions::default())
        .expect("load");
    let alt = dir.join(".git/index.alt");
    repo.index()
        .write(&mut index, &IndexWriteOptions::default().with_path(&alt))
        .expect("write alt");
    let reread = repo
        .index()
        .load(&IndexLoadOptions::default().with_path(&alt))
        .expect("load alt");
    assert_eq!(reread.entries().len(), index.entries().len());

    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_INDEX_FILE", &alt)
        .args(["ls-files", "-s"])
        .output()
        .expect("git ls-files");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        git(dir, &["ls-files", "-s"])
    );
}

#[test]
fn split_index_write_is_read_by_git() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    seed_worktree(dir);
    let split = WriteSplitIndexRequest::default().with_explicit(Some(true));
    assert_rewrite_matches(dir, &IndexWriteOptions::default().with_split(split));
    let shared: Vec<_> = fs::read_dir(dir.join(".git"))
        .expect("read .git")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("sharedindex."))
        .collect();
    assert_eq!(shared.len(), 1, "one shared index file");
    let dump = git(dir, &["ls-files", "--stage"]);
    assert!(dump.contains("a/deep/three.txt"));

    // And back to a single file.
    let unsplit = WriteSplitIndexRequest::default().with_explicit(Some(false));
    assert_rewrite_matches(dir, &IndexWriteOptions::default().with_split(unsplit));
}

#[test]
fn sparse_index_load_expands_and_write_collapses_like_git() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    seed_worktree(dir);
    git(
        dir,
        &["sparse-checkout", "set", "--cone", "--sparse-index", "a"],
    );
    let git_sparse = git(dir, &["ls-files", "--sparse", "-s"]);
    assert!(git_sparse.contains("b/"), "git collapsed b/:\n{git_sparse}");

    let repo = open(dir);
    let preserved = repo
        .index()
        .load(&IndexLoadOptions::default().with_sparse(SparseIndexLoad::Preserve))
        .expect("load preserving");
    assert!(preserved.has_sparse_directory_placeholders());

    let mut expanded = repo
        .index()
        .load(&IndexLoadOptions::default())
        .expect("load expanded");
    assert!(!expanded.has_sparse_directory_placeholders());
    assert!(expanded
        .entries()
        .iter()
        .any(|e| e.path == b"b/four.txt" && e.skip_worktree()));

    repo.index()
        .write(&mut expanded, &IndexWriteOptions::default())
        .expect("write");
    assert_eq!(git(dir, &["ls-files", "--sparse", "-s"]), git_sparse);
    git_fsck_strict(dir);
}

fn count_objects(dir: &Path) -> Vec<(String, String)> {
    git(dir, &["count-objects", "-v"])
        .lines()
        .filter_map(|l| l.split_once(": "))
        .filter(|(k, _)| {
            matches!(
                *k,
                "count" | "in-pack" | "packs" | "prune-packable" | "garbage"
            )
        })
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn history_with_loose_objects(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..12 {
        fs::write(
            dir.join(format!("f{}.txt", i % 4)),
            format!("rev {i}\n").repeat(i + 1),
        )
        .expect("write");
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(dir, &["branch", "side", "HEAD~3"]);
    git(dir, &["tag", "-a", "-m", "v1", "v1", "HEAD~5"]);
}

#[test]
fn maintenance_gc_matches_git_gc_object_counts() {
    let tmp = TempDir::new().expect("tempdir");
    let ours = tmp.path().join("ours");
    let theirs = tmp.path().join("theirs");
    fs::create_dir_all(&ours).expect("mkdir");
    history_with_loose_objects(&ours);
    let status = Command::new("cp")
        .args(["-a"])
        .arg(&ours)
        .arg(&theirs)
        .status()
        .expect("cp");
    assert!(status.success());

    git(&theirs, &["gc", "--quiet"]);

    let repo = open(&ours);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let stats = repo
        .maintenance()
        .gc(
            &GcOptions::full_run(false, now),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("gc");
    assert!(stats.repack.objects_packed.is_some_and(|n| n > 0));

    git_fsck_strict(&ours);
    assert_eq!(count_objects(&ours), count_objects(&theirs));
    assert_eq!(
        git(&ours, &["rev-list", "--all", "--objects"]),
        git(&theirs, &["rev-list", "--all", "--objects"])
    );

    let report = repo
        .maintenance()
        .fsck(
            &FsckOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit fsck");
    assert!(report.ok(
        &repo.config().snapshot().expect("config"),
        &FsckOptions::default()
    ));
}

#[test]
fn maintenance_repack_all_is_read_by_git() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    history_with_loose_objects(dir);
    let before = git(dir, &["rev-list", "--all", "--objects"]);
    let repo = open(dir);
    let stats = repo
        .maintenance()
        .repack_all(
            &grit_lib::maintenance::RepackOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("repack");
    assert!(stats.loose_before > 0);
    git_fsck_strict(dir);
    let counts = count_objects(dir);
    assert!(
        counts.contains(&("count".to_owned(), "0".to_owned())),
        "{counts:?}"
    );
    assert!(
        counts.contains(&("packs".to_owned(), "1".to_owned())),
        "{counts:?}"
    );
    assert_eq!(git(dir, &["rev-list", "--all", "--objects"]), before);
}

#[test]
fn maintenance_cancel_stops_gc_before_it_changes_anything() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    history_with_loose_objects(dir);
    let before = count_objects(dir);
    let repo = open(dir);
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = repo
        .maintenance()
        .gc(&GcOptions::default(), &mut NullProgress, &cancel)
        .expect_err("cancelled gc");
    assert!(matches!(err, grit_lib::error::Error::Cancelled), "{err:?}");
    assert_eq!(count_objects(dir), before);
    assert!(
        repo.maintenance().lock_holder(0).is_none(),
        "gc.pid released"
    );
}
