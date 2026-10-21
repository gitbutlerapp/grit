//! Author grouping matches `git shortlog -sne` (counts, order, mailmap).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;
use grit_lib::revwalk::{group_commits_by_author, mailmap_for_grouping, GroupByAuthorOptions};
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
    let groups = group_commits_by_author(
        &repo,
        &oids,
        table.as_ref(),
        GroupByAuthorOptions::default(),
    )?;

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

fn commit_with_raw_author_bytes(root: &std::path::Path, author_value: &[u8]) -> ObjectId {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tree = git(root, &["mktree"]).trim().to_string();
    let mut body = Vec::new();
    body.extend_from_slice(b"tree ");
    body.extend_from_slice(tree.as_bytes());
    body.push(b'\n');
    body.extend_from_slice(b"author ");
    body.extend_from_slice(author_value);
    body.extend_from_slice(b" 1 +0000\ncommitter C <c@x.com> 1 +0000\n\nm\n");
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&body)
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(root, &["update-ref", "refs/heads/main", &commit]);
    commit.parse().expect("oid")
}

#[test]
fn group_by_author_accepts_non_utf8_author_header() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    let oid = commit_with_raw_author_bytes(root, b"\xFFAlice <alice@example.com>");
    let repo = Repository::discover(Some(root), &RepositoryOptions::default())?;
    let groups = group_commits_by_author(
        &repo,
        std::slice::from_ref(&oid),
        None,
        GroupByAuthorOptions { count_only: true },
    )?;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count, 1);
    assert!(groups[0].email.contains("alice@example.com"));
    Ok(())
}
