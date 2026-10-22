//! Switch overwrite errors match system `git switch` refusal sets.

use std::collections::BTreeSet;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, OverwriteReason};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_switch_refused_paths(repo: &std::path::Path, branch: &str) -> BTreeSet<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["switch", branch])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git switch");
    assert!(
        !out.status.success(),
        "git switch should refuse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    parse_git_overwrite_paths(&text)
}

fn parse_git_overwrite_paths(text: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut in_list = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.contains("would be overwritten") {
            in_list = true;
            continue;
        }
        if in_list {
            if trimmed.is_empty()
                || trimmed.starts_with("Please ")
                || trimmed.starts_with("Aborting")
            {
                break;
            }
            paths.insert(trimmed.to_owned());
        }
    }
    paths
}

fn grit_switch_refused_paths(repo: &Repository, branch: &str, oid: ObjectId) -> BTreeSet<String> {
    let err = switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: branch.to_owned(),
                oid,
            },
            dry_run: false,
        },
    )
    .expect_err("grit switch should refuse");
    match err {
        Error::WouldOverwrite { paths, .. } => paths.into_iter().collect(),
        other => panic!("expected WouldOverwrite, got {other:?}"),
    }
}

fn init_repo_with_two_branches(
    tmp: &tempfile::TempDir,
) -> (Repository, String, ObjectId, String, ObjectId) {
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    std::fs::write(tmp.path().join("shared.txt"), "base\n").expect("write");
    std::fs::write(tmp.path().join("only-main.txt"), "main\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["switch", "-q", "-c", "other"]);
    std::fs::write(tmp.path().join("shared.txt"), "other\n").expect("write");
    std::fs::write(tmp.path().join("only-other.txt"), "other-only\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "other"]);
    let other_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "other"]).trim()).expect("other oid");

    git(tmp.path(), &["switch", "-q", "main"]);
    let main_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "main"]).trim()).expect("main oid");

    let repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");

    (repo, "main".into(), main_oid, "other".into(), other_oid)
}

#[test]
fn switch_local_changes_refusal_paths_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _main, _main_oid, other, other_oid) = init_repo_with_two_branches(&tmp);

    std::fs::write(tmp.path().join("shared.txt"), "local edit\n").expect("edit");

    let git_paths = git_switch_refused_paths(tmp.path(), &other);
    let grit_paths = grit_switch_refused_paths(&repo, &other, other_oid);
    assert_eq!(git_paths, grit_paths);
    assert!(
        git_paths.contains("shared.txt"),
        "expected shared.txt in refusal set: {git_paths:?}"
    );

    let err = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other.clone(),
                oid: other_oid,
            },
            dry_run: false,
        },
    )
    .expect_err("switch");
    assert!(matches!(
        err,
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            ..
        }
    ));
}

#[test]
fn switch_untracked_refusal_paths_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _main, _main_oid, other, other_oid) = init_repo_with_two_branches(&tmp);

    std::fs::write(tmp.path().join("only-other.txt"), "untracked copy\n").expect("untracked");

    let git_paths = git_switch_refused_paths(tmp.path(), &other);
    let grit_paths = grit_switch_refused_paths(&repo, &other, other_oid);
    assert_eq!(git_paths, grit_paths);
    assert!(git_paths.contains("only-other.txt"));

    let err = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other,
                oid: other_oid,
            },
            dry_run: false,
        },
    )
    .expect_err("switch");
    assert!(matches!(
        err,
        Error::WouldOverwrite {
            reason: OverwriteReason::Untracked,
            ..
        }
    ));
}

fn dir_bytes_snapshot(root: &std::path::Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap_or_default()
}

#[test]
fn switch_dry_run_leaves_repo_unchanged_and_reports_path_count() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _main, main_oid, other, other_oid) = init_repo_with_two_branches(&tmp);

    let index_before = dir_bytes_snapshot(tmp.path(), ".git/index");
    let head_before = dir_bytes_snapshot(tmp.path(), ".git/HEAD");
    let reflog_before = dir_bytes_snapshot(tmp.path(), ".git/logs/HEAD");

    let git_count = git(tmp.path(), &["diff", "--name-only", "HEAD", "other"])
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other.clone(),
                oid: other_oid,
            },
            dry_run: true,
        },
    )
    .expect("dry-run switch");
    assert!(outcome.dry_run);
    assert_eq!(outcome.changed_paths, git_count);
    assert_eq!(outcome.commit, other_oid);

    assert_eq!(dir_bytes_snapshot(tmp.path(), ".git/index"), index_before);
    assert_eq!(dir_bytes_snapshot(tmp.path(), ".git/HEAD"), head_before);
    assert_eq!(
        dir_bytes_snapshot(tmp.path(), ".git/logs/HEAD"),
        reflog_before
    );

    // Sanity: a real switch still works afterward.
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other,
                oid: other_oid,
            },
            dry_run: false,
        },
    )
    .expect("real switch");
    let _ = main_oid;
}
