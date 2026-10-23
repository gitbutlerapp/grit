//! Index-vs-HEAD diff on a wide root whose cache-tree was written by the system `git`.
//!
//! Directory names have different lengths, so tree order and plain byte order of the names
//! differ. Whatever order git stored the subtrees in, the walk must report exactly what
//! `git diff --cached` reports. (Skipping via the cache-tree is asserted by the read-counting
//! unit tests in `diff::core`.)

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_tree, tree_oid_of_commit, DiffStatus};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;

const DIRS: usize = 300;

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Diff Test")
        .env("GIT_AUTHOR_EMAIL", "diff@example.com")
        .env("GIT_COMMITTER_NAME", "Diff Test")
        .env("GIT_COMMITTER_EMAIL", "diff@example.com")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

type Row = (char, String, String, String);

fn git_cached_rows(root: &Path) -> BTreeSet<Row> {
    let raw = git(
        root,
        &[
            "diff",
            "--cached",
            "--raw",
            "--no-renames",
            "--abbrev=40",
            "-z",
        ],
    );
    let mut fields = raw.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut rows = BTreeSet::new();
    while let Some(meta) = fields.next() {
        let meta = String::from_utf8_lossy(meta);
        let path = String::from_utf8_lossy(fields.next().expect("path")).into_owned();
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let status = parts[4].chars().next().expect("status");
        rows.insert((status, path, parts[2].to_owned(), parts[3].to_owned()));
    }
    rows
}

#[test]
fn staged_diff_on_wide_git_written_cache_tree_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    for d in 0..DIRS {
        write(root, &format!("d{d}/a/b/f.txt"), &format!("file {d}\n"));
        write(root, &format!("d{d}/top.txt"), &format!("top {d}\n"));
    }
    write(root, "root.txt", "root\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    // Rebuild the cache-tree so every subtree is valid before staging changes.
    git(root, &["read-tree", "HEAD"]);

    for d in [0, 7, 42, 199, DIRS - 1] {
        write(root, &format!("d{d}/a/b/f.txt"), &format!("changed {d}\n"));
    }
    write(root, "d13/a/new.txt", "new in existing dir\n");
    write(root, "zz-new/x.txt", "new dir\n");
    std::fs::remove_dir_all(root.join("d55")).expect("rm dir");
    std::fs::remove_file(root.join("d56/top.txt")).expect("rm file");
    git(root, &["add", "-A"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let index = repo.index_handle().load().expect("index");
    let cache = index.cache_tree.as_ref().expect("git wrote a cache-tree");
    assert!(
        cache.children.len() > 100,
        "expected a wide cache-tree root, got {} children",
        cache.children.len()
    );
    let head = resolve_head(repo.git_dir()).expect("head");
    let head_tree = tree_oid_of_commit(repo.odb(), head.oid().expect("commit")).expect("tree");

    let entries = diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false).expect("diff");
    let grit_rows: BTreeSet<Row> = entries
        .iter()
        .map(|e| {
            let status = match e.status {
                DiffStatus::Added => 'A',
                DiffStatus::Deleted => 'D',
                DiffStatus::Modified => 'M',
                other => panic!("unexpected status {other:?}"),
            };
            (
                status,
                e.path().to_owned(),
                e.old_oid.to_hex(),
                e.new_oid.to_hex(),
            )
        })
        .collect();

    let git_rows = git_cached_rows(root);
    assert_eq!(git_rows.len(), 5 + 2 + 3, "{git_rows:#?}");
    assert_eq!(grit_rows, git_rows);
}
