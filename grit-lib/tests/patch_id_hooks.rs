//! Patch-ID and hooks library integration tests (task #1479).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::patch_ids::PatchIdMode;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_patch_ids_from_log(dir: &std::path::Path) -> Vec<(ObjectId, ObjectId)> {
    let log_p = git_in(dir, &["log", "-p", "--reverse"]);
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["patch-id", "--stable"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git patch-id");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(log_p.as_bytes())
        .expect("write log");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            let patch = ObjectId::from_hex(parts.next().expect("patch")).expect("patch hex");
            let commit = ObjectId::from_hex(parts.next().expect("commit")).expect("commit hex");
            (patch, commit)
        })
        .collect()
}

#[test]
fn patch_id_matches_git_patch_id_stable() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "first"]);
    std::fs::write(dir.path().join("f.txt"), b"one\ntwo\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "second"]);
    std::fs::write(dir.path().join("g.txt"), b"new\n").unwrap();
    git_in(dir.path(), &["add", "g.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "third"]);
    std::fs::write(dir.path().join("bin.dat"), &[0u8, 1, 2, 3]).unwrap();
    git_in(dir.path(), &["add", "bin.dat"]);
    git_in(dir.path(), &["commit", "-q", "-m", "binary"]);
    std::fs::write(dir.path().join("h.txt"), b"a\n").unwrap();
    git_in(dir.path(), &["add", "h.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-h"]);
    git_in(dir.path(), &["mv", "h.txt", "i.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "rename"]);
    let mut perms = std::fs::metadata(dir.path().join("f.txt"))
        .expect("f.txt")
        .permissions();
    perms.set_mode(0o100755);
    std::fs::set_permissions(dir.path().join("f.txt"), perms).expect("chmod");
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "mode-only"]);
    std::fs::write(dir.path().join("j.txt"), b"j\n").unwrap();
    git_in(dir.path(), &["add", "j.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-j"]);
    std::fs::write(dir.path().join("k.txt"), b"k\n").unwrap();
    git_in(dir.path(), &["add", "k.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-k"]);
    std::fs::write(dir.path().join("l.txt"), b"l\n").unwrap();
    git_in(dir.path(), &["add", "l.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-l"]);

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let git_ids = git_patch_ids_from_log(dir.path());
    assert!(
        git_ids.len() >= 10,
        "expected at least 10 patch-ids, got {}",
        git_ids.len()
    );

    for (git_patch, commit) in git_ids {
        let grit = repo
            .patch_id(&commit, PatchIdMode::Stable)
            .expect("patch_id")
            .expect("non-merge commit should have patch-id");
        assert_eq!(grit, git_patch, "patch-id for {}", commit.to_hex());
    }
}

fn git_rev_list_oids(dir: &std::path::Path, range: &str) -> Vec<ObjectId> {
    git_in(dir, &["rev-list", range])
        .lines()
        .map(|l| l.parse().expect("rev-list oid"))
        .collect()
}

fn git_stable_patch_id_of(dir: &std::path::Path, commit: &ObjectId) -> ObjectId {
    let show = git_in(dir, &["show", "-p", &commit.to_hex()]);
    git_patch_id_from_text(dir, show.as_bytes(), "--stable")
}

#[test]
fn patch_ids_for_revs_expands_two_dot_range_like_git() {
    let history = crate::support::HistoryRepo::init();
    let wt = history.worktree();
    crate::support::rewritten_branch_fixture(wt);
    let repo = crate::support::open_grit_repo(wt);

    let got = repo
        .patch_ids_for_revs(&["main..old-feat"], PatchIdMode::Stable)
        .expect("range patch-ids");
    let want: Vec<(ObjectId, ObjectId)> = git_rev_list_oids(wt, "main..old-feat")
        .into_iter()
        .map(|c| (git_stable_patch_id_of(wt, &c), c))
        .collect();
    assert_eq!(want.len(), 3, "c1..c3 on the old branch");
    assert_eq!(got, want, "rev-list order and git patch-id --stable");

    let rewritten = repo
        .patch_ids_for_revs(&["main..feat"], PatchIdMode::Stable)
        .expect("rewritten range");
    let old_ids: Vec<ObjectId> = got.iter().map(|(p, _)| *p).collect();
    let new_ids: Vec<ObjectId> = rewritten.iter().map(|(p, _)| *p).collect();
    assert_eq!(new_ids, old_ids, "rebase and c3 reword keep every patch-id");
    assert_ne!(rewritten[0].1, got[0].1, "c3 was rewritten");
}

#[test]
fn patch_ids_for_revs_mixes_single_revisions_and_symmetric_ranges() {
    let history = crate::support::HistoryRepo::init();
    let wt = history.worktree();
    crate::support::rewritten_branch_fixture(wt);
    let repo = crate::support::open_grit_repo(wt);

    let got = repo
        .patch_ids_for_revs(&["feat", "old-feat...feat", "feat"], PatchIdMode::Stable)
        .expect("mixed specs");
    let tip: ObjectId = git_in(wt, &["rev-parse", "feat"]).trim().parse().unwrap();
    let symmetric = git_rev_list_oids(wt, "old-feat...feat");
    assert_eq!(
        symmetric.len(),
        7,
        "3 old commits, 3 rewritten and main's m"
    );
    let mut want_commits = vec![tip];
    want_commits.extend(&symmetric);
    want_commits.push(tip);
    let got_commits: Vec<ObjectId> = got.iter().map(|(_, c)| *c).collect();
    assert_eq!(got_commits, want_commits, "spec order, then walk order");
    for (patch_id, commit) in &got {
        assert_eq!(*patch_id, git_stable_patch_id_of(wt, commit), "{commit}");
    }

    let unstable = repo
        .patch_ids_for_revs(&["main..old-feat"], PatchIdMode::Unstable)
        .expect("unstable range");
    let single: Vec<(ObjectId, ObjectId)> = git_rev_list_oids(wt, "main..old-feat")
        .into_iter()
        .map(|c| {
            (
                repo.patch_id(&c, PatchIdMode::Unstable).unwrap().unwrap(),
                c,
            )
        })
        .collect();
    assert_eq!(unstable, single, "mode applies to every expanded commit");
}

#[test]
fn patch_ids_and_cherry_mark_match_git_on_ranges_split_across_workers() {
    let history = crate::support::HistoryRepo::init();
    let wt = history.worktree();
    crate::support::commit_file(wt, "base", "base\n", "base");
    git_in(wt, &["branch", "-M", "main"]);
    git_in(wt, &["switch", "-q", "-c", "topic"]);
    for i in 0..40 {
        crate::support::commit_file(wt, &format!("t{i}"), &format!("{i}\n"), &format!("t{i}"));
    }
    git_in(wt, &["branch", "old-topic"]);
    git_in(wt, &["switch", "-q", "main"]);
    crate::support::commit_file(wt, "up", "up\n", "upstream");
    git_in(wt, &["switch", "-q", "topic"]);
    git_in(wt, &["rebase", "-q", "main"]);
    let repo = crate::support::open_grit_repo(wt);

    let got = repo
        .patch_ids_for_revs(&["old-topic...topic"], PatchIdMode::Stable)
        .expect("range patch-ids");
    let want: Vec<(ObjectId, ObjectId)> = git_rev_list_oids(wt, "old-topic...topic")
        .into_iter()
        .map(|c| (git_stable_patch_id_of(wt, &c), c))
        .collect();
    assert_eq!(want.len(), 81);
    assert_eq!(got, want);

    let opts = grit_lib::revwalk::RevWalkListOptions {
        cherry_mark: true,
        ..Default::default()
    };
    let result = repo
        .rev_walk()
        .rev_list_result(&["old-topic...topic"], &opts)
        .expect("cherry-mark");
    assert_eq!(result.cherry_matches.len(), 40);
    assert!(result
        .cherry_matches
        .iter()
        .all(|m| m.left.len() == 1 && m.right.len() == 1 && m.message_changed == Some(false)));
    let marked = git_in(
        wt,
        &[
            "rev-list",
            "--left-right",
            "--cherry-mark",
            "old-topic...topic",
        ],
    );
    let equivalent: Vec<&str> = marked.lines().filter_map(|l| l.strip_prefix('=')).collect();
    assert_eq!(equivalent.len(), 80);
    for oid in equivalent {
        assert!(result.cherry_equivalent.contains(&oid.parse().unwrap()));
    }
}

#[test]
fn patch_ids_for_revs_skips_merges_and_rejects_unknown_specs() {
    let history = crate::support::HistoryRepo::init();
    let wt = history.worktree();
    crate::support::rewritten_branch_fixture(wt);
    git_in(wt, &["switch", "-q", "main"]);
    git_in(
        wt,
        &["merge", "-q", "--no-ff", "-m", "merge old", "old-feat"],
    );
    let repo = crate::support::open_grit_repo(wt);

    let got = repo
        .patch_ids_for_revs(&["old-feat..main", "main"], PatchIdMode::Stable)
        .expect("range with merge");
    let commits: Vec<ObjectId> = got.iter().map(|(_, c)| *c).collect();
    let m: ObjectId = git_in(wt, &["rev-parse", "main^1"]).trim().parse().unwrap();
    assert_eq!(
        commits,
        vec![m],
        "merge skipped in the range and as a single spec"
    );

    assert!(repo
        .patch_ids_for_revs(&["main..no-such-branch"], PatchIdMode::Stable)
        .is_err());
    assert!(repo
        .patch_ids_for_revs(&["no-such-branch"], PatchIdMode::Stable)
        .is_err());
    assert!(repo
        .patch_ids_for_revs(&[], PatchIdMode::Stable)
        .expect("no specs")
        .is_empty());
}

fn git_patch_id_from_show(dir: &std::path::Path, flag: &str) -> ObjectId {
    let show_p = git_in(dir, &["show", "-p", "HEAD"]);
    git_patch_id_from_text(dir, show_p.as_bytes(), flag)
}

fn git_patch_id_from_text(dir: &std::path::Path, patch: &[u8], flag: &str) -> ObjectId {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["patch-id", flag])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git patch-id");
    child.stdin.take().unwrap().write_all(patch).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git patch-id {flag}");
    ObjectId::from_hex(
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .expect("patch id"),
    )
    .expect("hex")
}

#[test]
fn patch_id_verbatim_matches_git_whitespace_sensitive() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.path().join("f.txt"), b"one  \ntwo\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "trailing-space"]);

    let commit: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let git_stable = git_patch_id_from_show(dir.path(), "--stable");
    let git_verbatim = git_patch_id_from_show(dir.path(), "--verbatim");
    assert_ne!(git_stable, git_verbatim, "fixture must differ by mode");

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let grit_stable = repo
        .patch_id(&commit, PatchIdMode::Stable)
        .expect("stable")
        .expect("some");
    let grit_verbatim = repo
        .patch_id(&commit, PatchIdMode::Verbatim)
        .expect("verbatim")
        .expect("some");

    assert_eq!(grit_stable, git_stable, "stable ODB vs git");
    assert_eq!(grit_verbatim, git_verbatim, "verbatim ODB vs git");
    assert_ne!(grit_stable, grit_verbatim);
}

