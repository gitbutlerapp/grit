//! `porcelain::pull` rebase integration against system `git pull --rebase`.

use grit_lib::change_id::{change_id, resolved_change_id, ChangeIdPolicy};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::pull::{pull, PullMode, PullOptions, PullResult};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_fsck, unique_tmp, write_file};
use std::fs;
use std::path::{Path, PathBuf};

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

#[test]
fn pull_rebase_conflict_leaves_repo_unchanged() {
    let (grit_dir, _git_dir) = setup_tracked_pair("conflict");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "conflict.txt", "local\n", "local");
    commit(&origin, "conflict.txt", "remote\n", "remote");
    git(&grit_dir, &["fetch", "-q"]);

    let repo = open(&grit_dir);
    let head_before = rev(&grit_dir, "HEAD");
    let index_before = index_blob(&grit_dir);
    let wt_before = worktree_snapshot(&grit_dir);
    let refs_before = fs::read(grit_dir.join(".git/HEAD")).expect("head");

    let outcome = pull_rebase_grit(&repo);
    assert_eq!(outcome.result, PullResult::Conflicts);
    assert_eq!(rev(&grit_dir, "HEAD"), head_before);
    assert_eq!(index_blob(&grit_dir), index_before);
    assert_eq!(worktree_snapshot(&grit_dir), wt_before);
    assert_eq!(fs::read(grit_dir.join(".git/HEAD")).unwrap(), refs_before);
    assert_fsck(&grit_dir);
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
