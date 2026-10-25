//! `porcelain::pull` rebase integration against system `git pull --rebase`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::change_id::{change_id, resolved_change_id, ChangeIdPolicy};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::pull::{pull, PullError, PullMode, PullOptions, PullResult};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    abort_rebase, continue_rebase, ApplyOptions, EmptyPolicy, RebaseOntoOptions, RebaseOutcome,
    RebaseRequest, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_test_support::{git, git_fsck, unique_tmp, write_file};
use std::fs;
use std::path::{Path, PathBuf};

fn test_sig() -> grit_lib::ident::Signature {
    "T <t@example.com> 1700000000 +0000"
        .parse()
        .expect("test signature")
}

fn commit(dir: &Path, file: &str, content: &str, message: &str) {
    write_file(dir, file, content);
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

fn open(dir: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = dir.to_path_buf();
    Repository::discover(Some(dir), &RepositoryOptions::with_environment(env)).expect("open")
}

fn rev(dir: &Path, spec: &str) -> ObjectId {
    git(dir, &["rev-parse", spec]).trim().parse().expect("oid")
}

fn tree_oid(dir: &Path, commit: &str) -> ObjectId {
    rev(dir, &format!("{commit}^{{tree}}"))
}

fn grit_identity(dir: &Path) {
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@example.com"]);
}

fn setup_tracked_pair(tag: &str) -> (PathBuf, PathBuf) {
    let root = unique_tmp("pull-rebase-git", tag);
    let origin = root.join("origin");
    fs::create_dir_all(&origin).expect("origin");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "f", "seed\n", "seed");
    git(&root, &["clone", "-q", "origin", "grit"]);
    git(&root, &["clone", "-q", "origin", "git"]);
    let grit_dir = root.join("grit");
    let git_dir = root.join("git");
    grit_identity(&grit_dir);
    grit_identity(&git_dir);
    (grit_dir, git_dir)
}

fn fetch_opts(env: &Environment) -> FetchOptions {
    FetchOptions {
        tags: TagMode::Following,
        environment: env.clone(),
        ..Default::default()
    }
}

