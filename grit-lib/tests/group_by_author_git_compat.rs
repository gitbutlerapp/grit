//! Author grouping matches `git shortlog -sne` (counts, order, mailmap).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;
use grit_lib::revwalk::{group_commits_by_author, mailmap_for_grouping};
use grit_test_support::git;

fn commit_as(root: &std::path::Path, name: &str, email: &str, msg: &str) {
    fs::write(root.join("f.txt"), format!("{msg}\n")).expect("write");
    git(
        root,
        &[
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "-am",
            msg,
        ],
    );
}

#[test]
fn group_by_author_matches_git_shortlog_sne() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "alice@example.com"]);
    git(root, &["config", "user.name", "Alice"]);
    fs::write(root.join("f.txt"), "start\n")?;
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-m", "root"]);

    commit_as(root, "Alice", "alice@example.com", "a1");
    commit_as(root, "Alice", "alice@example.com", "a2");
    commit_as(root, "Bob", "bob@example.com", "b1");
    fs::write(
        root.join(".mailmap"),
        "Mapped Alice <mapped@example.com> <alice@example.com>\n",
    )?;
    commit_as(root, "Alice", "alice@example.com", "a3");

    let repo = Repository::discover(Some(root), &RepositoryOptions::default())?;
    let oids = repo
        .rev_walk()
        .commits(&["HEAD"], &RevWalkListOptions::default())?;
    let table = mailmap_for_grouping(&repo, true)?;
    let groups = group_commits_by_author(&repo, &oids, table.as_ref())?;

    let git_out = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["shortlog", "-sne", "HEAD"])
        .output()
        .expect("git shortlog");
    assert!(git_out.status.success());
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let git_lines: Vec<&str> = git_stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    assert_eq!(groups.len(), git_lines.len());
    for (group, git_line) in groups.iter().zip(git_lines) {
        let expected_count: usize = git_line
            .split_whitespace()
            .next()
            .expect("count")
            .parse()
            .expect("parse count");
        assert_eq!(group.count, expected_count);
        let suffix = git_line
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ");
        let grit_label = if group.email.is_empty() {
            group.name.clone()
        } else {
            format!("{} <{}>", group.name, group.email)
        };
        assert_eq!(grit_label, suffix, "line {git_line:?}");
    }
    Ok(())
}
