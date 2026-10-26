//! `RepositoryRemotes::remove` matches `git remote remove` on twin repositories.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::remote::RemoteError;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::RemoveOptions;
use grit_test_support::{git, git_cmd};
use support::{git_for_each_ref, git_interop_available, Backend};

fn git_config_local(dir: &Path) -> Vec<String> {
    git(dir, &["config", "--list", "--local"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn git_fsck(dir: &Path) {
    git_cmd(&["fsck", "--no-progress"]).in_dir(dir).suc();
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn copy_seed(seed: &Path, label: &str) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let dest = root.path().join(label);
    let seed_abs = seed.canonicalize().unwrap_or_else(|_| seed.to_path_buf());
    let status = Command::new("cp")
        .arg("-a")
        .arg(&seed_abs)
        .arg(&dest)
        .status()
        .expect("cp");
    assert!(status.success(), "cp -a seed to {label}");
    (root, dest)
}

fn build_seed(reftable: bool) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = root.path();
    if reftable {
        git(repo, &["init", "-q", "-b", "main", "--ref-format=reftable"]);
    } else {
        git(repo, &["init", "-q", "-b", "main"]);
    }
    git(
        repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    git(repo, &["remote", "add", "origin", "/tmp/unused.git"]);
    git(repo, &["remote", "add", "upstream", "/tmp/up.git"]);
    let head: ObjectId = git(repo, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    git(
        repo,
        &["update-ref", "refs/remotes/origin/main", &head.to_hex()],
    );
    git(
        repo,
        &["update-ref", "refs/remotes/origin/shared", &head.to_hex()],
    );
    git(
        repo,
        &["update-ref", "refs/remotes/upstream/shared", &head.to_hex()],
    );
    git(
        repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    git(repo, &["config", "branch.main.remote", "origin"]);
    git(repo, &["config", "branch.main.merge", "refs/heads/main"]);
    git(repo, &["config", "branch.topic.remote", "origin"]);
    git(repo, &["config", "branch.topic.merge", "refs/heads/topic"]);
    git(repo, &["config", "branch.topic.pushRemote", "origin"]);
    git(repo, &["config", "remote.pushDefault", "origin"]);
    if !reftable {
        git(repo, &["pack-refs", "--all"]);
    }
    root
}

fn assert_twin_parity(seed: &Path) {
    let (_grit_root, grit_dir) = copy_seed(seed, "grit");
    let (_git_root, git_dir) = copy_seed(seed, "git");

    open(&grit_dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("grit remove");
    git(&git_dir, &["remote", "remove", "origin"]);

    assert_eq!(
        git_config_local(&grit_dir),
        git_config_local(&git_dir),
        "config --list --local"
    );

    let grit_refs: Vec<_> = git_for_each_ref(&grit_dir, "refs/")
        .into_iter()
        .map(|(n, o)| (n, o.to_hex()))
        .collect();
    let git_refs: Vec<_> = git_for_each_ref(&git_dir, "refs/")
        .into_iter()
        .map(|(n, o)| (n, o.to_hex()))
        .collect();
    assert_eq!(grit_refs, git_refs, "for-each-ref");

    git_fsck(&grit_dir);
    git_fsck(&git_dir);
}

#[test]
fn remove_matches_git_twin_repo_files_backend() {
    let seed = build_seed(false);
    assert_twin_parity(seed.path());
}

#[test]
fn remove_matches_git_twin_repo_reftable_backend() {
    if !git_interop_available(Backend::Reftable) {
        eprintln!("skip reftable remote remove: git reftable interop unavailable");
        return;
    }
    let seed = build_seed(true);
    assert_twin_parity(seed.path());
}

#[test]
fn remove_honors_negative_fetch_refspec_twin() {
    let seed = build_seed(false);
    let repo = seed.path();
    git(
        repo,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "^refs/heads/private",
        ],
    );
    let head: ObjectId = git(repo, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    git(
        repo,
        &["update-ref", "refs/remotes/origin/private", &head.to_hex()],
    );
    assert_twin_parity(repo);
}

#[test]
fn remove_does_not_delete_non_remote_tracking_destinations() {
    let seed = build_seed(false);
    let repo = seed.path();
    git(
        repo,
        &[
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/main:refs/heads/local-danger",
        ],
    );
    let head: ObjectId = git(repo, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    git(
        repo,
        &["update-ref", "refs/heads/local-danger", &head.to_hex()],
    );

    let (_grit_root, grit_dir) = copy_seed(repo, "grit");
    let (_git_root, git_dir) = copy_seed(repo, "git");

    open(&grit_dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("grit remove");
    git(&git_dir, &["remote", "remove", "origin"]);

    assert!(
        git_cmd(&["show-ref", "--verify", "-q", "refs/heads/local-danger"])
            .in_dir(&grit_dir)
            .exec()
            .ok(),
        "grit must keep local branch ref outside refs/remotes/"
    );
    assert_eq!(
        git_for_each_ref(&grit_dir, "refs/"),
        git_for_each_ref(&git_dir, "refs/"),
        "refs must match git"
    );
}

#[test]
fn remove_clears_pushremote_branch_config() {
    let seed = build_seed(false);
    let repo = seed.path();
    let (_grit_root, grit_dir) = copy_seed(repo, "grit");
    let (_git_root, git_dir) = copy_seed(repo, "git");

    open(&grit_dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("grit remove");
    git(&git_dir, &["remote", "remove", "origin"]);

    assert!(
        !git_cmd(&["config", "--get", "branch.topic.pushRemote"])
            .in_dir(&grit_dir)
            .exec()
            .ok(),
        "branch.topic.pushRemote must be unset"
    );
    assert_eq!(git_config_local(&grit_dir), git_config_local(&git_dir));
}

#[test]
fn remove_dry_run_changes_nothing() {
    let seed = build_seed(false);
    let dir = seed.path();
    let cfg_before = git_config_local(dir);
    let refs_before = git_for_each_ref(dir, "refs/");

    open(dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: true })
        .expect("dry run");

    assert_eq!(git_config_local(dir), cfg_before);
    assert_eq!(git_for_each_ref(dir, "refs/"), refs_before);
}

#[test]
fn remove_keeps_refs_shared_with_other_remote() {
    let seed = build_seed(false);
    let dir = seed.path();
    open(dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("remove");
    assert!(
        git_cmd(&["show-ref", "--verify", "-q", "refs/remotes/upstream/shared"])
            .in_dir(dir)
            .exec()
            .ok(),
        "shared ref must remain"
    );
    assert!(
        !git_cmd(&["show-ref", "--verify", "-q", "refs/remotes/origin/main"])
            .in_dir(dir)
            .exec()
            .ok(),
        "origin-only ref must be gone"
    );
}

#[test]
fn remove_unknown_remote_typed_error() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    let err = open(tmp.path())
        .remotes()
        .remove("nosuch", RemoveOptions::default())
        .expect_err("unknown");
    assert!(matches!(err, RemoteError::UnknownRemote(name) if name == "nosuch"));
}

#[test]
fn remove_clears_push_default() {
    let seed = build_seed(false);
    let dir = seed.path();
    open(dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("remove");
    assert!(
        !git_cmd(&["config", "--get", "remote.pushDefault"])
            .in_dir(dir)
            .exec()
            .ok(),
        "remote.pushDefault must be unset"
    );
}

#[test]
fn remove_with_only_negative_fetch_does_not_invent_default_refspec() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = root.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(
        repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    git(repo, &["remote", "add", "origin", "/tmp/unused.git"]);
    git(
        repo,
        &[
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "^refs/heads/unused",
        ],
    );
    let head: ObjectId = git(repo, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    git(
        repo,
        &["update-ref", "refs/remotes/origin/main", &head.to_hex()],
    );

    let (_grit_root, grit_dir) = copy_seed(repo, "grit");
    let (_git_root, git_dir) = copy_seed(repo, "git");

    open(&grit_dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("grit remove");
    git(&git_dir, &["remote", "remove", "origin"]);

    assert_eq!(
        git_for_each_ref(&grit_dir, "refs/"),
        git_for_each_ref(&git_dir, "refs/"),
        "negative-only fetch must not apply default +refs/heads/* refspec"
    );
}

/// Directories left below the top-level `refs/<kind>` directories (relative paths, sorted).
fn ref_dirs(git_dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                let rel = path.strip_prefix(root).unwrap();
                if rel.components().count() > 2 {
                    out.push(rel.display().to_string());
                }
                walk(root, &path, out);
            }
        }
    }
    let root = git_dir.join(".git");
    let mut out = Vec::new();
    walk(&root, &root.join("refs"), &mut out);
    out.sort();
    out
}

#[test]
fn remove_many_packed_and_loose_tracking_refs_matches_git() {
    let seed = build_seed(false);
    let repo = seed.path();
    let head = git(repo, &["rev-parse", "HEAD"]).trim().to_owned();
    git(
        repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "tag",
            "-a",
            "-m",
            "annotated",
            "v1",
        ],
    );
    let tag = git(repo, &["rev-parse", "v1"]).trim().to_owned();
    let mut stdin = String::new();
    for i in 0..600 {
        stdin.push_str(&format!(
            "create refs/remotes/origin/team{}/b{i} {head}\n",
            i % 7
        ));
    }
    stdin.push_str(&format!("create refs/remotes/origin/tagged {tag}\n"));
    stdin.push_str(&format!("create refs/remotes/upstream/keep {head}\n"));
    git_cmd(&["update-ref", "--stdin"])
        .in_dir(repo)
        .stdin(stdin)
        .suc();
    git(repo, &["pack-refs", "--all"]);
    for i in (0..600).step_by(50) {
        git(
            repo,
            &[
                "update-ref",
                &format!("refs/remotes/origin/team{}/b{i}", i % 7),
                &tag,
            ],
        );
    }
    git(
        repo,
        &["update-ref", "refs/remotes/origin/loose-only", &head],
    );

    let (_grit_root, grit_dir) = copy_seed(repo, "grit");
    let (_git_root, git_dir) = copy_seed(repo, "git");
    let removal = open(&grit_dir)
        .remotes()
        .remove("origin", RemoveOptions { dry_run: false })
        .expect("grit remove");
    git(&git_dir, &["remote", "remove", "origin"]);

    let mut expected_deleted: Vec<String> = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes/origin/",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    expected_deleted.sort();
    let mut deleted = removal.refs_deleted.clone();
    deleted.sort();
    assert_eq!(deleted, expected_deleted);
    assert_eq!(
        git_for_each_ref(&grit_dir, "refs/"),
        git_for_each_ref(&git_dir, "refs/"),
        "for-each-ref"
    );
    assert_eq!(git_config_local(&grit_dir), git_config_local(&git_dir));
    assert_eq!(ref_dirs(&grit_dir), ref_dirs(&git_dir), "refs/ directories");
    let packed = std::fs::read_to_string(grit_dir.join(".git/packed-refs")).expect("packed");
    assert!(!packed.contains("refs/remotes/origin/"), "{packed}");
    assert!(packed.contains("refs/remotes/upstream/keep"), "{packed}");
    assert!(
        packed.contains(&format!("{tag} refs/tags/v1\n^{head}\n")),
        "{packed}"
    );
    git_fsck(&grit_dir);
}
