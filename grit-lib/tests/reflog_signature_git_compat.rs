//! Reflog identity parsing vs system `git` (files and reftable backends).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::refs::reflog::{read_reflog, ReflogEntry};
use grit_lib::refs::reflog_file_path;

use support::{
    each_backend, git, git_fsck_strict, git_interop_available, null_device_for_tests,
    require_reftable_git, Backend, TestRepo, AUTHOR_EMAIL, AUTHOR_NAME,
};

fn git_with_committer_date(worktree: &Path, date: &str, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_raw_log_columns_match(repo: &TestRepo, refname: &str, entries: &[ReflogEntry]) {
    if repo.backend() != Backend::Files {
        return;
    }
    let path = reflog_file_path(&repo.git_dir(), refname);
    let raw = fs::read_to_string(path).expect("read reflog file");
    let lines: Vec<_> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), entries.len(), "reflog line count");
    for (line, entry) in lines.iter().zip(entries) {
        let before_tab = line.split('\t').next().unwrap_or(line);
        let mut parts = before_tab.splitn(3, ' ');
        let old = parts.next().expect("old oid column");
        let new = parts.next().expect("new oid column");
        assert_eq!(old, entry.old_oid.to_hex());
        assert_eq!(new, entry.new_oid.to_hex());
    }
}

fn identities_from_loose_log(repo: &TestRepo, refname: &str) -> Vec<String> {
    let path = reflog_file_path(&repo.git_dir(), refname);
    let raw = fs::read_to_string(path).expect("read reflog file");
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let before_tab = line.split('\t').next().unwrap_or(line);
            before_tab
                .splitn(3, ' ')
                .nth(2)
                .expect("identity column")
                .to_owned()
        })
        .collect()
}

fn assert_signatures_match_git(repo: &TestRepo, refname: &str, entries: &[ReflogEntry]) {
    assert_raw_log_columns_match(repo, refname, entries);
    if repo.backend() == Backend::Files {
        let identities = identities_from_loose_log(repo, refname);
        assert_eq!(identities.len(), entries.len());
        for (identity, entry) in identities.into_iter().zip(entries) {
            assert_eq!(identity, entry.identity);
            let sig = entry.signature().expect("parseable reflog identity");
            assert_eq!(
                grit_lib::ident::parse_signature(&identity),
                Some(sig.clone())
            );
        }
    }
    for entry in entries {
        let sig = entry.signature().expect("parseable reflog identity");
        assert!(
            sig.format_rfc3339().is_some(),
            "RFC 3339 date for {}",
            entry.identity
        );
    }
    let out = git(
        repo.worktree(),
        &["log", "-g", refname, "--format=%H%x00%gn%x00%ge"],
    );
    let mut git_rows: Vec<(String, String, String)> = out
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split('\0').collect();
            (parts.len() == 3).then(|| {
                (
                    parts[0].to_owned(),
                    parts[1].to_owned(),
                    parts[2].to_owned(),
                )
            })
        })
        .collect();
    git_rows.reverse();
    assert_eq!(git_rows.len(), entries.len());
    for ((new_hex, git_name, git_email), entry) in git_rows.into_iter().zip(entries) {
        assert_eq!(new_hex, entry.new_oid.to_hex());
        let sig = entry.signature().expect("parseable reflog identity");
        assert_eq!(sig.name, git_name);
        assert_eq!(sig.email, git_email);
    }
}

fn seed_git_reflog(repo: &TestRepo) {
    fs::write(repo.worktree().join("f.txt"), "one\n").expect("write");
    git_with_committer_date(repo.worktree(), "1700000000 +0530", &["add", "f.txt"]);
    git_with_committer_date(
        repo.worktree(),
        "1700000000 +0530",
        &["commit", "-m", "first"],
    );
    fs::write(repo.worktree().join("g.txt"), "two\n").expect("write");
    git_with_committer_date(repo.worktree(), "1700100000 -0800", &["add", "g.txt"]);
    git_with_committer_date(
        repo.worktree(),
        "1700100000 -0800",
        &["commit", "-m", "second"],
    );
    git_with_committer_date(
        repo.worktree(),
        "1700200000 +0000",
        &["reset", "--hard", "HEAD~1"],
    );
    git_with_committer_date(
        repo.worktree(),
        "1700300000 +0530",
        &["checkout", "-b", "side"],
    );
    git_with_committer_date(
        repo.worktree(),
        "1700400000 +0000",
        &["branch", "-m", "main", "trunk"],
    );
    let entries = read_reflog(&repo.git_dir(), "HEAD").expect("read");
    let first_commit = entries
        .iter()
        .find(|e| e.message.contains("first"))
        .expect("first commit reflog line");
    let sig = first_commit.signature().expect("signature");
    assert_eq!(
        sig.format_rfc3339().as_deref(),
        Some("2023-11-15T03:43:20+05:30")
    );
}

#[test]
fn git_writes_grit_reads_reflog_signatures() {
    each_backend(|backend, repo| {
        if backend == Backend::Reftable && !git_interop_available(backend) {
            eprintln!("SKIP: git lacks reftable interop");
            return;
        }
        seed_git_reflog(&repo);
        let entries = read_reflog(&repo.git_dir(), "HEAD").expect("read reflog");
        assert!(entries.len() >= 4, "expected several reflog entries");
        assert_signatures_match_git(&repo, "HEAD", &entries);
    });
}

fn grit_bin() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_grit") {
        return Some(std::path::PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("GRIT_BIN") {
        return Some(std::path::PathBuf::from(p));
    }
    let debug = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/grit");
    debug.is_file().then_some(debug)
}

fn run_grit(worktree: &Path, grit: &Path, args: &[&str]) {
    let out = Command::new(grit)
        .current_dir(worktree)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit_writes_git_reads() {
    let Some(grit) = grit_bin() else {
        eprintln!("SKIP: CARGO_BIN_EXE_grit not set");
        return;
    };
    each_backend(|backend, repo| {
        if backend == Backend::Reftable && !git_interop_available(backend) {
            eprintln!("SKIP: git lacks reftable interop");
            return;
        }
        fs::write(repo.worktree().join("a.txt"), "a\n").expect("write");
        run_grit(repo.worktree(), &grit, &["add", "a.txt"]);
        run_grit(repo.worktree(), &grit, &["commit", "-m", "grit first"]);
        fs::write(repo.worktree().join("b.txt"), "b\n").expect("write");
        run_grit(repo.worktree(), &grit, &["add", "b.txt"]);
        run_grit(repo.worktree(), &grit, &["commit", "-m", "grit second"]);
        run_grit(repo.worktree(), &grit, &["switch", "-c", "grit-side"]);
        let entries = read_reflog(&repo.git_dir(), "HEAD").expect("grit read reflog");
        assert!(entries.len() >= 2);
        assert_signatures_match_git(&repo, "HEAD", &entries);
        assert!(
            git_fsck_strict(repo.worktree()),
            "git fsck after grit writes"
        );
    });
}

#[test]
fn grit_writes_git_reads_reflog_signatures() {
    grit_writes_git_reads();
}

#[test]
fn reftable_git_interop_required_when_env_set() {
    if std::env::var("GRIT_REQUIRE_REFTABLE_GIT")
        .ok()
        .is_some_and(|v| v == "1")
    {
        assert!(require_reftable_git());
    }
}