#[test]
fn patch_id_stable_and_unstable_differ_for_multi_file_commit() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git_in(dir.path(), &["add", "a.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.path().join("a.txt"), b"a\nchanged\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b\n").unwrap();
    git_in(dir.path(), &["add", "a.txt", "b.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "two-file"]);

    let commit: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let git_stable_id = git_patch_id_from_show(dir.path(), "--stable");
    let git_unstable_id = git_patch_id_from_show(dir.path(), "--unstable");
    assert_ne!(
        git_stable_id, git_unstable_id,
        "git stable and unstable must differ for this fixture"
    );

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let grit_stable = repo
        .patch_id(&commit, PatchIdMode::Stable)
        .expect("stable")
        .expect("some");
    let grit_unstable = repo
        .patch_id(&commit, PatchIdMode::Unstable)
        .expect("unstable")
        .expect("some");

    assert_eq!(
        grit_stable, git_stable_id,
        "stable ODB vs git patch-id --stable"
    );
    assert_eq!(
        grit_unstable, git_unstable_id,
        "unstable ODB vs git patch-id --unstable"
    );
    assert_ne!(grit_stable, grit_unstable);
}

#[test]
fn merge_commit_has_no_patch_id() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f.txt"), b"x\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "base"]);
    git_in(dir.path(), &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.path().join("side.txt"), b"s\n").unwrap();
    git_in(dir.path(), &["add", "side.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "side"]);
    git_in(dir.path(), &["checkout", "-q", "main"]);
    git_in(dir.path(), &["merge", "-q", "--no-ff", "side"]);
    let merge_tip: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    assert!(repo
        .patch_id(&merge_tip, PatchIdMode::Stable)
        .expect("read")
        .is_none());
}

#[test]
fn hooks_list_respects_core_hooks_path() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "."]);
    let custom = dir.path().join("my-hooks");
    std::fs::create_dir_all(&custom).expect("mkdir");
    let hook_path = custom.join("pre-commit");
    std::fs::write(&hook_path, b"#!/bin/sh\nexit 0\n").expect("write hook");
    let mut perms = std::fs::metadata(&hook_path).expect("meta").permissions();
    perms.set_mode(0o100755);
    std::fs::set_permissions(&hook_path, perms).expect("chmod");
    git_in(
        dir.path(),
        &["config", "core.hooksPath", custom.to_str().expect("utf8")],
    );

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let hooks = repo.hooks().list(None).expect("list").items;
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].name, "pre-commit");
    assert_eq!(hooks[0].path, hook_path);
    assert!(hooks[0].executable);
    assert_eq!(
        hooks[0].source,
        grit_lib::repo_handles::HookSource::CoreHooksPath
    );
}

