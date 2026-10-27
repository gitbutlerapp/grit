//! Checkout-updating operations in a cone sparse checkout, against system `git` (issue #1814).
//!
//! Each scenario copies one repository set up with `git sparse-checkout set --cone a`, runs the
//! operation with system `git` in one copy and with the `grit-lib` entry point in the other, and
//! compares the work tree, `git ls-files -s` / `-t` and `git status`. Out-of-cone paths must stay
//! absent from disk and keep the skip-worktree bit.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

mod support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::index::sparse_checkout::SparseCheckout;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::pull::{pull, PullMode, PullOptions};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd, git_fsck, unique_tmp};

fn open(dir: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = dir.to_path_buf();
    Repository::discover(Some(dir), &RepositoryOptions::with_environment(env)).expect("open")
}

fn rev(dir: &Path, spec: &str) -> ObjectId {
    git(dir, &["rev-parse", spec]).trim().parse().expect("oid")
}

fn put(dir: &Path, name: &str, contents: &str) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn identity(dir: &Path) {
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@example.com"]);
}

/// History shared by every scenario, built in a full checkout:
///
/// - `main~2`: `a/f`, `b/f`, `top`
/// - `main~1`: changes `a/f` and `b/f`, adds `b/g`
/// - `main`: changes `top`
/// - `side` (from `main~1`): adds `a/new` and `b/new`
/// - `ours` / `theirs` (from `main~1`): both change `a/f` and `b/f`
fn seed_history(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main"]);
    identity(dir);
    put(dir, "a/f", "1\n");
    put(dir, "b/f", "1\n");
    put(dir, "top", "1\n");
    commit_all(dir, "c1");
    put(dir, "a/f", "2\n");
    put(dir, "b/f", "2\n");
    put(dir, "b/g", "2\n");
    commit_all(dir, "c2");
    git(dir, &["checkout", "-q", "-b", "side"]);
    put(dir, "a/new", "n\n");
    put(dir, "b/new", "n\n");
    commit_all(dir, "side");
    for (branch, body) in [("ours", "ours\n"), ("theirs", "theirs\n")] {
        git(dir, &["checkout", "-q", "-b", branch, "main"]);
        put(dir, "a/f", body);
        put(dir, "b/f", body);
        commit_all(dir, branch);
    }
    git(dir, &["checkout", "-q", "main"]);
    put(dir, "top", "3\n");
    commit_all(dir, "c3");
}

fn enable_cone(dir: &Path, sparse_index: bool) {
    let mut args = vec!["sparse-checkout", "set", "--cone"];
    if sparse_index {
        args.push("--sparse-index");
    }
    args.push("a");
    git(dir, &args);
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), &dest).expect("copy");
        }
    }
}

/// A sparse repository and two copies of it: `git_dir` for the oracle, `grit_dir` for grit.
struct Pair {
    git_dir: PathBuf,
    grit_dir: PathBuf,
}

fn sparse_pair(tag: &str, sparse_index: bool) -> Pair {
    let root = unique_tmp("sparse-checkout-writes", tag);
    let seed = root.join("seed");
    seed_history(&seed);
    enable_cone(&seed, sparse_index);
    let pair = Pair {
        git_dir: root.join("git"),
        grit_dir: root.join("grit"),
    };
    copy_dir(&seed, &pair.git_dir);
    copy_dir(&seed, &pair.grit_dir);
    pair
}

fn worktree_files(dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir.join(rel)).expect("read_dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if rel.is_empty() && name == ".git" {
            continue;
        }
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type().expect("type").is_dir() {
            worktree_files(dir, &path, out);
        } else {
            out.insert(path, fs::read(entry.path()).expect("read"));
        }
    }
}

/// What a sparse checkout looks like from the outside, as reported by system `git`.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    files: Vec<String>,
    stages: String,
    tags: String,
    status: String,
    sparse_entries: String,
}

