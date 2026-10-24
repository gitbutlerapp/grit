//! `RepositoryRemotes::inspect` compared with `git remote show`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::remote::{InspectOptions, RemoteBranchStateKind};
use grit_lib::repo::Repository;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn open_repo(path: &Path) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("discover")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GitRemoteShowBranch {
    name: String,
    state: RemoteBranchStateKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitRemoteShowParsed {
    head_branch: Option<String>,
    branches: BTreeSet<GitRemoteShowBranch>,
}

fn normalize_git_branch_name(raw: &str) -> String {
    raw.trim()
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| raw.trim().strip_prefix("refs/remotes/upstream/"))
        .unwrap_or(raw.trim())
        .to_owned()
}

fn parse_git_remote_show(text: &str) -> GitRemoteShowParsed {
    let mut head_branch = None;
    let mut branches = BTreeSet::new();
    let mut in_branches = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("HEAD branch:") {
            head_branch = Some(rest.trim().to_owned());
            in_branches = false;
            continue;
        }
        if trimmed == "Remote branches:" {
            in_branches = true;
            continue;
        }
        if in_branches {
            if trimmed.is_empty() || trimmed.contains("Local branch") {
                in_branches = false;
                continue;
            }
            let (name_part, state) = if let Some(idx) = trimmed.find(" stale") {
                (&trimmed[..idx], RemoteBranchStateKind::Stale)
            } else if let Some(idx) = trimmed.find(" new") {
                (&trimmed[..idx], RemoteBranchStateKind::New)
            } else if let Some(name) = trimmed.strip_suffix(" tracked") {
                (name, RemoteBranchStateKind::Tracked)
            } else {
                continue;
            };
            let name = normalize_git_branch_name(name_part.trim());
            branches.insert(GitRemoteShowBranch { name, state });
        }
    }
    GitRemoteShowParsed {
        head_branch,
        branches,
    }
}

fn grit_branch_set(summary: &grit_lib::remote::RemoteSummary) -> BTreeSet<GitRemoteShowBranch> {
    summary
        .branches
        .iter()
        .map(|b| GitRemoteShowBranch {
            name: b.name.clone(),
            state: b.state,
        })
        .collect()
}

fn scenario_clone_then_upstream_changes() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    let clone = tmp.path().join("clone");
    std::fs::create_dir_all(&upstream).expect("upstream dir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    git(&upstream, &["commit", "--allow-empty", "-q", "-m", "init"]);
    git(&upstream, &["branch", "old1"]);
    git(
        tmp.path(),
        &["clone", "-q", upstream.to_str().unwrap(), "clone"],
    );
    git(&upstream, &["branch", "new1"]);
    git(&upstream, &["branch", "-D", "old1"]);
    (tmp, upstream, clone)
}

#[test]
fn inspect_reports_head_and_states_like_git() {
    let (_tmp, _upstream, clone) = scenario_clone_then_upstream_changes();
    let git_text = git(&clone, &["remote", "show", "origin"]);
    let expected = parse_git_remote_show(&git_text);

    let repo = open_repo(&clone);
    let summary = repo
        .remotes()
        .inspect("origin", &InspectOptions::default(), None)
        .expect("inspect");

    assert_eq!(
        expected.head_branch.as_deref(),
        Some("main"),
        "git remote show must report HEAD branch main: {git_text}"
    );
    assert_eq!(
        summary.head_branch.as_deref(),
        Some("main"),
        "grit inspect must report HEAD branch from advertisement"
    );
    assert_eq!(grit_branch_set(&summary), expected.branches);
}