#[test]
fn non_executable_hook_reported() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "."]);
    let hooks_dir = dir.path().join("custom-hooks");
    std::fs::create_dir_all(&hooks_dir).expect("mkdir");
    git_in(
        dir.path(),
        &[
            "config",
            "core.hooksPath",
            hooks_dir.to_str().expect("utf8"),
        ],
    );
    let hook_path = hooks_dir.join("pre-push");
    std::fs::write(&hook_path, b"#!/bin/sh\nexit 0\n").expect("write");
    std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o100644)).expect("chmod");

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let hooks = repo.hooks().list(None).expect("list").items;
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].name, "pre-push");
    assert!(!hooks[0].executable);
    assert_eq!(
        hooks[0].source,
        grit_lib::repo_handles::HookSource::CoreHooksPath
    );
}

// Compare the ODB and text entry points against the same ordinary unified patch.
fn assert_patch_id_matches_normal_patch(
    dir: &std::path::Path,
    paths: &[String],
    label: &str,
) -> ObjectId {
    use grit_lib::diff::patch_ids::{compute_patch_id_for_paths, compute_patch_ids_from_text};
    use grit_lib::objects::HashAlgo;

    let commit: ObjectId = git_in(dir, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let repo = Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir),
    )
    .unwrap();
    let mut args = vec!["show", "--format=", "--no-ext-diff", "-U3", "HEAD", "--"];
    args.extend(paths.iter().map(String::as_str));
    let patch = git_in(dir, &args);
    let mut stable = None;
    for (mode, flag) in [
        (PatchIdMode::Stable, "--stable"),
        (PatchIdMode::Unstable, "--unstable"),
        (PatchIdMode::Verbatim, "--verbatim"),
    ] {
        let expected = git_patch_id_from_text(dir, patch.as_bytes(), flag);
        let from_text = compute_patch_ids_from_text(HashAlgo::Sha1, patch.as_bytes(), mode);
        assert_eq!(from_text.len(), 1, "{label}: {flag}");
        assert_eq!(from_text[0].0, expected, "{label}: text {flag}");
        let actual = compute_patch_id_for_paths(HashAlgo::Sha1, repo.odb(), &commit, paths, mode)
            .unwrap()
            .unwrap();
        assert_eq!(actual, expected, "{label}: filtered ODB {flag}");
        if paths.is_empty() {
            assert_eq!(
                repo.patch_id(&commit, mode).unwrap(),
                Some(expected),
                "{label}: repository {flag}"
            );
        }
        if mode == PatchIdMode::Stable {
            stable = Some(actual);
        }
    }
    git_in(dir, &["fsck", "--strict"]);
    stable.unwrap()
}

