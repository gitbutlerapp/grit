//! Bundle remotes: fetch and clone compatibility with system `git`.

use std::path::Path;
use std::process::Command;

use grit_lib::clone::{clone as clone_repo, CloneOptions};
use grit_lib::environment::Environment;
use grit_lib::fetch::NoProgress;
use grit_lib::remote::{Remote, RemoteUrl};
use grit_lib::repo::{init_repository, Repository};
use grit_lib::transfer::{FetchOptions, TagMode};

fn git_in(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn init_with_commits(dir: &Path, messages: &[&str]) {
    assert!(git_in(dir, &["init", "-q", "-b", "main", "."]));
    for msg in messages {
        std::fs::write(dir.join("file.txt"), format!("{msg}\n")).unwrap();
        assert!(git_in(dir, &["add", "file.txt"]));
        assert!(git_in(dir, &["commit", "-qm", msg]));
    }
}

#[test]
fn fetch_from_git_bundle_matches_git_fetch() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["one", "two"]);
    let bundle_path = upstream.path().join("test.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    ));

    let dest = tempfile::tempdir().expect("dest");
    init_repository(dest.path(), false, "main", None, "files").expect("init");
    assert!(git_in(
        dest.path(),
        &["commit", "--allow-empty", "-qm", "seed"],
    ));

    let remote = Remote::from_url(bundle_path.to_str().unwrap()).expect("bundle url");
    assert!(matches!(remote.fetch_url(), RemoteUrl::Bundle(_)));

    let repo = Repository::open(&dest.path().join(".git"), Some(dest.path())).unwrap();
    let mut progress = NoProgress;
    let outcome = remote
        .fetch(
            &repo,
            FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            },
            &mut progress,
            None,
        )
        .expect("fetch bundle");

    assert!(git_in(
        dest.path(),
        &[
            "fetch",
            bundle_path.to_str().unwrap(),
            "refs/heads/main:refs/remotes/git/main",
        ],
    ));

    let grit_tip = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .and_then(|u| u.new_oid)
        .expect("grit fetched main");
    let git_tip = grit_lib::refs::resolve_ref(&dest.path().join(".git"), "refs/remotes/git/main")
        .expect("git tracking ref");
    assert_eq!(grit_tip, git_tip);
}

#[test]
fn clone_from_bundle_matches_git_default_branch_when_head_not_first() {
    let upstream = tempfile::tempdir().expect("upstream");
    assert!(git_in(upstream.path(), &["init", "-q", "-b", "main", "."]));
    std::fs::write(upstream.path().join("file.txt"), "base\n").unwrap();
    assert!(git_in(upstream.path(), &["add", "file.txt"]));
    assert!(git_in(upstream.path(), &["commit", "-qm", "base"]));
    assert!(git_in(upstream.path(), &["branch", "feature"]));
    assert!(git_in(upstream.path(), &["checkout", "feature"]));
    std::fs::write(upstream.path().join("file.txt"), "feature\n").unwrap();
    assert!(git_in(upstream.path(), &["add", "file.txt"]));
    assert!(git_in(upstream.path(), &["commit", "-qm", "feature-only"]));
    assert!(git_in(upstream.path(), &["checkout", "main"]));

    let bundle_path = upstream.path().join("multi.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/feature",
            "refs/heads/main",
            "HEAD",
        ],
    ));

    let git_dest = tempfile::tempdir().expect("git dest");
    assert!(git_in(
        git_dest.path(),
        &["clone", "-q", bundle_path.to_str().unwrap(), "repo"],
    ));
    let git_repo = git_dest.path().join("repo");
    let git_branch = Command::new("git")
        .current_dir(&git_repo)
        .args(["branch", "--show-current"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git branch");
    let git_branch = String::from_utf8(git_branch.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let git_head = Command::new("git")
        .current_dir(&git_repo)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-parse");
    let git_head = String::from_utf8(git_head.stdout)
        .expect("utf8")
        .trim()
        .to_owned();

    let grit_dest = tempfile::tempdir().expect("grit dest");
    let opts = CloneOptions {
        url: bundle_path.to_str().unwrap().to_owned(),
        dest: grit_dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(bundle_path.to_str().unwrap(), grit_dest.path().to_path_buf())
    };
    let outcome = clone_repo(&opts, &mut NoProgress, None).expect("grit clone from bundle");

    assert_eq!(outcome.branch, git_branch);
    assert_eq!(outcome.checkout_oid.to_hex(), git_head);
}

#[test]
fn clone_from_bundle_passes_git_fsck() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["base"]);
    let bundle_path = upstream.path().join("clone.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    ));

    let dest = tempfile::tempdir().expect("dest");
    let opts = CloneOptions {
        url: bundle_path.to_str().unwrap().to_owned(),
        dest: dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(bundle_path.to_str().unwrap(), dest.path().to_path_buf())
    };
    clone_repo(&opts, &mut NoProgress, None).expect("clone from bundle");

    assert!(git_in(dest.path(), &["fsck"]));
}