#[test]
fn inspect_tracked_uses_fetch_refspec_destination() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("upstream");
    git(&upstream, &["init", "-q", "-b", "main"]);
    git(&upstream, &["commit", "--allow-empty", "-q", "-m", "init"]);
    git(&upstream, &["branch", "topic"]);
    git(
        tmp.path(),
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = tmp.path().join("client");
    git(
        &client,
        &[
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/*:refs/custom/*",
        ],
    );
    git(&client, &["fetch", "-q", "origin"]);

    let git_text = git(&client, &["remote", "show", "origin"]);
    let expected = parse_git_remote_show(&git_text);
    let repo = open_repo(&client);
    let summary = repo
        .remotes()
        .inspect("origin", &InspectOptions::default(), None)
        .expect("inspect");
    assert_eq!(grit_branch_set(&summary), expected.branches);
    assert!(
        summary
            .branches
            .iter()
            .all(|b| b.state == RemoteBranchStateKind::Tracked),
        "expected all branches tracked with custom refspec: {:?}",
        summary.branches
    );
}

#[test]
fn inspect_packed_loose_and_exact_refspec_states_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("upstream");
    git(&upstream, &["init", "-q", "-b", "main"]);
    git(&upstream, &["commit", "--allow-empty", "-q", "-m", "init"]);
    for branch in ["packed1", "packed2", "loose1", "gone", "dropped", "solo"] {
        git(&upstream, &["branch", branch]);
    }
    git(
        tmp.path(),
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = tmp.path().join("client");
    git(
        &client,
        &[
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/solo:refs/solo-track",
        ],
    );
    git(
        &client,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    git(&client, &["fetch", "-q", "origin"]);
    git(&client, &["pack-refs", "--all"]);
    git(
        &client,
        &["update-ref", "refs/remotes/origin/loose1", "HEAD"],
    );
    git(
        &client,
        &["update-ref", "-d", "refs/remotes/origin/dropped"],
    );
    git(&upstream, &["branch", "-D", "gone"]);
    git(&upstream, &["branch", "fresh"]);

    let git_text = git(&client, &["remote", "show", "origin"]);
    let expected = parse_git_remote_show(&git_text);
    let repo = open_repo(&client);
    let summary = repo
        .remotes()
        .inspect("origin", &InspectOptions::default(), None)
        .expect("inspect");
    assert_eq!(grit_branch_set(&summary), expected.branches, "{git_text}");

    let state_of = |name: &str| {
        summary
            .branches
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.state)
    };
    assert_eq!(state_of("packed1"), Some(RemoteBranchStateKind::Tracked));
    assert_eq!(state_of("loose1"), Some(RemoteBranchStateKind::Tracked));
    assert_eq!(state_of("solo"), Some(RemoteBranchStateKind::Tracked));
    assert_eq!(state_of("dropped"), Some(RemoteBranchStateKind::New));
    assert_eq!(state_of("fresh"), Some(RemoteBranchStateKind::New));
    assert_eq!(state_of("gone"), Some(RemoteBranchStateKind::Stale));
}

#[test]
fn inspect_no_query_uses_local_refs() {
    let (_tmp, _upstream, clone) = scenario_clone_then_upstream_changes();
    let repo = open_repo(&clone);
    let summary = repo
        .remotes()
        .inspect(
            "origin",
            &InspectOptions {
                query: false,
                limit: None,
            },
            None,
        )
        .expect("inspect");

    assert!(
        summary
            .branches
            .iter()
            .all(|b| b.state == RemoteBranchStateKind::Tracked),
        "without query every branch must be tracked"
    );
    assert!(
        summary.branches.iter().any(|b| b.name == "main"),
        "expected local tracking branch main"
    );
    assert!(
        summary.branches.iter().any(|b| b.name == "old1"),
        "stale local tracking ref old1 must appear as tracked without query"
    );
    assert!(
        !summary.branches.iter().any(|b| b.name == "new1"),
        "new1 must not appear without query"
    );
}

#[test]
fn inspect_unknown_remote_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let repo = open_repo(dir.path());
    let err = repo
        .remotes()
        .inspect("nosuch", &InspectOptions::default(), None)
        .expect_err("missing remote");
    assert!(matches!(
        err,
        grit_lib::remote::RemoteError::UnknownRemote(name) if name == "nosuch"
    ));
}

#[test]
fn inspect_pull_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-q", "-m", "c"]);
    git(
        dir.path(),
        &["remote", "add", "origin", "/tmp/upstream.git"],
    );
    git(dir.path(), &["config", "branch.main.remote", "origin"]);
    git(
        dir.path(),
        &["config", "branch.main.merge", "refs/heads/main"],
    );
    let repo = open_repo(dir.path());
    let summary = repo
        .remotes()
        .inspect(
            "origin",
            &InspectOptions {
                query: false,
                limit: None,
            },
            None,
        )
        .expect("inspect");
    assert_eq!(summary.pull.len(), 1);
    assert_eq!(summary.pull[0].local, "main");
    assert_eq!(summary.pull[0].merges, "refs/heads/main");
}