#[test]
fn patch_id_uses_three_context_lines_for_text_hunks() {
    for (label, count, edits, rename, final_newline) in [
        ("ten-line last-line edit", 10, vec![9], false, true),
        ("beginning", 40, vec![0], false, true),
        ("middle", 40, vec![19], false, true),
        ("separated hunks", 40, vec![4, 30], false, true),
        ("overlapping context", 40, vec![14, 19], false, true),
        ("touching context", 40, vec![14, 21], false, true),
        ("one-line gap between hunks", 40, vec![14, 22], false, true),
        ("edited rename", 80, vec![39], true, true),
        ("EOF without newline", 40, vec![39], false, false),
    ] {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        let mut lines: Vec<String> = (1..=count).map(|i| format!("{i}\n")).collect();
        if !final_newline {
            lines.last_mut().unwrap().pop();
        }
        std::fs::write(dir.path().join("file"), lines.concat()).unwrap();
        git_in(dir.path(), &["add", "file"]);
        git_in(dir.path(), &["commit", "-qm", "base"]);
        for i in edits {
            lines[i] = if i == count - 1 && !final_newline {
                "changed".to_owned()
            } else {
                "changed\n".to_owned()
            };
        }
        let path = if rename {
            git_in(dir.path(), &["mv", "file", "renamed"]);
            "renamed"
        } else {
            "file"
        };
        std::fs::write(dir.path().join(path), lines.concat()).unwrap();
        git_in(dir.path(), &["commit", "-qam", "change"]);
        let id = assert_patch_id_matches_normal_patch(dir.path(), &[], label);
        if label == "ten-line last-line edit" {
            assert_eq!(id.to_hex(), "5ab484ff6e8f56d89403fae49e2063b0712ed58e");
        }
        // A second file makes filtered and whole-commit IDs distinct, and exercises
        // per-file accumulation for both stable and unstable hashing.
        std::fs::write(dir.path().join("other"), "unrelated\n").unwrap();
        git_in(dir.path(), &["add", "other"]);
        git_in(dir.path(), &["commit", "--amend", "--no-edit", "-q"]);
        assert_patch_id_matches_normal_patch(dir.path(), &[], label);
        // Include both rename endpoints so Git retains rename detection when filtering.
        let paths = if rename {
            vec!["file".to_owned(), path.to_owned()]
        } else {
            vec![path.to_owned()]
        };
        assert_eq!(
            assert_patch_id_matches_normal_patch(dir.path(), &paths, label),
            id,
            "{label}: selecting only the edited file"
        );
    }
}

