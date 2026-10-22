//! `RepositoryRemotes::{add, configured}`, `resolve_remote_argument` and `fold_peeled_refs`
//! checked against the system `git` (config round-trip, worktrees, `git ls-remote`).

use std::path::Path;
use std::process::Command;

use grit_lib::remote::fetch_request::resolve_remote_argument;
use grit_lib::remote::{fold_peeled_refs, ListRefsOptions, RemoteError};
use grit_lib::repo::Repository;
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

fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git")
        .status
        .success()
}

#[test]
fn remote_added_by_grit_is_fetchable_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    init_repo(&upstream);
    init_repo(&local);

    let repo = open(&local);
    repo.remotes()
        .add("up", upstream.to_str().unwrap())
        .expect("add remote");

    assert_eq!(git(&local, &["remote"]).trim(), "up");
    assert_eq!(
        git(&local, &["remote", "get-url", "up"]).trim(),
        upstream.to_str().unwrap()
    );
    assert_eq!(
        git(&local, &["config", "--get-all", "remote.up.fetch"]).trim(),
        "+refs/heads/*:refs/remotes/up/*"
    );
    git(&local, &["fetch", "-q", "up"]);
    assert_eq!(
        git(&local, &["rev-parse", "refs/remotes/up/main"]),
        git(&upstream, &["rev-parse", "main"])
    );
    git(&local, &["fsck", "--strict"]);
}

#[test]
fn configured_remotes_match_git_remote_listing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "/srv/origin.git"]);
    git(tmp.path(), &["remote", "add", "team.prod", "/srv/prod.git"]);
    git(
        tmp.path(),
        &[
            "config",
            "remote.half.fetch",
            "+refs/heads/*:refs/remotes/half/*",
        ],
    );

    let git_names: Vec<String> = git(tmp.path(), &["remote"])
        .lines()
        .map(str::to_owned)
        .collect();
    let configured = open(tmp.path()).remotes().configured().expect("configured");
    let grit_names: Vec<String> = configured.iter().map(|r| r.name.clone()).collect();
    assert_eq!(grit_names, git_names);
    assert_eq!(
        configured
            .iter()
            .map(|r| r.url.as_deref())
            .collect::<Vec<_>>(),
        [None, Some("/srv/origin.git"), Some("/srv/prod.git")]
    );

    let config = open(tmp.path()).config().expect("config");
    assert!(matches!(
        resolve_remote_argument(&config, "half"),
        Err(RemoteError::NoUrl)
    ));
    assert_eq!(
        resolve_remote_argument(&config, "team.prod")
            .expect("team.prod")
            .name
            .as_deref(),
        Some("team.prod")
    );
}

#[test]
fn add_refuses_what_git_refuses() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "/srv/origin.git"]);
    let repo = open(tmp.path());

    assert!(!git_succeeds(
        tmp.path(),
        &["remote", "add", "origin", "/srv/other.git"]
    ));
    assert!(matches!(
        repo.remotes().add("origin", "/srv/other.git"),
        Err(RemoteError::RemoteExists(_))
    ));

    for bad in ["bad name", "a..b", "x:y"] {
        assert!(
            !git_succeeds(tmp.path(), &["remote", "add", bad, "/srv/x.git"]),
            "git accepted {bad}"
        );
        assert!(
            matches!(
                repo.remotes().add(bad, "/srv/x.git"),
                Err(RemoteError::InvalidRemoteName(_))
            ),
            "grit accepted {bad}"
        );
    }
    assert_eq!(git(tmp.path(), &["remote"]).trim(), "origin");
}

#[test]
fn add_from_linked_worktree_writes_shared_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    init_repo(&main);
    let wt = tmp.path().join("wt");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);

    let repo = open(&wt);
    repo.remotes()
        .add("shared", "/srv/shared.git")
        .expect("add from worktree");

    assert_eq!(
        git(&main, &["remote", "get-url", "shared"]).trim(),
        "/srv/shared.git"
    );
    assert_eq!(
        git(&wt, &["remote", "get-url", "shared"]).trim(),
        "/srv/shared.git"
    );
    assert!(!main.join(".git/worktrees/wt/config").exists());
    assert_eq!(repo.remotes().list().expect("list"), vec!["shared"]);
}

#[test]
fn fold_peeled_refs_matches_git_ls_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    git(tmp.path(), &["tag", "light"]);
    git(tmp.path(), &["tag", "-a", "-m", "annotated", "v1"]);

    let ls_remote = git(tmp.path(), &["ls-remote", "."]);
    let git_lines: Vec<(&str, &str)> = ls_remote
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();

    let config = open(tmp.path()).config().expect("config");
    let remote = resolve_remote_argument(&config, tmp.path().to_str().unwrap()).expect("remote");
    let raw = remote
        .list_refs(
            None,
            &ListRefsOptions {
                peel: true,
                ..Default::default()
            },
            None,
        )
        .expect("list refs");
    let folded = fold_peeled_refs(&raw);

    let git_unpeeled: Vec<&str> = git_lines
        .iter()
        .map(|(_, name)| *name)
        .filter(|name| !name.ends_with("^{}"))
        .collect();
    let mut grit_names: Vec<&str> = folded.iter().map(|r| r.name.as_str()).collect();
    let mut git_sorted = git_unpeeled.clone();
    grit_names.sort_unstable();
    git_sorted.sort_unstable();
    assert_eq!(grit_names, git_sorted);

    for entry in &folded {
        let oid = entry.oid.to_hex();
        assert!(git_lines.contains(&(oid.as_str(), entry.name.as_str())));
        let peel_name = format!("{}^{{}}", entry.name);
        let git_peel = git_lines
            .iter()
            .find(|(_, name)| *name == peel_name)
            .map(|(oid, _)| (*oid).to_owned());
        assert_eq!(entry.peeled.map(|o| o.to_hex()), git_peel, "{}", entry.name);
    }
    assert!(folded
        .iter()
        .any(|r| r.name == "refs/tags/v1" && r.peeled.is_some()));
}
