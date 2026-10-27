//! `RepositoryRemotes::rename` vs the system `git remote rename`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::RemoteError;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use std::path::{Path, PathBuf};

mod support;

use support::require_reftable_git;

fn init_repo(root: &Path) {
    init_repo_with(root, &[]);
}

/// Like [`init_repo`], but the repository uses the reftable ref backend.
fn init_reftable_repo(root: &Path) {
    init_repo_with(root, &["--ref-format=reftable"]);
}

fn init_repo_with(root: &Path, init_args: &[&str]) {
    let mut args = vec!["init", "-q", "-b", "main"];
    args.extend_from_slice(init_args);
    args.push(".");
    git(root, &args);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "one"]);
}

fn open(path: &Path) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("open grit repo")
}

fn open_with_environment(path: &Path, env: Environment) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::with_environment(env))
        .expect("open grit repo")
}

fn bare_upstream(tmp: &Path) -> PathBuf {
    let wt = tmp.join("upstream-wt");
    std::fs::create_dir_all(&wt).unwrap();
    init_repo(&wt);
    let bare = tmp.join("upstream.git");
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    bare
}

fn twin_with_fetched_remote(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let bare = bare_upstream(tmp);
    let grit_repo = tmp.join("grit");
    let git_repo = tmp.join("git");
    std::fs::create_dir_all(&grit_repo).unwrap();
    std::fs::create_dir_all(&git_repo).unwrap();
    init_repo(&grit_repo);
    init_repo(&git_repo);
    git(
        &grit_repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(
        &git_repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&grit_repo, &["fetch", "-q", "origin"]);
    git(&git_repo, &["fetch", "-q", "origin"]);
    git(&grit_repo, &["branch", "topic", "HEAD"]);
    git(&git_repo, &["branch", "topic", "HEAD"]);
    git(&grit_repo, &["config", "branch.topic.remote", "origin"]);
    git(&git_repo, &["config", "branch.topic.remote", "origin"]);
    git(
        &grit_repo,
        &["config", "branch.topic.merge", "refs/heads/main"],
    );
    git(
        &git_repo,
        &["config", "branch.topic.merge", "refs/heads/main"],
    );
    git(&grit_repo, &["config", "branch.topic.pushRemote", "origin"]);
    git(&git_repo, &["config", "branch.topic.pushRemote", "origin"]);
    git(&grit_repo, &["config", "remote.pushDefault", "origin"]);
    git(&git_repo, &["config", "remote.pushDefault", "origin"]);
    git(&grit_repo, &["pack-refs", "--all"]);
    git(&git_repo, &["pack-refs", "--all"]);
    (grit_repo, git_repo, bare)
}

fn config_list_local(dir: &Path) -> String {
    git(dir, &["config", "--list", "--local"])
}

fn git_with_global(dir: &Path, global: &Path, args: &[&str]) -> String {
    git_cmd(args)
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .in_dir(dir)
        .suc()
        .stdout
}

fn for_each_ref(dir: &Path) -> String {
    git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
        ],
    )
}

fn assert_repo_matches_git(grit_repo: &Path, git_repo: &Path, new_remote: &str) {
    assert_eq!(config_list_local(grit_repo), config_list_local(git_repo));
    assert_eq!(for_each_ref(grit_repo), for_each_ref(git_repo));
    let reflog = git(
        grit_repo,
        &["reflog", "show", &format!("refs/remotes/{new_remote}/main")],
    );
    assert!(
        !reflog.trim().is_empty(),
        "expected reflog on renamed tracking ref"
    );
    git(grit_repo, &["fsck", "--no-progress"]);
    git(grit_repo, &["fetch", "-q", new_remote]);
}

#[test]
fn rename_matches_git_twin_repo_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (grit_repo, git_repo, _bare) = twin_with_fetched_remote(tmp.path());
    open(&grit_repo)
        .remotes()
        .rename("origin", "upstream", false)
        .expect("grit rename");
    git(&git_repo, &["remote", "rename", "origin", "upstream"]);
    assert_repo_matches_git(&grit_repo, &git_repo, "upstream");
}

#[test]
fn rename_matches_git_twin_repo_reftable() {
    if !require_reftable_git() {
        eprintln!("SKIP: reftable git unavailable");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    // Both twins use reftable so their configs and refs are directly comparable.
    let grit_repo = tmp.path().join("grit");
    let git_repo = tmp.path().join("git");
    std::fs::create_dir_all(&grit_repo).unwrap();
    std::fs::create_dir_all(&git_repo).unwrap();
    init_reftable_repo(&grit_repo);
    init_reftable_repo(&git_repo);
    git(
        &grit_repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(
        &git_repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&grit_repo, &["fetch", "-q", "origin"]);
    git(&git_repo, &["fetch", "-q", "origin"]);
    git(&grit_repo, &["pack-refs", "--all"]);
    git(&git_repo, &["pack-refs", "--all"]);

    open(&grit_repo)
        .remotes()
        .rename("origin", "upstream", false)
        .expect("grit rename");
    git(&git_repo, &["remote", "rename", "origin", "upstream"]);
    assert_repo_matches_git(&grit_repo, &git_repo, "upstream");
    assert_eq!(
        RefStorageFormat::detect(open(&grit_repo).git_dir()).expect("detect"),
        RefStorageFormat::Reftable
    );
}

#[test]
fn rename_keeps_custom_refspec() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git(&repo, &["remote", "add", "origin", "/srv/up.git"]);
    git(
        &repo,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/special:refs/remotes/other/special",
        ],
    );
    let custom = git(&repo, &["config", "--get-all", "remote.origin.fetch"]);
    open(&repo)
        .remotes()
        .rename("origin", "upstream", false)
        .expect("rename");
    let after = git(&repo, &["config", "--get-all", "remote.upstream.fetch"]);
    assert!(after.contains("+refs/heads/special:refs/remotes/other/special"));
    assert!(after.contains("+refs/heads/*:refs/remotes/upstream/*"));
    assert!(custom.contains("refs/remotes/other/special"));
}