#[test]
fn patch_id_places_slidable_insertion_like_git_diff() {
    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), "a\nb\n\n#t\nfn e\nx\ny\n").unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    // `#t\nfn c\n` can slide one block either side of the existing `#t`; Git's
    // change-compact and indent heuristic place it before.
    std::fs::write(dir.path().join("f"), "a\nb\n\n#t\nfn c\n#t\nfn e\nx\ny\n").unwrap();
    git_in(dir.path(), &["commit", "-qam", "t"]);
    let id = assert_patch_id_matches_normal_patch(dir.path(), &[], "slidable insertion");
    assert_eq!(id.to_hex(), "0490ffb8ca10b8210d0b8cd748ee0da016bc6130");
    let commit: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .unwrap();
    for (mode, flag) in [
        (PatchIdMode::Stable, "--stable"),
        (PatchIdMode::Unstable, "--unstable"),
        (PatchIdMode::Verbatim, "--verbatim"),
    ] {
        assert_eq!(
            repo.patch_id(&commit, mode).unwrap(),
            Some(git_patch_id_from_show(dir.path(), flag)),
            "git show | git patch-id {flag}"
        );
    }
}

#[test]
fn patch_id_keeps_insertion_off_final_line_without_newline() {
    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f.json"), "{\n  \"a\": 1\n}\n").unwrap();
    git_in(dir.path(), &["add", "f.json"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    // The new last line `}` has no newline, so it is not the old `}\n`: the
    // appended object must not slide up to absorb the old closing brace.
    std::fs::write(
        dir.path().join("f.json"),
        "{\n  \"a\": 2\n}\n{\n  \"b\": 2\n}",
    )
    .unwrap();
    git_in(dir.path(), &["commit", "-qam", "append"]);
    assert_patch_id_matches_normal_patch(dir.path(), &[], "insertion before missing newline");
}

#[test]
fn patch_id_hashes_form_feed_and_vertical_tab() {
    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), "keep\n8 | 4\x0c+\nv\x0bt\n").unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("f"), "keep\n8 | 4\u{240c}+\nvt\n").unwrap();
    git_in(dir.path(), &["commit", "-qam", "visible control pictures"]);
    assert_patch_id_matches_normal_patch(dir.path(), &[], "form feed and vertical tab");
}

