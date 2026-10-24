//! Branch listing on a clone whose refs live in a large `packed-refs`, against `git for-each-ref`.
//!
//! The listing and the upstream lookups read `packed-refs` through one cached view per
//! repository handle; these tests pin that the view tracks rewrites made by another process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::branch_tracking::{
    list_branches, BranchDetail, BranchListQuery, BranchScope, BranchSummary,
};
use grit_lib::refs::listing::RefQuery;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

/// Insert `entry` (a ref line plus any `^` peel line) into a sorted `packed-refs` file.
fn insert_packed_ref(packed_refs: &Path, refname: &str, entry: &str) {
    let text = std::fs::read_to_string(packed_refs).expect("read packed-refs");
    let mut out = String::with_capacity(text.len() + entry.len());
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        let name = line
            .split_once(' ')
            .filter(|_| !line.starts_with('#') && !line.starts_with('^'))
            .map(|(_, name)| name.trim_end());
        if !inserted && name.is_some_and(|name| name > refname) {
            out.push_str(entry);
            inserted = true;
        }
        out.push_str(line);
    }
    if !inserted {
        out.push_str(entry);
    }
    std::fs::write(packed_refs, out).expect("write packed-refs");
}

fn commit(dir: &Path, msg: &str) -> String {
    std::fs::write(dir.join("f"), msg.as_bytes()).expect("write");
    git(dir, &["add", "f"]);
    git(dir, &["commit", "-qm", msg]);
    git_out(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Upstream with many branches and annotated tags, cloned so every remote-tracking ref and
/// tag sits in a sorted, peeled `packed-refs` and `main` tracks `origin/main`.
fn clone_with_many_packed_refs() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let upstream = dir.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    let mut tips = Vec::new();
    for i in 0..20 {
        tips.push(commit(&upstream, &format!("commit {i}")));
    }
    let mut script = String::new();
    for i in 0..400 {
        let tip = &tips[i % tips.len()];
        script.push_str(&format!("create refs/heads/synth/b{i:04} {tip}\n"));
    }
    let mut child = Command::new("git")
        .current_dir(&upstream)
        .args(["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("update-ref");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
    }
    assert!(child.wait().unwrap().success());
    for i in 0..60 {
        git(
            &upstream,
            &[
                "tag",
                "-a",
                &format!("v{i:03}"),
                "-m",
                "tag",
                &tips[i % tips.len()],
            ],
        );
    }
    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &["clone", "-q", "--no-local", "upstream", "clone"],
    );
    let packed = std::fs::read_to_string(clone.join(".git/packed-refs")).expect("packed-refs");
    assert!(packed.starts_with("# pack-refs with:") && packed.contains(" sorted"));
    assert!(packed.contains("refs/remotes/origin/synth/b0399"));
    (dir, clone)
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
}

fn list(repo: &Repository, scope: BranchScope, refs: RefQuery) -> Vec<BranchSummary> {
    list_branches(repo, &BranchListQuery { scope, refs }, BranchDetail::Full).expect("list")
}

/// `(refname, oid, subject, upstream, ahead/behind)` rows the way `git for-each-ref` shows them.
fn git_rows(dir: &Path, prefix: &str) -> Vec<String> {
    git_out(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(upstream) %(upstream:track) %(contents:subject)",
            prefix,
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

fn grit_rows(rows: &[BranchSummary]) -> Vec<String> {
    rows.iter()
        .map(|b| {
            let (upstream, track) = match &b.upstream {
                None => (String::new(), String::new()),
                Some(u) if u.gone => (u.full_ref.clone(), "[gone]".to_owned()),
                Some(u) => {
                    let track = match (u.ahead.unwrap_or(0), u.behind.unwrap_or(0)) {
                        (0, 0) => String::new(),
                        (a, 0) => format!("[ahead {a}]"),
                        (0, b) => format!("[behind {b}]"),
                        (a, b) => format!("[ahead {a}, behind {b}]"),
                    };
                    (u.full_ref.clone(), track)
                }
            };
            format!(
                "{} {} {} {} {}",
                b.full_ref,
                b.oid,
                upstream,
                track,
                b.subject.as_deref().unwrap_or("")
            )
        })
        .collect()
}

#[test]
fn clone_branch_listing_matches_git_and_follows_packed_refs_rewrites() {
    let (_dir, clone) = clone_with_many_packed_refs();
    let repo = open(&clone);

    let local = list(&repo, BranchScope::Local, RefQuery::default());
    assert_eq!(local.len(), 1);
    assert_eq!(grit_rows(&local), git_rows(&clone, "refs/heads/"));
    let remote = list(&repo, BranchScope::Remote, RefQuery::default());
    assert_eq!(
        remote.len(),
        402,
        "400 synth branches, main and origin/HEAD"
    );
    assert_eq!(
        grit_rows(&remote)
            .into_iter()
            .filter(|row| !row.starts_with("refs/remotes/origin/HEAD "))
            .collect::<Vec<_>>(),
        git_rows(&clone, "refs/remotes/")
            .into_iter()
            .filter(|row| !row.starts_with("refs/remotes/origin/HEAD "))
            .collect::<Vec<_>>()
    );

    // Another process moves `origin/main` back and drops a remote branch, then repacks: the
    // same handle must list the rewritten file, not the view it indexed before.
    let older = git_out(&clone, &["rev-parse", "main~3"]).trim().to_owned();
    git(&clone, &["update-ref", "refs/remotes/origin/main", &older]);
    git(
        &clone,
        &["update-ref", "-d", "refs/remotes/origin/synth/b0007"],
    );
    git(&clone, &["pack-refs", "--all"]);
    assert!(!clone.join(".git/refs/remotes/origin/main").exists());

    let local = list(&repo, BranchScope::Local, RefQuery::default());
    assert_eq!(grit_rows(&local), git_rows(&clone, "refs/heads/"));
    let upstream = local[0].upstream.as_ref().expect("upstream");
    assert_eq!((upstream.ahead, upstream.behind), (Some(3), Some(0)));
    let remote = list(&repo, BranchScope::Remote, RefQuery::default());
    assert_eq!(remote.len(), 401);
    assert!(remote.iter().all(|b| b.name != "origin/synth/b0007"));

    git(&clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
    git(&clone, &["pack-refs", "--all"]);
    let local = list(&repo, BranchScope::Local, RefQuery::default());
    assert!(local[0].upstream.as_ref().expect("upstream").gone);
}

#[test]
fn branch_at_annotated_tag_is_peeled_for_filters_like_git() {
    let (_dir, clone) = clone_with_many_packed_refs();
    let tag = git_out(&clone, &["rev-parse", "refs/tags/v005"])
        .trim()
        .to_owned();
    let target = git_out(&clone, &["rev-parse", "refs/tags/v005^{commit}"])
        .trim()
        .to_owned();
    // Git refuses to write non-commits under `refs/heads/` (newer `git pack-refs` included),
    // but reads such refs, so add the peeled entry to the sorted `packed-refs` directly.
    insert_packed_ref(
        &clone.join(".git/packed-refs"),
        "refs/heads/at-tag",
        &format!("{tag} refs/heads/at-tag\n^{target}\n"),
    );
    assert_eq!(
        git_out(&clone, &["rev-parse", "refs/heads/at-tag"]).trim(),
        tag
    );
    let repo = open(&clone);
    let target: ObjectId = target.parse().unwrap();

    let all = list(&repo, BranchScope::Local, RefQuery::default());
    let at_tag = all.iter().find(|b| b.name == "at-tag").expect("at-tag");
    assert_eq!(at_tag.oid.to_hex(), tag);

    let names = |rows: Vec<BranchSummary>| rows.into_iter().map(|b| b.full_ref).collect::<Vec<_>>();
    let git_names = |args: &[&str]| {
        let mut full = vec!["for-each-ref", "--format=%(refname)"];
        full.extend_from_slice(args);
        full.push("refs/heads/");
        git_out(&clone, &full)
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let points_at = RefQuery {
        points_at: vec![target],
        ..RefQuery::default()
    };
    assert_eq!(
        names(list(&repo, BranchScope::Local, points_at)),
        git_names(&["--points-at", &target.to_hex()])
    );
    let contains = RefQuery {
        contains: vec![target],
        ..RefQuery::default()
    };
    assert_eq!(
        names(list(&repo, BranchScope::Local, contains)),
        git_names(&["--contains", &target.to_hex()])
    );
}