fn observe(dir: &Path) -> Observed {
    let mut files = BTreeMap::new();
    worktree_files(dir, "", &mut files);
    Observed {
        files: files.into_keys().collect(),
        stages: git(dir, &["ls-files", "-s"]),
        tags: git(dir, &["ls-files", "-t"]),
        status: git(dir, &["status", "--porcelain"]),
        sparse_entries: git(dir, &["ls-files", "--sparse"]),
    }
}

fn assert_matches_git(pair: &Pair) {
    let grit = observe(&pair.grit_dir);
    let oracle = observe(&pair.git_dir);
    assert_eq!(grit, oracle, "grit and git disagree on the sparse checkout");
    for dir in [&pair.grit_dir, &pair.git_dir] {
        let fsck = git_fsck(dir, false);
        assert!(
            fsck.ok,
            "git fsck failed in {}: {:?}",
            dir.display(),
            fsck.msg_ids
        );
    }
}

fn assert_b_not_materialized(dir: &Path) {
    assert!(
        !dir.join("b").exists(),
        "out-of-cone directory b/ was written to the work tree"
    );
}

fn reset(repo: &Repository, target: ObjectId, mode: ResetMode) {
    reset_to_commit(
        repo,
        &ResetOptions::new(
            target,
            mode,
            support::parse_reflog_signature("T <t@example.com>"),
            "reset",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit reset");
}

#[test]
fn sparse_checkout_matcher_follows_cone_definition() {
    let pair = sparse_pair("matcher", false);
    let repo = open(&pair.grit_dir);
    let sparse = SparseCheckout::load(&repo)
        .expect("load")
        .expect("sparse checkout is enabled");
    assert!(sparse.includes("top"));
    assert!(sparse.includes("a/f"));
    assert!(sparse.includes("a/deep/x"));
    assert!(!sparse.includes("b/f"));
    assert!(sparse.includes_index_path(b"a/new"));
    assert!(!sparse.includes_index_path(b"b/new"));

    git(&pair.git_dir, &["sparse-checkout", "disable"]);
    assert!(SparseCheckout::load(&open(&pair.git_dir))
        .expect("load")
        .is_none());
}

#[test]
fn sparse_checkout_parse_handles_cone_non_cone_and_empty_files() {
    let cone = SparseCheckout::parse("/*\n!/*/\n/a/\n", true);
    assert!(cone.includes("top") && cone.includes("a/x") && !cone.includes("b/x"));
    let non_cone = SparseCheckout::parse("/*\n!/b/\n", false);
    assert!(non_cone.includes("a/x") && !non_cone.includes("b/x"));
    let invalid_cone = SparseCheckout::parse("*.txt\n", true);
    assert!(invalid_cone.includes("b/x.txt") && !invalid_cone.includes("b/x.rs"));
    let empty = SparseCheckout::parse("# nothing\n\n", true);
    assert!(!empty.includes("top"));
}

#[test]
fn reset_hard_keeps_out_of_cone_paths_skip_worktree() {
    let pair = sparse_pair("reset-hard", false);
    git(&pair.git_dir, &["reset", "-q", "--hard", "HEAD~2"]);
    let repo = open(&pair.grit_dir);
    reset(&repo, rev(&pair.grit_dir, "HEAD~2"), ResetMode::Hard);

    assert_b_not_materialized(&pair.grit_dir);
    assert_eq!(
        git(&pair.grit_dir, &["ls-files", "-t"]),
        "H a/f\nS b/f\nH top\n"
    );
    assert_eq!(fs::read(pair.grit_dir.join("a/f")).expect("a/f"), b"1\n");
    assert_matches_git(&pair);
}

#[test]
fn reset_hard_to_head_restores_in_cone_files_only() {
    let pair = sparse_pair("reset-hard-head", false);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        put(dir, "a/f", "dirty\n");
    }
    git(&pair.git_dir, &["reset", "-q", "--hard"]);
    let repo = open(&pair.grit_dir);
    reset(&repo, rev(&pair.grit_dir, "HEAD"), ResetMode::Hard);

    assert_b_not_materialized(&pair.grit_dir);
    assert_matches_git(&pair);
}

#[test]
fn reset_mixed_carries_skip_worktree_bits() {
    let pair = sparse_pair("reset-mixed", false);
    git(&pair.git_dir, &["reset", "-q", "HEAD~2"]);
    let repo = open(&pair.grit_dir);
    reset(&repo, rev(&pair.grit_dir, "HEAD~2"), ResetMode::Mixed);

    assert_b_not_materialized(&pair.grit_dir);
    assert_matches_git(&pair);
}

#[test]
fn switch_detach_keeps_out_of_cone_paths_skip_worktree() {
    let pair = sparse_pair("switch-detach", false);
    git(&pair.git_dir, &["switch", "-q", "--detach", "HEAD~2"]);
    let repo = open(&pair.grit_dir);
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: rev(&pair.grit_dir, "HEAD~2"),
                reflog_to_label: "HEAD~2".into(),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit switch");

    assert_b_not_materialized(&pair.grit_dir);
    assert_eq!(
        git(&pair.grit_dir, &["ls-files", "-t"]),
        "H a/f\nS b/f\nH top\n"
    );
    assert_matches_git(&pair);
}

#[test]
fn switch_branch_adds_out_of_cone_paths_as_skip_worktree() {
    let pair = sparse_pair("switch-branch", false);
    git(&pair.git_dir, &["switch", "-q", "side"]);
    let repo = open(&pair.grit_dir);
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "side".into(),
                oid: rev(&pair.grit_dir, "side"),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit switch");

    assert_b_not_materialized(&pair.grit_dir);
    assert_matches_git(&pair);
}

#[test]
fn fast_forward_merge_keeps_new_out_of_cone_paths_skip_worktree() {
    let pair = sparse_pair("merge-ff", false);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        git(dir, &["reset", "-q", "--hard", "HEAD~1"]);
    }
    git(&pair.git_dir, &["merge", "-q", "--ff-only", "side"]);
    let repo = open(&pair.grit_dir);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        rev(&pair.grit_dir, "HEAD"),
        rev(&pair.grit_dir, "side"),
        "side",
    )
    .expect("grit merge");
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { .. }
    ));

    assert_b_not_materialized(&pair.grit_dir);
    assert_matches_git(&pair);
}