#[test]
fn patch_id_equivalence_ignores_distant_context_but_keeps_nearby_context() {
    let mut ids = Vec::new();
    for (distant, nearby) in [
        ("original", "nearby"),
        ("different", "nearby"),
        ("original", "different"),
    ] {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        let mut lines: Vec<String> = (1..=40).map(|i| format!("{i}\n")).collect();
        lines[0] = format!("{distant}\n");
        lines[18] = format!("{nearby}\n");
        std::fs::write(dir.path().join("file"), lines.concat()).unwrap();
        git_in(dir.path(), &["add", "file"]);
        git_in(dir.path(), &["commit", "-qm", "base"]);
        lines[19] = "changed\n".into();
        std::fs::write(dir.path().join("file"), lines.concat()).unwrap();
        git_in(dir.path(), &["commit", "-qam", "change"]);
        ids.push(assert_patch_id_matches_normal_patch(
            dir.path(),
            &[],
            "equivalence",
        ));
    }
    assert_eq!(
        ids[0], ids[1],
        "distant unchanged lines are outside the patch"
    );
    assert_ne!(
        ids[0], ids[2],
        "nearby unchanged lines are part of the patch"
    );
}

fn git_patch_id_of_commit(dir: &std::path::Path, commit: &ObjectId, flag: &str) -> ObjectId {
    let show_p = git_in(dir, &["show", "-p", &commit.to_hex()]);
    git_patch_id_from_text(dir, show_p.as_bytes(), flag)
}