fn pull_rebase_grit(repo: &Repository) -> grit_lib::porcelain::pull::PullOutcome {
    let mut progress = NoProgress;
    pull(
        repo,
        &PullOptions {
            mode: Some(PullMode::Rebase),
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
        &mut progress,
        &NeverCancel,
        None,
    )
    .expect("grit pull --rebase")
}

fn git_pull_rebase(dir: &Path) {
    git(dir, &["pull", "-q", "--rebase"]);
}

fn assert_fsck(dir: &Path) {
    let outcome = git_fsck(dir, false);
    assert!(outcome.ok, "git fsck failed: {:?}", outcome.msg_ids);
}

fn rev_by_subject(dir: &Path, subject: &str) -> ObjectId {
    git(
        dir,
        &[
            "log",
            "--format=%H",
            "-n",
            "1",
            "--fixed-strings",
            &format!("--grep={subject}"),
            "HEAD",
        ],
    )
    .trim()
    .parse()
    .expect("commit by subject")
}

fn log_messages(dir: &Path) -> Vec<String> {
    git(dir, &["log", "--format=%s", "--reverse", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn log_full_messages(dir: &Path) -> Vec<String> {
    git(dir, &["log", "--format=%B", "--reverse", "HEAD"])
        .split("\n\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn log_authors(dir: &Path) -> Vec<String> {
    git(dir, &["log", "--format=%an <%ae>", "--reverse", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn assert_repo_matches_git(grit_dir: &Path, git_dir: &Path) {
    assert_eq!(
        tree_oid(grit_dir, "HEAD"),
        tree_oid(git_dir, "HEAD"),
        "result tree must match `git pull --rebase`"
    );
    assert_eq!(
        log_messages(grit_dir),
        log_messages(git_dir),
        "commit subjects must match `git pull --rebase`"
    );
    assert_eq!(
        log_full_messages(grit_dir),
        log_full_messages(git_dir),
        "commit messages must match `git pull --rebase`"
    );
    assert_eq!(
        log_authors(grit_dir),
        log_authors(git_dir),
        "commit authors must match `git pull --rebase`"
    );
    assert_fsck(grit_dir);
    assert_fsck(git_dir);
}

fn stored_change_id(repo: &Repository, oid: ObjectId) -> Option<String> {
    let obj = repo.odb().read(&oid).ok()?;
    let data = parse_commit(&obj.data).ok()?;
    let config = repo.config().ok()?;
    if ChangeIdPolicy::from_config(config.as_ref()).enabled() {
        Some(resolved_change_id(&oid, &data).id.as_str().to_owned())
    } else {
        change_id(&data).map(|id| id.as_str().to_owned())
    }
}

fn index_blob(dir: &Path) -> Vec<u8> {
    fs::read(dir.join(".git/index")).expect("index")
}

fn worktree_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    fn walk(base: &Path, rel: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(base.join(rel)).expect("read_dir") {
            let entry = entry.expect("entry");
            let name = entry.file_name();
            let rel_path = if rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                rel.join(name)
            };
            if rel_path.starts_with(".git") {
                continue;
            }
            let meta = entry.metadata().expect("meta");
            if meta.is_dir() {
                walk(base, &rel_path, out);
            } else if meta.is_file() {
                let bytes = fs::read(base.join(&rel_path)).expect("read");
                out.push((rel_path.to_string_lossy().into_owned(), bytes));
            }
        }
    }
    walk(dir, Path::new(""), &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn pull_rebase_linear_matches_git() {
    let (grit_dir, git_dir) = setup_tracked_pair("linear");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "local1.txt", "a\n", "local one");
    commit(&grit_dir, "local2.txt", "b\n", "local two");
    commit(&grit_dir, "local3.txt", "c\n", "local three");
    commit(&git_dir, "local1.txt", "a\n", "local one");
    commit(&git_dir, "local2.txt", "b\n", "local two");
    commit(&git_dir, "local3.txt", "c\n", "local three");
    for i in 0..5 {
        commit(
            &origin,
            &format!("up{i}.txt"),
            "u\n",
            &format!("upstream {i}"),
        );
    }
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let repo = open(&grit_dir);
    let mut before_ids = std::collections::BTreeMap::new();
    for subject in ["local one", "local two", "local three"] {
        let oid = rev_by_subject(&grit_dir, subject);
        before_ids.insert(
            subject.to_owned(),
            stored_change_id(&repo, oid).expect("change-id on local commit"),
        );
    }
    let outcome = pull_rebase_grit(&repo);
    assert_eq!(outcome.result, PullResult::Rebased);

    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);

    let repo = open(&grit_dir);
    for (subject, expected) in before_ids {
        let oid = rev_by_subject(&grit_dir, &subject);
        assert_eq!(
            stored_change_id(&repo, oid),
            Some(expected),
            "change-id preserved for {subject}"
        );
    }
}

#[test]
fn pull_rebase_drops_patch_already_upstream() {
    let (grit_dir, git_dir) = setup_tracked_pair("skip-applied");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "shared.txt", "same\n", "shared change");
    commit(&git_dir, "shared.txt", "same\n", "shared change");
    commit(&origin, "shared.txt", "same\n", "shared change");
    commit(&origin, "other.txt", "x\n", "other upstream");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert!(
        matches!(
            outcome.result,
            PullResult::Rebased | PullResult::FastForward | PullResult::UpToDate
        ),
        "unexpected {:?}",
        outcome.result
    );
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_rebase_merges_config_replays_merge_commit() {
    let (grit_dir, git_dir) = setup_tracked_pair("merges");
    let origin = grit_dir.parent().unwrap().join("origin");
    git(&grit_dir, &["checkout", "-q", "-b", "side"]);
    git(&git_dir, &["checkout", "-q", "-b", "side"]);
    commit(&grit_dir, "side.txt", "s\n", "side branch");
    commit(&git_dir, "side.txt", "s\n", "side branch");
    git(&grit_dir, &["checkout", "-q", "main"]);
    git(&git_dir, &["checkout", "-q", "main"]);
    commit(&grit_dir, "main.txt", "m\n", "main line");
    commit(&git_dir, "main.txt", "m\n", "main line");
    git(&grit_dir, &["merge", "-q", "--no-ff", "--no-edit", "side"]);
    git(&git_dir, &["merge", "-q", "--no-ff", "--no-edit", "side"]);
    assert_eq!(
        git(&grit_dir, &["rev-list", "--parents", "-n", "1", "HEAD"])
            .split_whitespace()
            .count()
            .saturating_sub(1),
        2,
        "fixture must start with a merge commit"
    );
    commit(&origin, "up.txt", "u\n", "upstream");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);
    git(&grit_dir, &["config", "pull.rebase", "merges"]);
    git(&git_dir, &["config", "pull.rebase", "merges"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    git_pull_rebase(&git_dir);
    assert_eq!(
        tree_oid(&grit_dir, "HEAD"),
        tree_oid(&git_dir, "HEAD"),
        "merge pull rebase must match `git pull --rebase` tree"
    );
    assert_fsck(&grit_dir);
    assert_fsck(&git_dir);
    let grit_parents = git(&grit_dir, &["rev-list", "--parents", "-n", "1", "HEAD"])
        .split_whitespace()
        .count()
        .saturating_sub(1);
    let git_parents = git(&git_dir, &["rev-list", "--parents", "-n", "1", "HEAD"])
        .split_whitespace()
        .count()
        .saturating_sub(1);
    assert_eq!(
        grit_parents, git_parents,
        "merge parent count must match git pull --rebase with pull.rebase=merges"
    );
    for dir in [&grit_dir, &git_dir] {
        for file in ["f", "main.txt", "side.txt", "up.txt"] {
            assert!(
                dir.join(file).is_file(),
                "{file} missing after pull rebase in {}",
                dir.display()
            );
        }
    }
    if git_parents == 2 {
        let upstream_tip = rev(&grit_dir, "origin/main").to_hex();
        for dir in [&grit_dir, &git_dir] {
            let second = git(dir, &["rev-parse", "HEAD^2"]).trim().to_string();
            git(
                dir,
                &["merge-base", "--is-ancestor", &upstream_tip, &second],
            );
        }
        let grit_second = git(&grit_dir, &["rev-parse", "HEAD^2"]).trim().to_string();
        let git_second = git(&git_dir, &["rev-parse", "HEAD^2"]).trim().to_string();
        assert_eq!(grit_second, git_second, "second parent must match git");
        let grit_first = git(&grit_dir, &["rev-parse", "HEAD^1"]).trim().to_string();
        let git_first = git(&git_dir, &["rev-parse", "HEAD^1"]).trim().to_string();
        assert_eq!(grit_first, git_first, "first parent must match git");
    }
}

fn git_unchecked(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("spawn git")
}

/// Both clones commit `local` to `conflict.txt` while the origin commits `remote` there.
fn conflicting_pair(tag: &str) -> (PathBuf, PathBuf) {
    let (grit_dir, git_dir) = setup_tracked_pair(tag);
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "conflict.txt", "local\n", "local");
    commit(&git_dir, "conflict.txt", "local\n", "local");
    commit(&origin, "conflict.txt", "remote\n", "remote");
    commit(&origin, "up.txt", "u\n", "upstream extra");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);
    (grit_dir, git_dir)
}

fn short_status(dir: &Path) -> String {
    git(dir, &["status", "--porcelain=v1"])
}

fn assert_stopped_rebase(dir: &Path) {
    assert!(dir.join(".git/rebase-merge").is_dir(), "rebase-merge state");
    assert!(
        git_unchecked(dir, &["symbolic-ref", "-q", "HEAD"])
            .status
            .code()
            == Some(1),
        "HEAD must be detached during the stop"
    );
    let body = fs::read_to_string(dir.join("conflict.txt")).expect("conflict.txt");
    assert!(
        body.contains("<<<<<<<") && body.contains(">>>>>>>"),
        "{body}"
    );
    // Added on both sides: stages 2 and 3 only (add/add).
    assert_eq!(
        git(dir, &["ls-files", "-u", "conflict.txt"])
            .lines()
            .count(),
        2
    );
}

#[test]
fn pull_rebase_conflict_stops_like_git_and_continues() {
    let (grit_dir, git_dir) = conflicting_pair("conflict-continue");
    let local_tip = rev(&grit_dir, "HEAD");
    let repo = open(&grit_dir);

    let outcome = pull_rebase_grit(&repo);
    assert_eq!(outcome.result, PullResult::RebaseConflicts);
    assert_eq!(outcome.oid, None, "nothing was integrated yet");
    assert_eq!(outcome.stopped_commit, Some(local_tip));
    assert_eq!(outcome.autostash, None);
    assert_eq!(
        outcome.conflicts,
        Some(vec![grit_lib::operation::Conflict {
            path: "conflict.txt".into(),
            kind: grit_lib::operation::ConflictKind::AddAdd,
        }])
    );
    assert_eq!(
        rev(&grit_dir, "refs/heads/main"),
        local_tip,
        "branch ref untouched"
    );
    assert_eq!(
        grit_lib::state::detect_in_progress(repo.git_dir()),
        vec![grit_lib::state::InProgressOperation::Rebase]
    );

    let git_out = git_unchecked(&git_dir, &["pull", "-q", "--rebase"]);
    assert!(!git_out.status.success(), "git pull --rebase must stop too");
    assert_stopped_rebase(&grit_dir);
    assert_stopped_rebase(&git_dir);
    assert_eq!(short_status(&grit_dir), short_status(&git_dir));
    assert_eq!(
        rev(&grit_dir, "HEAD"),
        rev(&git_dir, "HEAD"),
        "same replay base"
    );
    let git_status = git(&grit_dir, &["status"]);
    assert!(
        git_status.contains("rebasing branch 'main'"),
        "{git_status}"
    );

    for dir in [&grit_dir, &git_dir] {
        write_file(dir, "conflict.txt", "resolved\n");
        git(dir, &["add", "conflict.txt"]);
    }
    let resume = StartRebaseOptions {
        request: RebaseRequest {
            upstream: local_tip,
            onto: None,
            branch_tip: local_tip,
        },
        rebase: RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: "T <t@example.com> 1700000000 +0000".parse().expect("sig"),
            preserve_committer: false,
            sign: false,
        }),
        apply: ApplyOptions {
            identity: test_sig(),
            reflog_message: "rebase: continue".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Skip,
        },
    };
    let done = continue_rebase(&open(&grit_dir), &resume).expect("continue");
    assert!(matches!(done, RebaseOutcome::Done { .. }), "{done:?}");
    let out = git_unchecked(&git_dir, &["rebase", "--continue"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        git(&grit_dir, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert!(!grit_dir.join(".git/rebase-merge").exists());
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_rebase_conflict_abort_restores_tip_with_grit_and_git() {
    for use_git_abort in [false, true] {
        let tag = if use_git_abort {
            "conflict-git-abort"
        } else {
            "conflict-abort"
        };
        let (grit_dir, _git_dir) = conflicting_pair(tag);
        let local_tip = rev(&grit_dir, "HEAD");
        let outcome = pull_rebase_grit(&open(&grit_dir));
        assert_eq!(outcome.result, PullResult::RebaseConflicts);

        if use_git_abort {
            git(&grit_dir, &["rebase", "--abort"]);
        } else {
            abort_rebase(&open(&grit_dir), &test_sig()).expect("abort");
        }
        assert_eq!(rev(&grit_dir, "HEAD"), local_tip);
        assert_eq!(
            git(&grit_dir, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/main"
        );
        assert_eq!(short_status(&grit_dir), "");
        assert!(!grit_dir.join(".git/rebase-merge").exists());
        assert_fsck(&grit_dir);
    }
}

#[test]
fn pull_rebase_conflict_keeps_autostash_on_stash_ref() {
    let (grit_dir, _git_dir) = conflicting_pair("conflict-autostash");
    write_file(&grit_dir, "f", "seed\ndirty\n");
    let repo = open(&grit_dir);
    let outcome = pull(
        &repo,
        &PullOptions {
            mode: Some(PullMode::Rebase),
            autostash: true,
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("pull");
    assert_eq!(outcome.result, PullResult::RebaseConflicts);
    let stash = outcome.autostash.expect("autostash kept");
    assert_eq!(rev(&grit_dir, "refs/stash"), stash);
    assert_eq!(
        fs::read_to_string(grit_dir.join("f")).expect("f"),
        "seed\n",
        "the stash must not be applied over the conflict"
    );
    assert_stopped_rebase(&grit_dir);

    git(&grit_dir, &["rebase", "--abort"]);
    git(&grit_dir, &["stash", "pop", "-q"]);
    assert_eq!(
        fs::read_to_string(grit_dir.join("f")).expect("f"),
        "seed\ndirty\n"
    );
    assert_fsck(&grit_dir);
}

#[test]
fn pull_rebase_conflict_with_local_changes_refuses_and_keeps_them() {
    let (grit_dir, _git_dir) = conflicting_pair("conflict-dirty");
    let local_tip = rev(&grit_dir, "HEAD");
    write_file(&grit_dir, "f", "seed\ndirty\n");
    let repo = open(&grit_dir);
    let mut progress = NoProgress;
    let err = pull(
        &repo,
        &PullOptions {
            mode: Some(PullMode::Rebase),
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
        &mut progress,
        &NeverCancel,
        None,
    )
    .expect_err("stopping would discard the local change");
    assert!(
        matches!(
            &err,
            PullError::Repository(grit_lib::error::Error::DirtyWorktree {
                operation: grit_lib::error::WorktreeOperation::Rebase,
                paths,
            }) if paths == &vec!["f".to_owned()]
        ),
        "unexpected {err:?}"
    );
    assert_eq!(rev(&grit_dir, "HEAD"), local_tip);
    assert!(!grit_dir.join(".git/rebase-merge").exists());
    assert_eq!(
        fs::read_to_string(grit_dir.join("f")).expect("f"),
        "seed\ndirty\n"
    );
}

#[test]
fn pull_rebase_check_predicts_conflicts_without_changes() {
    let (grit_dir, _git_dir) = conflicting_pair("conflict-check");
    let head_before = rev(&grit_dir, "HEAD");
    let index_before = index_blob(&grit_dir);
    let wt_before = worktree_snapshot(&grit_dir);
    let repo = open(&grit_dir);
    let outcome = pull(
        &repo,
        &PullOptions {
            mode: Some(PullMode::Rebase),
            check: true,
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("pull --check");
    assert_eq!(outcome.result, PullResult::RebaseConflicts);
    let check = outcome.check.expect("check");
    assert_eq!(check.would, PullResult::RebaseConflicts);
    assert!(!check.fast_forward_possible);
    assert_eq!(rev(&grit_dir, "HEAD"), head_before);
    assert_eq!(index_blob(&grit_dir), index_before);
    assert_eq!(worktree_snapshot(&grit_dir), wt_before);
    assert!(!grit_dir.join(".git/rebase-merge").exists());

    commit(&grit_dir, "other.txt", "o\n", "unrelated local");
    git(&grit_dir, &["reset", "-q", "--hard", "HEAD~2"]);
    commit(&grit_dir, "other.txt", "o\n", "unrelated local");
    let clean = pull(
        &open(&grit_dir),
        &PullOptions {
            mode: Some(PullMode::Rebase),
            check: true,
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("pull --check");
    assert_eq!(clean.check.expect("check").would, PullResult::Rebased);
}

#[test]
fn pull_rebase_fast_forward_when_no_local_commits() {
    let (grit_dir, git_dir) = setup_tracked_pair("ff-only-local");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&origin, "up.txt", "u\n", "upstream only");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::FastForward);
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
}

fn git_pull_merge(dir: &Path) {
    git(dir, &["pull", "-q", "--no-rebase", "--no-ff", "--no-edit"]);
}

fn parent_count(dir: &Path, spec: &str) -> usize {
    git(dir, &["rev-list", "--parents", "-n", "1", spec])
        .split_whitespace()
        .count()
        .saturating_sub(1)
}

/// Both clones commit `Change a`, merge upstream `S2` with `pull --no-rebase`, then upstream
/// adds `S3`: the shape a refresh after a merge pull leaves behind.
fn setup_merge_of_upstream(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let (grit_dir, git_dir) = setup_tracked_pair(tag);
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "a.txt", "local\n", "Change a");
    commit(&git_dir, "a.txt", "local\n", "Change a");
    commit(&origin, "b.txt", "2\n", "S2");
    git_pull_merge(&grit_dir);
    git_pull_merge(&git_dir);
    assert_eq!(parent_count(&grit_dir, "HEAD"), 2, "fixture tip is a merge");
    (grit_dir, git_dir, origin)
}

#[test]
fn pull_rebase_keeps_local_commits_under_merge_of_upstream() {
    let (grit_dir, git_dir, origin) = setup_merge_of_upstream("merge-of-upstream");
    commit(&origin, "c.txt", "3\n", "S3");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    assert_eq!(outcome.oid, Some(rev(&grit_dir, "HEAD")));
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);

    assert_eq!(
        rev(&grit_dir, "HEAD^"),
        rev(&grit_dir, "origin/main"),
        "the local commit is replayed onto the new upstream tip"
    );
    assert_eq!(parent_count(&grit_dir, "HEAD"), 1);
    assert_eq!(
        fs::read_to_string(grit_dir.join("a.txt")).expect("a.txt"),
        "local\n",
        "the local change stays in the worktree"
    );
    assert!(
        grit_dir.join("c.txt").is_file(),
        "upstream S3 is checked out"
    );
    assert_eq!(git(&grit_dir, &["status", "--porcelain"]), "");
}

#[test]
fn pull_rebase_linearizes_merge_of_upstream_below_local_commit() {
    let (grit_dir, git_dir, origin) = setup_merge_of_upstream("merge-of-upstream-mid");
    commit(&grit_dir, "d.txt", "after\n", "Change d");
    commit(&git_dir, "d.txt", "after\n", "Change d");
    commit(&origin, "c.txt", "3\n", "S3");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD~2"), rev(&grit_dir, "origin/main"));
}

#[test]
fn pull_rebase_keeps_local_commits_under_two_merges_of_upstream() {
    let (grit_dir, git_dir, origin) = setup_merge_of_upstream("merge-of-upstream-twice");
    commit(&origin, "c.txt", "3\n", "S3");
    git_pull_merge(&grit_dir);
    git_pull_merge(&git_dir);
    commit(&origin, "e.txt", "4\n", "S4");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD^"), rev(&grit_dir, "origin/main"));
}

#[test]
fn pull_rebase_flattens_no_ff_topic_merge_onto_upstream() {
    let (grit_dir, git_dir) = setup_tracked_pair("no-ff-topic");
    let origin = grit_dir.parent().unwrap().join("origin");
    for dir in [&grit_dir, &git_dir] {
        git(dir, &["checkout", "-q", "-b", "topic"]);
        commit(dir, "topic.txt", "t\n", "topic change");
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["merge", "-q", "--no-ff", "--no-edit", "topic"]);
        assert_eq!(parent_count(dir, "HEAD"), 2, "fixture tip is a merge");
    }
    commit(&origin, "up.txt", "u\n", "upstream");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD^"), rev(&grit_dir, "origin/main"));
}

#[test]
fn pull_rebase_flattens_multi_commit_topic_merge() {
    let (grit_dir, git_dir) = setup_tracked_pair("multi-topic");
    let origin = grit_dir.parent().unwrap().join("origin");
    for dir in [&grit_dir, &git_dir] {
        git(dir, &["checkout", "-q", "-b", "topic"]);
        commit(dir, "t1.txt", "1\n", "topic one");
        commit(dir, "t2.txt", "2\n", "topic two");
        git(dir, &["checkout", "-q", "main"]);
        commit(dir, "main.txt", "m\n", "main line");
        git(dir, &["merge", "-q", "--no-ff", "--no-edit", "topic"]);
    }
    commit(&origin, "up.txt", "u\n", "upstream");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD~3"), rev(&grit_dir, "origin/main"));
}

#[test]
fn pull_rebase_skips_patch_already_upstream_between_local_commits() {
    let (grit_dir, git_dir) = setup_tracked_pair("applied-mid-chain");
    let origin = grit_dir.parent().unwrap().join("origin");
    for dir in [&grit_dir, &git_dir] {
        commit(dir, "a.txt", "a\n", "local a");
        commit(dir, "shared.txt", "same\n", "shared change");
        commit(dir, "c.txt", "c\n", "local c");
    }
    commit(&origin, "shared.txt", "same\n", "shared change");
    commit(&origin, "other.txt", "x\n", "other upstream");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let outcome = pull_rebase_grit(&open(&grit_dir));
    assert_eq!(outcome.result, PullResult::Rebased);
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD~2"), rev(&grit_dir, "origin/main"));
}

#[test]
fn pull_rebase_lands_on_upstream_when_only_local_patch_is_upstream() {
    let (grit_dir, git_dir) = setup_tracked_pair("applied-only-patch");
    let origin = grit_dir.parent().unwrap().join("origin");
    // Distinct commit objects with the same patch, so the local commit is not simply upstream's.
    commit(&grit_dir, "shared.txt", "same\n", "shared change (local)");
    commit(&git_dir, "shared.txt", "same\n", "shared change (local)");
    commit(&origin, "shared.txt", "same\n", "shared change");
    commit(&origin, "other.txt", "x\n", "other upstream");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    pull_rebase_grit(&open(&grit_dir));
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD"), rev(&grit_dir, "origin/main"));
}

#[test]
fn pull_rebase_drops_merge_of_upstream_without_local_commits() {
    let (grit_dir, git_dir) = setup_tracked_pair("merge-only");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&origin, "b.txt", "2\n", "S2");
    for dir in [&grit_dir, &git_dir] {
        git(dir, &["pull", "-q", "--no-rebase", "--no-ff", "--no-edit"]);
        assert_eq!(parent_count(dir, "HEAD"), 2, "fixture tip is a merge");
    }
    commit(&origin, "c.txt", "3\n", "S3");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    pull_rebase_grit(&open(&grit_dir));
    git_pull_rebase(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(rev(&grit_dir, "HEAD"), rev(&grit_dir, "origin/main"));
}
