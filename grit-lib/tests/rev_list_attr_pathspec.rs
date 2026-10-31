//! Path-limited `rev-list` / `rev_list` honors `:(attr:...)` like Git (issue #2356).

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "n")
        .env("GIT_AUTHOR_EMAIL", "a@b")
        .env("GIT_COMMITTER_NAME", "n")
        .env("GIT_COMMITTER_EMAIL", "a@b")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_issue_fixture() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for path in ["a/g.c", "a/b/f.c", "c/d/f.c", "A.c", "top.c"] {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, path).unwrap();
        git(root, &["add", path]);
        git(root, &["commit", "-qm", &format!("add {path}")]);
    }
    std::fs::write(root.join(".gitattributes"), "*.c myattr\n").unwrap();
    git(root, &["add", ".gitattributes"]);
    git(root, &["commit", "-qm", "attrs"]);
    dir
}

fn git_rev_list(root: &Path, spec: &str) -> Vec<String> {
    git(root, &["rev-list", "HEAD", "--", spec])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn grit_rev_list(repo: &Repository, spec: &str) -> Vec<ObjectId> {
    let opts = RevListOptions {
        paths: vec![spec.to_owned()],
        ..RevListOptions::default()
    };
    rev_list(repo, &["HEAD".into()], &[], &opts)
        .expect("rev_list")
        .commits
}

#[test]
fn rev_list_attr_pathspec_matches_git() {
    let dir = init_issue_fixture();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    let spec = ":(attr:myattr)";
    let git_ids = git_rev_list(dir.path(), spec);
    assert_eq!(git_ids.len(), 5, "git baseline: {git_ids:?}");
    let grit_ids = grit_rev_list(&repo, spec);
    assert_eq!(grit_ids.len(), git_ids.len());
    for oid in &grit_ids {
        assert!(git_ids.contains(&oid.to_string()));
    }
}

#[test]
fn rev_list_attr_pathspec_non_matching_attr_is_empty_while_unfiltered_is_not() {
    let dir = init_issue_fixture();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    assert!(grit_rev_list(&repo, ":(attr:no-such-attr)").is_empty());
    assert!(git_rev_list(dir.path(), ":(attr:no-such-attr)").is_empty());
    let all = rev_list(&repo, &["HEAD".into()], &[], &RevListOptions::default())
        .expect("all")
        .commits;
    assert!(all.len() > 5);
}

#[test]
fn rev_list_exclude_attr_pathspec_matches_git() {
    let dir = init_issue_fixture();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    let spec = ":(exclude,attr:myattr)";
    let git_ids = git_rev_list(dir.path(), spec);
    let grit_ids = grit_rev_list(&repo, spec);
    assert_eq!(grit_ids.len(), git_ids.len());
    for oid in &grit_ids {
        assert!(git_ids.contains(&oid.to_string()));
    }
}