#[test]
fn patch_id_hashes_gitlink_changes_without_reading_the_submodule_commit() {
    let dir = TempDir::new().unwrap();
    let missing_a = "13299d28f69f8009be8e08e453a9b0024f153a60";
    let missing_b = "23299d28f69f8009be8e08e453a9b0024f153a60";
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a"), "a\n").unwrap();
    git_in(dir.path(), &["add", "a"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    let cacheinfo = |oid: &str, path: &str| format!("160000,{oid},{path}");
    let mut commits = Vec::new();
    let mut commit = |message: &str| {
        git_in(dir.path(), &["commit", "-qm", message]);
        let oid: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .unwrap();
        commits.push(oid);
    };

    git_in(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &cacheinfo(missing_a, "sub"),
        ],
    );
    commit("add gitlink");
    git_in(
        dir.path(),
        &["update-index", "--cacheinfo", &cacheinfo(missing_b, "sub")],
    );
    commit("bump gitlink");
    git_in(
        dir.path(),
        &["update-index", "--cacheinfo", &cacheinfo(missing_a, "sub")],
    );
    std::fs::write(dir.path().join("a"), "a\nb\n").unwrap();
    git_in(dir.path(), &["add", "a"]);
    commit("bump gitlink and edit a file");
    git_in(dir.path(), &["update-index", "--force-remove", "sub"]);
    commit("delete gitlink");
    git_in(dir.path(), &["rm", "-q", "--cached", "a"]);
    git_in(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &cacheinfo(missing_b, "a"),
        ],
    );
    commit("replace a file with a gitlink");
    std::fs::write(dir.path().join("f"), "f\n").unwrap();
    git_in(dir.path(), &["add", "f"]);
    commit("add a file");
    std::fs::remove_file(dir.path().join("f")).unwrap();
    std::os::unix::fs::symlink("target", dir.path().join("f")).unwrap();
    git_in(dir.path(), &["add", "f"]);
    commit("replace a file with a symlink");

    for missing in [missing_a, missing_b] {
        let probe = Command::new("git")
            .current_dir(dir.path())
            .args(["cat-file", "-t", missing])
            .output()
            .unwrap();
        assert!(!probe.status.success(), "{missing} must not be an object");
    }

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .unwrap();
    for (mode, flag) in [
        (PatchIdMode::Stable, "--stable"),
        (PatchIdMode::Unstable, "--unstable"),
        (PatchIdMode::Verbatim, "--verbatim"),
    ] {
        let expected: Vec<(ObjectId, ObjectId)> = commits
            .iter()
            .map(|c| (git_patch_id_of_commit(dir.path(), c, flag), *c))
            .collect();
        for (patch_id, commit) in &expected {
            assert_eq!(
                repo.patch_id(commit, mode).unwrap(),
                Some(*patch_id),
                "git show {commit} | git patch-id {flag}"
            );
        }
        let range = format!("main~{}..main", commits.len());
        let mut from_range = repo.patch_ids_for_revs(&[&range], mode).unwrap();
        from_range.reverse();
        assert_eq!(from_range, expected, "range {flag}");
    }
}

fn tail_lines_bytes(content: &[u8], n: usize) -> Vec<u8> {
    let mut lines: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();
    if lines.last() == Some(&&b""[..]) {
        lines.pop();
    }
    let start = lines.len().saturating_sub(n);
    let mut out = Vec::new();
    for line in &lines[start..] {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn patch_id_stable_matches_git_git_t4201_shortlog_invalid_utf8() {
    let git_src = std::path::Path::new("/tmp/git-src");
    if !git_src.join("HEAD").exists() {
        eprintln!(
            "skip patch_id_stable_matches_git_git_t4201_shortlog_invalid_utf8: no /tmp/git-src"
        );
        return;
    }
    const C: &str = "253118d1d99b28b1c809e3607c0a182e141f3e45";
    let old_blob = git_in(git_src, &["show", &format!("{C}^:t/t4201-shortlog.sh")]);
    let new_blob = git_in(git_src, &["show", &format!("{C}:t/t4201-shortlog.sh")]);

    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), &old_blob).unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("f"), &new_blob).unwrap();
    git_in(dir.path(), &["commit", "-qam", "change"]);
    assert_patch_id_matches_normal_patch(dir.path(), &[], "git t4201 shortlog full file");

    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    let old_tail = tail_lines_bytes(old_blob.as_bytes(), 320);
    let new_tail = tail_lines_bytes(new_blob.as_bytes(), 320);
    std::fs::write(dir.path().join("f"), &old_tail).unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("f"), &new_tail).unwrap();
    git_in(dir.path(), &["commit", "-qam", "change"]);
    assert_patch_id_matches_normal_patch(dir.path(), &[], "git t4201 shortlog tail-320");
}
