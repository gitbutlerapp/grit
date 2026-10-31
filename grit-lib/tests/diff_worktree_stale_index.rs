//! Index-vs-worktree diffs when every entry's stat data is stale, so each tracked file is
//! hashed: the parallel pass must report exactly the paths system git does, in index order.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 git output")
}

/// Commit enough files to take the parallel stat path, then give every file a new mtime and
/// edit a few: two plain files, one `eol=crlf` file rewritten with only its line endings
/// changed (clean after conversion), and one `eol=crlf` file with a real change.
fn stale_index_repo(root: &TempDir) -> std::path::PathBuf {
    let repo = root.path().join("rp");
    fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "a"]);
    git(&repo, &["config", "user.email", "a@b"]);
    fs::write(repo.join(".gitattributes"), "*.crlf text eol=crlf\n").expect("attrs");
    let files = grit_lib::PARALLEL_STAT_MIN_ENTRIES * 2;
    for i in 0..files {
        let dir = repo.join(format!("d{:02}", i % 16));
        fs::create_dir_all(&dir).expect("mkdir dir");
        fs::write(dir.join(format!("f{i:05}.txt")), format!("line {i}\n")).expect("file");
    }
    fs::write(repo.join("d03/eol.crlf"), "one\r\ntwo\r\n").expect("crlf");
    fs::write(repo.join("d07/real.crlf"), "one\r\ntwo\r\n").expect("crlf");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "c"]);

    let later = SystemTime::now() + Duration::from_secs(120);
    for path in git(&repo, &["ls-files"]).lines() {
        let file = fs::File::options()
            .write(true)
            .open(repo.join(path))
            .expect("open tracked file");
        file.set_modified(later).expect("set mtime");
    }
    fs::write(repo.join("d00/f00000.txt"), "changed 0\n").expect("edit");
    fs::write(repo.join("d09/f00009.txt"), "line 9\nmore\n").expect("edit");
    fs::write(repo.join("d03/eol.crlf"), "one\ntwo\n").expect("eol rewrite");
    fs::write(repo.join("d07/real.crlf"), "one\nTWO\n").expect("real edit");
    repo
}

#[test]
fn stale_index_parallel_hash_reports_git_changes_in_index_order() {
    let root = TempDir::new().expect("tempdir");
    let repo = stale_index_repo(&root);
    let git_paths: Vec<String> = git(&repo, &["diff", "--name-only"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        git_paths,
        ["d00/f00000.txt", "d07/real.crlf", "d09/f00009.txt"],
        "fixture: system git sees the edits but not the line-ending rewrite"
    );

    let grit_repo = Repository::open(
        &repo.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&repo),
    )
    .expect("open");
    for threads in [1, 4] {
        let mut index = grit_repo.index_handle().load().expect("index");
        let (entries, _) = diff_index_to_worktree_with_options(
            grit_repo.odb(),
            &mut index,
            &repo,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(grit_repo.git_dir().to_path_buf()),
                stat_parallel_threads: Some(threads),
                refresh_index_stat_in_pass: false,
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("diff");
        let paths: Vec<&str> = entries.iter().map(|e| e.path()).collect();
        assert_eq!(paths, git_paths, "threads={threads}");
        for entry in &entries {
            let path = entry.path();
            let indexed = index.get(path.as_bytes(), 0).expect("indexed entry");
            assert_eq!(entry.old_oid, indexed.oid, "threads={threads} {path}");
            assert_eq!(entry.new_oid, ObjectId::zero(), "threads={threads} {path}");
            assert_eq!(
                entry.old_mode.bits(),
                indexed.mode,
                "threads={threads} {path}"
            );
        }
    }
}