#[test]
fn three_way_merge_keeps_new_out_of_cone_paths_skip_worktree() {
    let pair = sparse_pair("merge-3way", false);
    git(&pair.git_dir, &["merge", "-q", "--no-edit", "side"]);
    let repo = open(&pair.grit_dir);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        rev(&pair.grit_dir, "HEAD"),
        rev(&pair.grit_dir, "side"),
        "side",
    )
    .expect("grit merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Merged { .. }));

    assert_b_not_materialized(&pair.grit_dir);
    assert_eq!(
        rev(&pair.grit_dir, "HEAD^{tree}"),
        rev(&pair.git_dir, "HEAD^{tree}")
    );
    assert!(git(&pair.grit_dir, &["ls-files", "-t"]).contains("S b/new\n"));
    assert_matches_git(&pair);
}

#[test]
fn conflicted_out_of_cone_path_is_materialized_like_git() {
    let pair = sparse_pair("merge-conflict", false);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        git(dir, &["reset", "-q", "--hard", "ours"]);
    }
    let failed = git_cmd(&["merge", "-q", "--no-edit", "theirs"])
        .in_dir(&pair.git_dir)
        .exec();
    assert!(!failed.ok(), "git merge must stop on conflicts");
    let repo = open(&pair.grit_dir);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        rev(&pair.grit_dir, "HEAD"),
        rev(&pair.grit_dir, "theirs"),
        "theirs",
    )
    .expect("grit merge");
    let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert_eq!(paths, ["a/f", "b/f"]);

    let grit = observe(&pair.grit_dir);
    let oracle = observe(&pair.git_dir);
    assert_eq!(grit.files, oracle.files);
    assert_eq!(grit.stages, oracle.stages);
    assert_eq!(grit.tags, oracle.tags);
    assert_eq!(grit.status, oracle.status);
}

