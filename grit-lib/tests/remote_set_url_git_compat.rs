//! `RepositoryRemotes::set_url` and verbose remote metadata vs the system `git`.

use std::path::Path;
use std::process::Command;

use grit_lib::remote::RemoteError;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::RemoteUrlKind;
use grit_test_support::git;

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "one"]);
}

fn open(path: &Path) -> Repository {
    Repository::discover(Some(path), &Default::default()).expect("open grit repo")
}

fn twin_repos() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let grit_repo = tmp.path().join("grit");
    let git_repo = tmp.path().join("git");
    std::fs::create_dir_all(&grit_repo).unwrap();
    std::fs::create_dir_all(&git_repo).unwrap();
    init_repo(&grit_repo);
    init_repo(&git_repo);
    git(&grit_repo, &["remote", "add", "origin", "/srv/up.git"]);
    git(&git_repo, &["remote", "add", "origin", "/srv/up.git"]);
    (tmp, grit_repo, git_repo)
}

fn config_file(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(".git/config")).expect("config")
}

#[test]
fn set_url_fetch_matches_git() {
    let (_tmp, grit_repo, git_repo) = twin_repos();
    let new_url = "https://example.com/other.git";

    git(&git_repo, &["remote", "set-url", "origin", new_url]);
    open(&grit_repo)
        .remotes()
        .set_url("origin", new_url, RemoteUrlKind::Fetch, false)
        .expect("set_url fetch");

    assert_eq!(config_file(&grit_repo), config_file(&git_repo));
    assert_eq!(
        git(&grit_repo, &["remote", "-v"]).trim(),
        git(&git_repo, &["remote", "-v"]).trim()
    );
}

#[test]
fn set_url_push_matches_git() {
    let (_tmp, grit_repo, git_repo) = twin_repos();
    let push_url = "/srv/push.git";

    git(
        &git_repo,
        &["remote", "set-url", "--push", "origin", push_url],
    );
    open(&grit_repo)
        .remotes()
        .set_url("origin", push_url, RemoteUrlKind::Push, false)
        .expect("set_url push");

    assert_eq!(config_file(&grit_repo), config_file(&git_repo));
    assert_eq!(
        git(&grit_repo, &["remote", "-v"]).trim(),
        git(&git_repo, &["remote", "-v"]).trim()
    );
}

#[test]
fn set_url_push_replaces_all_pushurls_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git(&repo, &["remote", "add", "origin", "/fetch.git"]);
    git(
        &repo,
        &["config", "--add", "remote.origin.pushurl", "/push-a.git"],
    );
    git(
        &repo,
        &["config", "--add", "remote.origin.pushurl", "/push-b.git"],
    );

    let grit = open(&repo);
    grit.remotes()
        .set_url("origin", "/push-only.git", RemoteUrlKind::Push, false)
        .expect("set push");
    assert_eq!(
        git(&repo, &["config", "--get-all", "remote.origin.pushurl"]).trim(),
        "/push-only.git"
    );
}

#[test]
fn set_url_unknown_remote_typed_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    let repo = open(tmp.path());
    assert!(matches!(
        repo.remotes()
            .set_url("missing", "x", RemoteUrlKind::Fetch, false),
        Err(RemoteError::UnknownRemote(name)) if name == "missing"
    ));
}

#[test]
fn set_url_dry_run_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "/old.git"]);
    let before = config_file(tmp.path());
    let repo = open(tmp.path());
    let change = repo
        .remotes()
        .set_url("origin", "/new.git", RemoteUrlKind::Fetch, true)
        .expect("dry run");
    assert_eq!(change.old.as_deref(), Some("/old.git"));
    assert_eq!(change.new, "/new.git");
    assert_eq!(config_file(tmp.path()), before);
    assert_eq!(
        repo.config()
            .expect("config")
            .get("remote.origin.url")
            .as_deref(),
        Some("/old.git")
    );
}

#[test]
fn configured_remotes_reports_push_urls_and_refspecs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "gh:org/repo.git"]);
    git(
        tmp.path(),
        &["config", "remote.origin.pushurl", "gh:org/push.git"],
    );
    git(
        tmp.path(),
        &["config", "url.https://github.com/.insteadOf", "gh:"],
    );

    let configured = open(tmp.path())
        .remotes()
        .configured()
        .expect("configured")
        .into_iter()
        .find(|r| r.name == "origin")
        .expect("origin");
    assert_eq!(configured.url.as_deref(), Some("gh:org/repo.git"));
    assert_eq!(configured.push_urls, vec!["gh:org/push.git".to_owned()]);
    assert_eq!(
        configured.fetch_refspecs,
        vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()]
    );
    assert_eq!(
        configured.effective_fetch_url.as_deref(),
        Some("https://github.com/org/repo.git")
    );
    assert_eq!(
        configured.effective_push_url.as_deref(),
        Some("https://github.com/org/push.git")
    );
}

#[test]
fn push_after_set_url_uses_new_pushurl() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare.git");
    let seed = tmp.path().join("seed");
    let client = tmp.path().join("client");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::create_dir_all(&client).unwrap();
    git(&seed, &["init", "-q", "-b", "main", "."]);
    git(&seed, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );
    init_repo(&client);
    git(
        &client,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );

    let alt_bare = tmp.path().join("alt.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", alt_bare.to_str().unwrap()],
    );

    open(&client)
        .remotes()
        .set_url(
            "origin",
            alt_bare.to_str().unwrap(),
            RemoteUrlKind::Push,
            false,
        )
        .expect("set push url");

    let client_repo = open(&client);
    let main_oid =
        grit_lib::refs::resolve_ref(client_repo.git_dir(), "refs/heads/main").expect("main oid");
    let remote =
        grit_lib::remote::Remote::from_config(&client_repo.config().expect("config"), "origin")
            .expect("remote");
    client_repo
        .remotes()
        .push(
            &remote,
            &[grit_lib::remote::transfer::PushRefSpec {
                src: Some(main_oid),
                src_ref: Some("refs/heads/main".to_owned()),
                dst: "refs/heads/main".to_owned(),
                force: false,
                ..Default::default()
            }],
            grit_lib::remote::transfer::PushOptions::default(),
            &mut grit_lib::remote::fetch::NoProgress,
            None,
        )
        .expect("push");

    assert_eq!(
        git(alt_bare.as_path(), &["rev-parse", "main"]).trim(),
        git(client.as_path(), &["rev-parse", "main"]).trim()
    );
    assert!(!Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(&bare)
        .output()
        .expect("spawn")
        .status
        .success());
}