#[test]
fn rename_to_existing_typed_error() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git(&repo, &["remote", "add", "origin", "/a.git"]);
    git(&repo, &["remote", "add", "other", "/b.git"]);
    assert!(matches!(
        open(&repo).remotes().rename("origin", "other", false),
        Err(RemoteError::RemoteExists(name)) if name == "other"
    ));
}

#[test]
fn rename_invalid_name_typed_error() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git(&repo, &["remote", "add", "origin", "/a.git"]);
    assert!(matches!(
        open(&repo).remotes().rename("origin", "bad name", false),
        Err(RemoteError::InvalidRemoteName(name)) if name == "bad name"
    ));
}

#[test]
fn rename_rejects_remote_only_in_global_config_leaves_state() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    let global = tmp.path().join("global-config");
    std::fs::write(
        &global,
        format!(
            "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            bare.display()
        ),
    )
    .unwrap();
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git_with_global(&repo, &global, &["fetch", "-q", "origin"]);
    let refs_before = for_each_ref(&repo);
    let cfg_before = config_list_local(&repo);
    std::env::set_var("GIT_CONFIG_GLOBAL", &global);
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    let mut env = Environment::capture_process();
    env.git_config_global = Some(global.to_string_lossy().into_owned());
    env.git_config_nosystem = Some("1".into());
    assert!(matches!(
        open_with_environment(&repo, env).remotes().rename("origin", "upstream", false),
        Err(RemoteError::UnknownRemote(name)) if name == "origin"
    ));
    assert_eq!(for_each_ref(&repo), refs_before);
    assert_eq!(config_list_local(&repo), cfg_before);
    let git_err = git_cmd(&["remote", "rename", "origin", "upstream"])
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .in_dir(&repo)
        .with_status(2);
    assert!(
        git_err.stderr.contains("No such remote"),
        "git stderr: {}",
        git_err.stderr
    );
    std::env::remove_var("GIT_CONFIG_GLOBAL");
    std::env::remove_var("GIT_CONFIG_NOSYSTEM");
}

#[test]
fn rename_retargets_branch_remote_from_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global-config");
    std::fs::write(
        &global,
        "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    )
    .unwrap();
    let bare = bare_upstream(tmp.path());
    let grit_repo = tmp.path().join("grit");
    let git_repo = tmp.path().join("git");
    std::fs::create_dir_all(&grit_repo).unwrap();
    std::fs::create_dir_all(&git_repo).unwrap();
    for repo in [&grit_repo, &git_repo] {
        init_repo(repo);
        git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    }
    std::env::set_var("GIT_CONFIG_GLOBAL", &global);
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    let mut env = Environment::capture_process();
    env.git_config_global = Some(global.to_string_lossy().into_owned());
    env.git_config_nosystem = Some("1".into());
    open_with_environment(&grit_repo, env)
        .remotes()
        .rename("origin", "upstream", false)
        .expect("rename");
    git_with_global(
        &git_repo,
        &global,
        &["remote", "rename", "origin", "upstream"],
    );
    for repo in [&grit_repo, &git_repo] {
        assert_eq!(
            git_with_global(repo, &global, &["config", "--get", "branch.main.remote"]).trim(),
            "upstream"
        );
        assert_eq!(
            git_with_global(
                repo,
                &global,
                &["config", "--local", "--get", "branch.main.remote"],
            )
            .trim(),
            "upstream"
        );
    }
    std::env::remove_var("GIT_CONFIG_GLOBAL");
    std::env::remove_var("GIT_CONFIG_NOSYSTEM");
}

#[test]
fn rename_same_name_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git(&repo, &["remote", "add", "origin", "/srv/a.git"]);
    let out = open(&repo)
        .remotes()
        .rename("origin", "origin", false)
        .expect("same-name");
    assert_eq!(out.refs_renamed, 0);
    assert_eq!(git(&repo, &["remote"]).trim(), "origin");
}

#[test]
fn rename_dry_run_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let (grit_repo, _, _) = twin_with_fetched_remote(tmp.path());
    let before_cfg = config_list_local(&grit_repo);
    let before_refs = for_each_ref(&grit_repo);
    let out = open(&grit_repo)
        .remotes()
        .rename("origin", "upstream", true)
        .expect("dry-run");
    assert!(out.dry_run);
    assert!(out.refs_renamed > 0);
    assert_eq!(config_list_local(&grit_repo), before_cfg);
    assert_eq!(for_each_ref(&grit_repo), before_refs);
    assert!(git(&grit_repo, &["remote"]).contains("origin"));
}