#[test]
fn restore_from_source_matches_git() {
    let pair = sparse_pair("restore", false);
    git(&pair.git_dir, &["restore", "--source", "HEAD~2", "a"]);
    let repo = open(&pair.grit_dir);
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["a".into()],
            pathspec_sources: vec!["a".into()],
            source: RestoreSource::Tree(rev(&pair.grit_dir, "HEAD~2^{tree}")),
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    )
    .expect("grit restore");

    assert_b_not_materialized(&pair.grit_dir);
    assert_matches_git(&pair);
}

fn pull_pair(tag: &str, local_commit: bool) -> (PathBuf, Pair) {
    let root = unique_tmp("sparse-checkout-pull", tag);
    let origin = root.join("origin");
    seed_history(&origin);
    git(&origin, &["reset", "-q", "--hard", "HEAD~1"]);
    let seed = root.join("seed");
    git(
        &root,
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &seed.to_string_lossy(),
        ],
    );
    identity(&seed);
    enable_cone(&seed, false);
    if local_commit {
        put(&seed, "a/local", "local\n");
        commit_all(&seed, "local");
    }
    git(&origin, &["merge", "-q", "--ff-only", "side"]);
    let pair = Pair {
        git_dir: root.join("git"),
        grit_dir: root.join("grit"),
    };
    copy_dir(&seed, &pair.git_dir);
    copy_dir(&seed, &pair.grit_dir);
    (origin, pair)
}

