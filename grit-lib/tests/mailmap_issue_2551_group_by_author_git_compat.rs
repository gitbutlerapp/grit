//! Issue #2551: mailmap lines with name-only remainder or trailing comment after second pair.

use grit_lib::progress::{CancelToken, NullProgress};

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::GroupByAuthorOptions;
use grit_lib::revwalk::RevWalkListOptions;
use grit_test_support::git;

fn git_stdout(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn group_by_author_mailmap_issue_2551_matches_git_shortlog_sne() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "c@e.com"]);
    git(root, &["config", "user.name", "C"]);

    let c = |name: &str, email: &str, msg: &str| {
        git(
            root,
            &[
                "-c",
                &format!("user.name={name}"),
                "-c",
                &format!("user.email={email}"),
                "commit",
                "--allow-empty",
                "-m",
                msg,
            ],
        );
    };
    c("Rene Scharfe", "l.s.r@web.de", "one");
    c("René Scharfe", "l.s.r@web.de", "two");
    c("Phil", "philipoakley@iee.org", "three");
    c("Old Name", "old@e.com", "four");

    fs::write(
        root.join(".mailmap"),
        concat!(
            "René Scharfe <l.s.r@web.de> Rene Scharfe\n",
            "Philip Oakley <philipoakley@iee.email> <philipoakley@iee.org> # secondary <philipoakley@dunelm.org.uk>\n",
            "New Name <new@e.com> <old@e.com>\n",
        ),
    )?;

    let repo = Repository::discover(Some(root), &RepositoryOptions::default())?;
    let oids = repo.revwalk().commits(
        &["HEAD"],
        &RevWalkListOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    let table = repo.revwalk().mailmap_for_grouping(true)?;
    let groups =
        repo.revwalk()
            .group_by_author(&oids, table.as_ref(), GroupByAuthorOptions::default())?;

    let git_out = git_stdout(root, &["shortlog", "-sne", "HEAD"]);
    let git_lines: Vec<&str> = git_out
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
