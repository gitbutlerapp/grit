//! Author grouping matches `git shortlog -sne` (counts, order, mailmap).

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;
use grit_lib::revwalk::{
    group_commits_by_author, group_commits_by_author_from_rev, mailmap_for_grouping,
    GroupByAuthorOptions,
};
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

fn git_stdout(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_shortlog_count_parity(root: &std::path::Path) -> grit_lib::error::Result<()> {
    let repo = Repository::discover(Some(root), &RepositoryOptions::default())?;
    let walk = RevWalkListOptions::default();
    let grouped = group_commits_by_author_from_rev(
        &repo,
        &["HEAD"],
        &walk,
        None,
        GroupByAuthorOptions { count_only: true },
    )?;

    let git_count: usize = git_stdout(root, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .expect("rev-list count");
    assert_eq!(
        grouped.iter().map(|g| g.count).sum::<usize>(),
        git_count,
        "total commit count must match git rev-list --count"
    );

    let git_shortlog = git_stdout(root, &["shortlog", "-sn", "HEAD"]);
    let git_lines: Vec<&str> = git_shortlog
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(grouped.len(), git_lines.len());
    for (group, git_line) in grouped.iter().zip(git_lines) {
        let expected_count: usize = git_line
            .split_whitespace()
            .next()
            .expect("count")
            .parse()
            .expect("parse count");
        assert_eq!(group.count, expected_count, "line {git_line:?}");
        let git_name = git_line
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(group.name, git_name, "line {git_line:?}");
    }
    Ok(())
}

const DELTA_FIXTURE_COMMITS: usize = 350;

fn write_similar_commit_fast_import(out: &mut Vec<u8>, commits: usize) -> std::io::Result<()> {
    let root_msg = "subject: template line for delta-friendly history";
    out.extend_from_slice(b"feature done\n");
    writeln!(out, "commit refs/heads/main")?;
    writeln!(out, "mark :1")?;
    writeln!(out, "author Author <author@example.com> 1 +0000")?;
    writeln!(out, "committer Author <author@example.com> 1 +0000")?;
    writeln!(out, "data {}", root_msg.len())?;
    writeln!(out, "{root_msg}")?;
    out.extend_from_slice(b"M 100644 inline seed.txt\n");
    out.extend_from_slice(b"data 5\nseed\n\n");
    for i in 2..=commits {
        let msg = format!(
            "subject: follow-up {i} with similar wording to encourage commit deltas"
        );
        writeln!(out, "commit refs/heads/main")?;
        writeln!(out, "mark :{i}")?;
        writeln!(out, "author Author <author@example.com> {i} +0000")?;
        writeln!(out, "committer Author <author@example.com> {i} +0000")?;
        writeln!(out, "data {}", msg.len())?;
        writeln!(out, "{msg}")?;
        writeln!(out, "from :{}", i - 1)?;
    }
    out.extend_from_slice(b"done\n");
    Ok(())
}

fn fast_import_similar_commits(root: &std::path::Path, commits: usize) {
    let mut script = Vec::new();
    write_similar_commit_fast_import(&mut script, commits).expect("fast-import script");
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&script)
        .expect("write fast-import");
    let out = child.wait_with_output().expect("wait fast-import");
    assert!(
        out.status.success(),
        "git fast-import: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn verify_pack_line_is_deltified_commit(line: &str) -> bool {
    let parts: Vec<&str> = line.split_whitespace().collect();
    // verify-pack -v: `<oid> commit <size> <in-pack-size> <offset> [<chain> <base>…]`
    parts.len() >= 7 && parts[1] == "commit"
}

fn verify_pack_has_delta_commit(root: &std::path::Path) -> bool {
    let pack_dir = root.join(".git/objects/pack");
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "pack") {
            continue;
        }
        let rel = format!(
            ".git/objects/pack/{}",
            path.file_name().unwrap().to_string_lossy()
        );
        let v = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["verify-pack", "-v"])
            .arg(&rel)
            .output()
            .expect("verify-pack");
        if !v.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&v.stdout);
        if text.lines().any(verify_pack_line_is_deltified_commit) {
            return true;
        }
    }
    false
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

/// Each commit lives in its own pack; author counting must not assume one pack holds all commits.
#[test]
fn group_by_author_count_spans_two_commit_packs() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(
        root,
        &[
            "-c",
            "user.name=One",
            "-c",
            "user.email=one@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "one",
        ],
    );
    git(root, &["repack", "-ad"]);
    git(
        root,
        &[
            "-c",
            "user.name=Two",
            "-c",
            "user.email=two@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "two",
        ],
    );
    git(root, &["repack", "-d", "-g", "1"]);

    assert_shortlog_count_parity(root)?;

    git(root, &["commit-graph", "write", "--reachable"]);
    assert_shortlog_count_parity(root)?;
    Ok(())
}

/// Deltified commits in one pack (no commit-graph, no bitmap) must still match git shortlog counts.
#[test]
fn group_by_author_count_with_deltified_commits() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    fast_import_similar_commits(root, DELTA_FIXTURE_COMMITS);
    git(root, &["repack", "-adf", "--depth=50", "--window=250"]);
    let _ = fs::remove_file(root.join("objects/info/commit-graph"));
    let _ = fs::remove_dir_all(root.join("objects/info/commit-graphs"));

    assert!(
        verify_pack_has_delta_commit(root),
        "fixture must include at least one deltified commit object"
    );
    assert_shortlog_count_parity(root)?;
    Ok(())
}

/// Commits split across two packs plus a loose object must match git shortlog counts.
#[test]
fn group_by_author_count_two_packs_and_loose() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(
        root,
        &[
            "-c",
            "user.name=Loose",
            "-c",
            "user.email=loose@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "loose-tip",
        ],
    );
    git(root, &["repack", "-ad"]);
    git(
        root,
        &[
            "-c",
            "user.name=Packed",
            "-c",
            "user.email=packed@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "packed",
        ],
    );
    git(root, &["repack", "-d", "-g", "1"]);
    git(
        root,
        &[
            "-c",
            "user.name=Loose2",
            "-c",
            "user.email=loose2@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "loose-head",
        ],
    );

    let _ = fs::remove_file(root.join("objects/info/commit-graph"));
    assert_shortlog_count_parity(root)?;
    Ok(())
}