fn grit_pull(dir: &Path, mode: PullMode) {
    let repo = open(dir);
    let mut env = Environment::empty();
    env.cwd = dir.to_path_buf();
    pull(
        &repo,
        &PullOptions {
            mode: Some(mode),
            fetch: FetchOptions {
                tags: TagMode::Following,
                environment: env,
                ..Default::default()
            },
            ..PullOptions::default()
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit pull");
}

#[test]
fn pull_fast_forward_keeps_out_of_cone_paths_skip_worktree() {
    let (_origin, pair) = pull_pair("ff", false);
    git(&pair.git_dir, &["pull", "-q", "--ff-only"]);
    grit_pull(&pair.grit_dir, PullMode::FfOnly);

    assert_b_not_materialized(&pair.grit_dir);
    assert_matches_git(&pair);
}

#[test]
fn pull_rebase_keeps_out_of_cone_paths_skip_worktree() {
    let (_origin, pair) = pull_pair("rebase", true);
    git(&pair.git_dir, &["pull", "-q", "--rebase"]);
    grit_pull(&pair.grit_dir, PullMode::Rebase);

    assert_b_not_materialized(&pair.grit_dir);
    assert_eq!(
        rev(&pair.grit_dir, "HEAD^{tree}"),
        rev(&pair.git_dir, "HEAD^{tree}")
    );
    assert_matches_git(&pair);
}

#[test]
fn sparse_index_survives_reset_hard_and_switch() {
    let pair = sparse_pair("sparse-index", true);
    assert!(
        git(&pair.grit_dir, &["ls-files", "--sparse"]).contains("b/\n"),
        "setup must start from a collapsed sparse index"
    );
    git(&pair.git_dir, &["reset", "-q", "--hard", "HEAD~2"]);
    git(&pair.git_dir, &["switch", "-q", "side"]);
    let repo = open(&pair.grit_dir);
    reset(&repo, rev(&pair.grit_dir, "HEAD~2"), ResetMode::Hard);
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "side".into(),
                oid: rev(&pair.grit_dir, "side"),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit switch");

    assert_b_not_materialized(&pair.grit_dir);
    assert!(git(&pair.grit_dir, &["ls-files", "--sparse"]).contains("b/\n"));
    assert_matches_git(&pair);
}

fn put_untracked_out_of_cone_file(pair: &Pair, contents: &str) {
    for dir in [&pair.git_dir, &pair.grit_dir] {
        put(dir, "b/new", contents);
    }
}

fn assert_untracked_file_kept(pair: &Pair, contents: &str) {
    assert_eq!(
        fs::read_to_string(pair.grit_dir.join("b/new")).expect("b/new"),
        contents
    );
    assert_matches_git(pair);
}

#[test]
fn switch_does_not_refuse_untracked_file_at_skipped_path() {
    for (tag, contents, sparse_index) in [
        ("switch-untracked", "mine\n", false),
        ("switch-same", "n\n", false),
        ("switch-untracked-sparse-index", "mine\n", true),
    ] {
        let pair = sparse_pair(tag, sparse_index);
        put_untracked_out_of_cone_file(&pair, contents);
        git(&pair.git_dir, &["switch", "-q", "side"]);
        let repo = open(&pair.grit_dir);
        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "side".into(),
                    oid: rev(&pair.grit_dir, "side"),
                },
                dry_run: false,
                hooks: grit_lib::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("grit switch");
        assert_untracked_file_kept(&pair, contents);
    }
}

#[test]
fn merges_do_not_refuse_untracked_file_at_skipped_path() {
    let ff = sparse_pair("merge-ff-untracked", false);
    for dir in [&ff.git_dir, &ff.grit_dir] {
        git(dir, &["reset", "-q", "--hard", "HEAD~1"]);
    }
    let three_way = sparse_pair("merge-3way-untracked", false);
    for (pair, git_args) in [
        (&ff, ["merge", "-q", "--ff-only", "side"]),
        (&three_way, ["merge", "-q", "--no-edit", "side"]),
    ] {
        put_untracked_out_of_cone_file(pair, "mine\n");
        git(&pair.git_dir, &git_args);
        let repo = open(&pair.grit_dir);
        integrate_branch(
            &repo,
            "refs/heads/main",
            rev(&pair.grit_dir, "HEAD"),
            rev(&pair.grit_dir, "side"),
            "side",
        )
        .expect("grit merge");
        assert_untracked_file_kept(pair, "mine\n");
    }
}

/// `x/sub` is the cone, so `x/` is a parent directory: `x/top` is in the cone, `x/other/*` is not.
/// Branch `wide` deletes every file under `b/` and `x/` and adds `b/new` and `x/other/new`, which
/// makes both top-level directories wholesale deletions for switch.
#[test]
fn switch_with_wholesale_deleted_directories_keeps_new_skip_worktree_rows() {
    let root = unique_tmp("sparse-checkout-writes", "wholesale");
    let seed = root.join("seed");
    fs::create_dir_all(&seed).expect("mkdir");
    git(&seed, &["init", "-q", "-b", "main"]);
    identity(&seed);
    put(&seed, "top", "1\n");
    for i in 0..10 {
        put(&seed, &format!("b/f{i}"), &format!("{i}\n"));
        put(&seed, &format!("x/other/f{i}"), &format!("{i}\n"));
    }
    put(&seed, "x/top", "1\n");
    put(&seed, "x/sub/f", "1\n");
    commit_all(&seed, "wide base");
    git(&seed, &["checkout", "-q", "-b", "wide"]);
    git(&seed, &["rm", "-q", "-r", "b", "x"]);
    put(&seed, "b/new", "n\n");
    put(&seed, "x/other/new", "n\n");
    commit_all(&seed, "wide");
    git(&seed, &["checkout", "-q", "main"]);
    git(&seed, &["sparse-checkout", "set", "--cone", "x/sub"]);
    let pair = Pair {
        git_dir: root.join("git"),
        grit_dir: root.join("grit"),
    };
    copy_dir(&seed, &pair.git_dir);
    copy_dir(&seed, &pair.grit_dir);

    git(&pair.git_dir, &["switch", "-q", "wide"]);
    let repo = open(&pair.grit_dir);
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "wide".into(),
                oid: rev(&pair.grit_dir, "wide"),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit switch");

    assert_b_not_materialized(&pair.grit_dir);
    assert_eq!(
        git(&pair.grit_dir, &["ls-files", "-t"]),
        "S b/new\nH top\nS x/other/new\n"
    );
    assert_matches_git(&pair);
}

#[test]
fn non_sparse_reset_hard_still_writes_every_path() {
    let root = unique_tmp("sparse-checkout-writes", "full");
    seed_history(&root);
    let repo = open(&root);
    assert!(SparseCheckout::load(&repo).expect("load").is_none());
    reset(&repo, rev(&root, "HEAD~2"), ResetMode::Hard);
    assert_eq!(fs::read(root.join("b/f")).expect("b/f"), b"1\n");
    assert!(!root.join("b/g").exists());
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
}

/// Rust equivalents of t1092's hard-reset and checkout sparse-directory cases.
#[test]
fn git_sparse_index_nested_transitions_and_dirty_head_reset() {
    let root = unique_tmp("sparse-checkout-writes", "nested-transitions");
    let seed = root.join("seed");
    fs::create_dir_all(&seed).expect("mkdir");
    git(&seed, &["init", "-q", "-b", "main"]);
    identity(&seed);
    for path in ["deep/visible/f", "deep/hidden/f", "excluded/f", "top"] {
        put(&seed, path, "base\n");
    }
    commit_all(&seed, "base");
    git(&seed, &["tag", "base"]);
    put(&seed, "deep/visible/f", "edited\n");
    put(&seed, "deep/hidden/f", "edited\n");
    fs::remove_dir_all(seed.join("excluded")).expect("delete excluded subtree");
    put(&seed, "new-excluded/f", "new\n");
    commit_all(&seed, "edited");
    git(&seed, &["tag", "edited"]);
    git(
        &seed,
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--sparse-index",
            "deep/visible",
        ],
    );
    let pair = Pair {
        git_dir: root.join("git"),
        grit_dir: root.join("grit"),
    };
    copy_dir(&seed, &pair.git_dir);
    copy_dir(&seed, &pair.grit_dir);
    for target in ["base", "edited", "base"] {
        git(&pair.git_dir, &["reset", "--hard", target]);
        reset(
            &open(&pair.grit_dir),
            rev(&pair.grit_dir, target),
            ResetMode::Hard,
        );
        assert_matches_git(&pair);
        git(
            &pair.git_dir,
            &[
                "switch",
                "--detach",
                if target == "base" { "edited" } else { "base" },
            ],
        );
        let other = if target == "base" { "edited" } else { "base" };
        switch_repository(
            &open(&pair.grit_dir),
            &SwitchOptions {
                mode: SwitchMode::Detached {
                    commit: rev(&pair.grit_dir, other),
                    reflog_to_label: other.into(),
                },
                dry_run: false,
                hooks: grit_lib::PostOperationHooks::Skip,
            },
            &mut grit_lib::progress::NullProgress,
            &NeverCancel,
        )
        .expect("switch");
        assert_matches_git(&pair);
    }
    for dir in [&pair.git_dir, &pair.grit_dir] {
        put(dir, "deep/visible/f", "staged\n");
        git(dir, &["add", "deep/visible/f"]);
        put(dir, "deep/visible/f", "unstaged\n");
        put(dir, "deep/hidden/f", "present outside cone\n");
    }
    git(&pair.git_dir, &["reset", "--hard", "HEAD"]);
    reset(
        &open(&pair.grit_dir),
        rev(&pair.grit_dir, "HEAD"),
        ResetMode::Hard,
    );
    assert_matches_git(&pair);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        assert_eq!(
            git(dir, &["write-tree"]).trim(),
            rev(dir, "HEAD^{tree}").to_string()
        );
        git(dir, &["reset", "--hard", "edited"]);
        git(dir, &["switch", "--detach", "base"]);
        git(dir, &["fsck", "--strict"]);
    }
    assert_matches_git(&pair);
}

#[test]
fn sparse_index_switch_preserves_staged_and_unstaged_excluded_changes() {
    let pair = sparse_pair("staged-excluded-switch", true);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        put(dir, "b/f", "staged outside cone\n");
        git(dir, &["add", "--sparse", "b/f"]);
        put(dir, "b/f", "unstaged outside cone\n");
    }
    git(&pair.git_dir, &["switch", "--detach", "HEAD~1"]);
    switch_repository(
        &open(&pair.grit_dir),
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: rev(&pair.grit_dir, "HEAD~1"),
                reflog_to_label: "HEAD~1".into(),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Skip,
        },
        &mut grit_lib::progress::NullProgress,
        &NeverCancel,
    )
    .expect("switch");
    assert_matches_git(&pair);
    assert_eq!(
        fs::read(pair.grit_dir.join("b/f")).expect("present file"),
        b"unstaged outside cone\n"
    );
}

#[test]
fn sparse_index_file_directory_and_gitlink_transitions_match_git() {
    let root = unique_tmp("sparse-checkout-writes", "sparse-types");
    let seed = root.join("seed");
    fs::create_dir_all(&seed).expect("mkdir");
    git(&seed, &["init", "-q", "-b", "main"]);
    identity(&seed);
    put(&seed, "a/f", "visible\n");
    put(&seed, "hidden/nested/f", "hidden\n");
    commit_all(&seed, "base");
    let link = rev(&seed, "HEAD").to_string();
    git(
        &seed,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{link},modules/sub"),
        ],
    );
    git(&seed, &["commit", "-q", "-m", "gitlink"]);
    git(&seed, &["tag", "base"]);
    fs::remove_dir_all(seed.join("hidden")).expect("remove hidden directory");
    put(&seed, "hidden", "replacement file\n");
    put(&seed, "a/f", "changed visible\n");
    // Leave the uninitialized gitlink unchanged in both commits.
    git(&seed, &["add", "-A", "--", "a", "hidden"]);
    git(&seed, &["commit", "-q", "-m", "types"]);
    git(&seed, &["tag", "types"]);
    git(
        &seed,
        &["sparse-checkout", "set", "--cone", "--sparse-index", "a"],
    );
    // Start from a Git-written sparse index that includes an opaque gitlink subtree.
    git(&seed, &["read-tree", "--empty"]);
    git(&seed, &["reset", "--hard", "HEAD"]);
    assert!(git(&seed, &["ls-files", "--sparse"]).contains("modules/\n"));
    let pair = Pair {
        git_dir: root.join("git"),
        grit_dir: root.join("grit"),
    };
    copy_dir(&seed, &pair.git_dir);
    copy_dir(&seed, &pair.grit_dir);
    for target in ["base", "types", "base"] {
        git(&pair.git_dir, &["reset", "--hard", target]);
        reset(
            &open(&pair.grit_dir),
            rev(&pair.grit_dir, target),
            ResetMode::Hard,
        );
        assert_matches_git(&pair);
    }
    git(&pair.git_dir, &["switch", "--detach", "types"]);
    switch_repository(
        &open(&pair.grit_dir),
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: rev(&pair.grit_dir, "types"),
                reflog_to_label: "types".into(),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Skip,
        },
        &mut grit_lib::progress::NullProgress,
        &NeverCancel,
    )
    .expect("switch");
    assert_matches_git(&pair);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        git(dir, &["fsck", "--strict"]);
    }
}

#[test]
fn sparse_head_reset_honors_expect_files_outside_patterns() {
    let pair = sparse_pair("expect-outside-patterns", true);
    for dir in [&pair.git_dir, &pair.grit_dir] {
        git(
            dir,
            &["config", "sparse.expectFilesOutsideOfPatterns", "true"],
        );
        put(dir, "b/f", "left outside cone\n");
    }
    git(&pair.git_dir, &["reset", "--hard", "HEAD"]);
    reset(
        &open(&pair.grit_dir),
        rev(&pair.grit_dir, "HEAD"),
        ResetMode::Hard,
    );
    assert_matches_git(&pair);
    assert_eq!(
        fs::read(pair.grit_dir.join("b/f")).expect("present file"),
        b"left outside cone\n"
    );
}
