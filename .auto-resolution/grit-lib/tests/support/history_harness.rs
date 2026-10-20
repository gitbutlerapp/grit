//! Hermetic system-`git` helpers for history (rev-list, rev-parse, index) tests.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Result;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{
    rev_list, split_revision_token, ObjectFilter, OrderingMode, RevListOptions,
};
use grit_lib::revwalk::{parse_rev_walk_tokens, RevWalkParsedSpecs};

use super::refs_harness::{git as refs_git, null_device_for_tests, AUTHOR_EMAIL, AUTHOR_NAME};

thread_local! {
    static COMMIT_TIME: Cell<u64> = const { Cell::new(1_700_000_000) };
}

/// Reset the monotonic harness author/committer clock (for paired git/grit repo fixtures).
pub fn reset_harness_commit_clock() {
    COMMIT_TIME.with(|t| t.set(1_700_000_000));
}

fn next_commit_date() -> String {
    COMMIT_TIME.with(|t| {
        let secs = t.get();
        t.set(secs.saturating_add(60));
        format!("{secs} +0000")
    })
}

fn git_command_advances_harness_clock(args: &[&str]) -> bool {
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-c" | "-C" | "--git-dir" | "--work-tree" => {
                i += 2;
            }
            cmd => {
                return !matches!(
                    cmd,
                    "rev-parse"
                        | "cat-file"
                        | "fsck"
                        | "log"
                        | "show"
                        | "status"
                        | "diff"
                        | "ls-files"
                        | "ls-tree"
                        | "help"
                        | "merge-base"
                        | "symbolic-ref"
                );
            }
        }
    }
    true
}

fn hermetic_git(dir: &Path, args: &[&str]) -> std::process::Output {
    let date = if git_command_advances_harness_clock(args) {
        next_commit_date()
    } else {
        peek_commit_date()
    };
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .output()
        .expect("spawn git")
}

/// Start a merge that is expected to stop with conflicts (non-zero exit, index left unmerged).
pub fn git_merge_expect_conflict(dir: &Path, branch: &str) {
    let out = hermetic_git(dir, &["merge", branch, "--no-commit", "--no-ff"]);
    assert!(
        !out.status.success(),
        "git merge {branch} should fail with conflicts: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run `git` in `dir` with hermetic config and monotonically advancing author/committer dates.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = hermetic_git(dir, args);
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Parsed `(major, minor)` from `git --version`, or `None` if git is missing.
pub fn git_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = Command::new("git").arg("--version").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // "git version 2.45.0"
        let token = text.split_whitespace().nth(2)?;
        let mut parts = token.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next().unwrap_or("0").parse().ok()?;
        Some((major, minor))
    })
}

/// Returns `true` when system git is at least `major.minor`.
#[must_use]
pub fn git_version_at_least(major: u32, minor: u32) -> bool {
    git_version().is_some_and(|(maj, min)| (maj, min) >= (major, minor))
}

/// Skip (return `false`) when system git is older than `major.minor`.
#[must_use]
pub fn skip_unless_git_at_least(major: u32, minor: u32) -> bool {
    if git_version_at_least(major, minor) {
        return true;
    }
    eprintln!("SKIP: system git >= {major}.{minor} required");
    false
}

/// True when `git history` stderr means the command or subcommand is absent (not a usage error).
#[must_use]
pub fn git_history_stderr_indicates_unavailable(stderr: &str) -> bool {
    stderr.contains("is not a git command") || stderr.contains("unknown subcommand")
}

/// Whether the system git implements the `git history` command (Git >= 2.54 and help succeeds).
#[must_use]
pub fn git_history_command_available() -> bool {
    if !git_version_at_least(2, 54) {
        return false;
    }
    let Ok(out) = Command::new("git").args(["help", "history"]).output() else {
        return false;
    };
    if out.status.success() {
        return true;
    }
    let Ok(out) = Command::new("git").args(["history"]).output() else {
        return false;
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    !git_history_stderr_indicates_unavailable(&stderr)
}

/// Whether `git history <subcommand>` exists (positive `-h` check after command probe).
#[must_use]
pub fn git_history_subcommand_available(subcommand: &str) -> bool {
    if !git_history_command_available() {
        return false;
    }
    let Ok(out) = Command::new("git")
        .args(["history", subcommand, "-h"])
        .output()
    else {
        return false;
    };
    if out.status.success() {
        return true;
    }
    // Git prints usage to stdout for `-h` but may exit 129.
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.contains("usage: git history") && stdout.contains(subcommand)
}

fn peek_commit_date() -> String {
    COMMIT_TIME.with(|t| format!("{} +0000", t.get()))
}

/// Committer line for the next [`git`] invocation without advancing the harness clock.
///
/// Pair with [`git_history_fixup_or_legacy`] so grit fixup and system git use the same committer timestamp.
#[must_use]
pub fn harness_committer_line() -> String {
    format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}> {}", peek_commit_date())
}

fn git_status_has_unstaged_changes(dir: &Path) -> bool {
    let out = hermetic_git(dir, &["status", "--porcelain"]);
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.len() >= 2 && line.as_bytes()[1] != b' ')
}

/// Git 2.45 fallback: `commit --fixup` and autosquash rebase without rewriting unchanged ancestors.
pub fn git_history_fixup_legacy_oracle(dir: &Path, target: &str) {
    let upstream = git_rev_parse(dir, &format!("{target}^"))
        .map(|parent| parent.to_string())
        .unwrap_or_else(|| "--root".to_owned());
    git(dir, &["commit", "--fixup", target]);
    let stashed = if git_status_has_unstaged_changes(dir) {
        git(dir, &["stash", "push", "-m", "grit-fixup-parity"]);
        true
    } else {
        false
    };
    git(
        dir,
        &[
            "-c",
            "sequence.editor=true",
            "rebase",
            "-i",
            "--autosquash",
            &upstream,
        ],
    );
    if stashed {
        git(dir, &["stash", "pop"]);
    }
}

/// Run system `git history fixup` when available, else [`git_history_fixup_legacy_oracle`].
pub fn git_history_fixup_or_legacy(dir: &Path, target: &str) {
    if git_history_subcommand_available("fixup") {
        git(dir, &["history", "fixup", target]);
        return;
    }
    git_history_fixup_legacy_oracle(dir, target);
}

/// Assert fixup `HEAD` tree, subject, and parent OIDs match between two repos.
pub fn assert_fixup_tip_structure_matches(grit_dir: &Path, git_dir: &Path) {
    assert_eq!(
        git(grit_dir, &["rev-parse", "HEAD^{tree}"]).trim(),
        git(git_dir, &["rev-parse", "HEAD^{tree}"]).trim(),
        "fixup HEAD tree"
    );
    assert_eq!(
        git(grit_dir, &["log", "-1", "--format=%s"]).trim(),
        git(git_dir, &["log", "-1", "--format=%s"]).trim(),
        "fixup HEAD subject"
    );
    assert_eq!(
        git(grit_dir, &["log", "-1", "--format=%P"]).trim(),
        git(git_dir, &["log", "-1", "--format=%P"]).trim(),
        "fixup HEAD parents"
    );
}

/// Assert grit and git fixup results match; full commit OID only when `git history fixup` exists.
pub fn assert_fixup_tip_matches_git_oracle(grit_dir: &Path, git_dir: &Path) {
    assert_fixup_tip_structure_matches(grit_dir, git_dir);
    if git_history_subcommand_available("fixup") {
        assert_eq!(
            git_rev_parse(grit_dir, "HEAD").expect("grit HEAD"),
            git_rev_parse(git_dir, "HEAD").expect("git HEAD"),
            "fixup HEAD oid (git history fixup)"
        );
    }
}

#[cfg(test)]
mod git_history_probe_tests {
    use super::git_history_stderr_indicates_unavailable;

    #[test]
    fn stderr_missing_history_command() {
        assert!(git_history_stderr_indicates_unavailable(
            "git: 'history' is not a git command. See 'git --help'."
        ));
    }

    #[test]
    fn stderr_unknown_subcommand() {
        assert!(git_history_stderr_indicates_unavailable(
            "error: unknown subcommand: `drop'"
        ));
    }

    #[test]
    fn stderr_usage_error_is_not_unavailable() {
        assert!(!git_history_stderr_indicates_unavailable(
            "error: command expects a single revision"
        ));
    }
}

/// Temporary repository initialized with system `git`.
pub struct HistoryRepo {
    _root: tempfile::TempDir,
    worktree: PathBuf,
}

impl HistoryRepo {
    /// Working tree path (contains `.git`).
    #[must_use]
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// Initialize a new repo under a temp directory (`main` as initial branch when supported).
    pub fn init() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().to_path_buf();
        git(&worktree, &["init", "-q"]);
        if git_version_at_least(2, 28) {
            let _ = hermetic_git(&worktree, &["branch", "-M", "main"]);
        }
        Self {
            _root: root,
            worktree,
        }
    }
}

/// Write `content` at `path`, stage, and commit with `msg`; returns the new commit oid.
pub fn commit_file(worktree: &Path, path: &str, content: &str, msg: &str) -> ObjectId {
    let full = worktree.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&full, content).expect("write");
    git(worktree, &["add", path]);
    git(worktree, &["commit", "-m", msg]);
    git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD oid")
}

/// Build a commit DAG from `edges` (`(child_label, parent_label)`); optional `tags` map labels to ref names.
///
/// Root commits have no incoming edge as a child whose parent is missing; labels only referenced as
/// parents are created as empty commits first.
pub fn build_commit_dag(
    worktree: &Path,
    edges: &[(&str, &str)],
    tags: &[(&str, &str)],
) -> HashMap<String, ObjectId> {
    let mut labels: HashSet<&str> = HashSet::new();
    for (child, parent) in edges {
        labels.insert(child);
        labels.insert(parent);
    }
    for (label, _) in tags {
        labels.insert(label);
    }

    let mut oids: HashMap<String, ObjectId> = HashMap::new();
    let mut remaining: HashSet<&str> = labels.iter().copied().collect();

    while !remaining.is_empty() {
        let ready: Vec<&str> = remaining
            .iter()
            .copied()
            .filter(|label| {
                let parents: Vec<&str> = edges
                    .iter()
                    .filter(|(child, _)| child == label)
                    .map(|(_, parent)| *parent)
                    .collect();
                parents.is_empty() || parents.iter().all(|p| oids.contains_key(*p))
            })
            .collect();
        if ready.is_empty() {
            panic!("commit DAG has unsatisfiable edges: {edges:?}");
        }
        for label in ready {
            remaining.remove(label);
            let parents: Vec<ObjectId> = edges
                .iter()
                .filter(|(child, _)| *child == label)
                .map(|(_, parent_label)| oids[*parent_label])
                .collect();
            let oid = commit_with_parents(worktree, label, &parents);
            oids.insert(label.to_owned(), oid);
        }
    }

    for (label, refname) in tags {
        let oid = oids
            .get(*label)
            .unwrap_or_else(|| panic!("unknown label {label} for tag"));
        git(worktree, &["update-ref", refname, &oid.to_hex()]);
    }

    oids
}

fn commit_with_parents(worktree: &Path, label: &str, parents: &[ObjectId]) -> ObjectId {
    let msg = format!("history harness {label}");
    let tree = git(worktree, &["mktree"]).trim().to_string();
    let mut args: Vec<String> = vec!["commit-tree".to_owned(), tree, "-m".to_owned(), msg];
    let parent_hex: Vec<String> = parents.iter().map(ObjectId::to_hex).collect();
    for hex in &parent_hex {
        args.push("-p".to_owned());
        args.push(hex.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let commit = git(worktree, &arg_refs).trim().to_string();
    let refname = format!("refs/history/{label}");
    git(worktree, &["update-ref", &refname, &commit]);
    commit.parse().expect("commit oid")
}

/// `git rev-list` in `dir`, returning object ids in git's order (one oid per line).
pub fn git_rev_list(dir: &Path, args: &[&str]) -> Vec<ObjectId> {
    let mut full: Vec<&str> = vec!["rev-list"];
    full.extend_from_slice(args);
    let out = refs_git(dir, &full);
    parse_rev_list_lines(&out)
}

fn parse_rev_list_lines(out: &str) -> Vec<ObjectId> {
    out.lines()
        .filter_map(|line| {
            let token = line.split_whitespace().next().unwrap_or(line).trim();
            parse_rev_list_token(token)
        })
        .collect()
}

fn parse_rev_list_token(token: &str) -> Option<ObjectId> {
    let token = token.trim();
    let token = token
        .strip_prefix('-')
        .or_else(|| token.strip_prefix('<').or_else(|| token.strip_prefix('>')))
        .or_else(|| token.strip_prefix('='))
        .unwrap_or(token);
    let token = token.strip_prefix('?').unwrap_or(token);
    token.split_whitespace().next()?.parse().ok()
}

/// Raw `git rev-list` lines (includes `-`, `<`, `>`, `=` prefixes when requested).
pub fn git_rev_list_lines(worktree: &Path, args: &[&str]) -> Vec<String> {
    let mut full: Vec<&str> = vec!["rev-list"];
    full.extend_from_slice(args);
    refs_git(worktree, &full)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `git rev-list` lines with boundary commits (`-` prefix) preserved.
pub fn git_rev_list_with_boundary(dir: &Path, args: &[&str]) -> Vec<(bool, ObjectId)> {
    let mut full: Vec<&str> = vec!["rev-list"];
    full.extend_from_slice(args);
    let out = refs_git(dir, &full);
    out.lines()
        .filter_map(|line| {
            let token = line.split_whitespace().next()?.trim();
            let boundary = token.starts_with('-');
            parse_rev_list_token(token).map(|oid| (boundary, oid))
        })
        .collect()
}

/// Resolve git tag names (or other revs) to commit OIDs in order.
pub fn rev_parse_many(dir: &Path, specs: &[&str]) -> Vec<ObjectId> {
    specs
        .iter()
        .map(|spec| git_rev_parse(dir, spec).unwrap_or_else(|| panic!("git rev-parse {spec}")))
        .collect()
}

/// Run grit `rev_list` on a discovered repo and return commit OIDs.
pub fn grit_rev_list_commits(
    worktree: &Path,
    positive: &[String],
    negative: &[String],
    options: &RevListOptions,
) -> Result<Vec<ObjectId>> {
    let repo = Repository::discover(Some(worktree), &RepositoryOptions::empty())?;
    grit_rev_list_commits_in_repo(&repo, positive, negative, options)
}

/// Grit `rev_list` commit OID sequence for an already-open repository.
pub fn grit_rev_list_commits_in_repo(
    repo: &Repository,
    positive: &[String],
    negative: &[String],
    options: &RevListOptions,
) -> Result<Vec<ObjectId>> {
    Ok(rev_list(repo, positive, negative, options)?.commits)
}

/// Assert grit commit order matches `git rev-list` with the same argv tail (after `rev-list`).
pub fn assert_grit_commits_match_git(
    worktree: &Path,
    git_args: &[&str],
    grit_positive: &[String],
    grit_negative: &[String],
    grit_options: &RevListOptions,
) {
    let git_oids = git_rev_list(worktree, git_args);
    let grit_oids = grit_rev_list_commits(worktree, grit_positive, grit_negative, grit_options)
        .expect("grit rev-list");
    assert_eq!(
        grit_oids,
        git_oids,
        "grit rev-list commits differ from git rev-list {}",
        git_args.join(" ")
    );
}

/// Object OIDs from `git rev-list --objects --no-object-names` (optional `--filter=`).
pub fn git_rev_list_object_set(dir: &Path, args: &[&str]) -> HashSet<ObjectId> {
    let mut full: Vec<&str> = vec!["rev-list", "--objects", "--no-object-names"];
    full.extend_from_slice(args);
    let out = refs_git(dir, &full);
    parse_rev_list_lines(&out).into_iter().collect()
}

/// Object OIDs from grit `rev_list` with `--objects`.
pub fn grit_rev_list_object_set(
    worktree: &Path,
    positive: &[String],
    negative: &[String],
    filter: Option<ObjectFilter>,
) -> HashSet<ObjectId> {
    let repo = Repository::discover(Some(worktree), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        objects: true,
        no_object_names: true,
        quiet: true,
        filter,
        ..RevListOptions::default()
    };
    let result = rev_list(&repo, positive, negative, &opts).expect("rev-list objects");
    let mut set = HashSet::new();
    for oid in result.commits {
        set.insert(oid);
    }
    for (oid, _) in result.objects {
        set.insert(oid);
    }
    set
}

/// Map common git ordering flags to [`OrderingMode`].
pub fn ordering_from_git_args(args: &[&str]) -> OrderingMode {
    let topo = args.iter().any(|a| *a == "--topo-order");
    let date = args.iter().any(|a| *a == "--date-order");
    let author = args.iter().any(|a| *a == "--author-date-order");
    match (topo, date, author) {
        (true, _, true) => OrderingMode::AuthorDateTopo,
        (true, _, false) => OrderingMode::Topo,
        (false, true, true) => OrderingMode::AuthorDateWalk,
        (false, true, false) => OrderingMode::DateOrderWalk,
        (false, false, true) => OrderingMode::AuthorDateWalk,
        _ => OrderingMode::Default,
    }
}

/// `git rev-parse` for `spec`, or `None` when git rejects the spec.
pub fn git_rev_parse(dir: &Path, spec: &str) -> Option<ObjectId> {
    let out = hermetic_git(dir, &["rev-parse", spec]);
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// `git rev-parse --verify -q` for `spec` (plumbing verify, no DWIM stdout noise).
pub fn git_rev_parse_verify(dir: &Path, spec: &str) -> Option<ObjectId> {
    let out = hermetic_git(dir, &["rev-parse", "--verify", "-q", spec]);
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Run `git merge-base` with arguments; returns OIDs in git's order (one per line).
pub fn git_merge_base(dir: &Path, args: &[&str]) -> Vec<ObjectId> {
    let mut full: Vec<&str> = vec!["merge-base"];
    full.extend_from_slice(args);
    let out = git(dir, &full);
    out.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// `git merge-base --is-ancestor A B` exit status (true when ancestor).
pub fn git_merge_base_is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    hermetic_git(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
        .status
        .success()
}

/// `git ls-files -s` as `(path, stage, mode, oid)` tuples.
pub fn git_ls_files_stage(dir: &Path) -> Vec<(String, u32, String, ObjectId)> {
    let out = git(dir, &["ls-files", "-s"]);
    let mut rows = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let mode = parts.next().unwrap_or("").to_string();
        let oid: ObjectId = parts.next().unwrap_or("").parse().expect("oid");
        let stage: u32 = parts.next().unwrap_or("0").parse().expect("stage");
        let path = parts.collect::<Vec<_>>().join(" ");
        rows.push((path, stage, mode, oid));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    rows
}

/// Index stage map keyed by path (stage 0 only), for quick comparisons.
pub fn git_ls_files_stage0_map(dir: &Path) -> BTreeMap<String, ObjectId> {
    git_ls_files_stage(dir)
        .into_iter()
        .filter(|(_, stage, _, _)| *stage == 0)
        .map(|(path, _, _, oid)| (path, oid))
        .collect()
}

/// Open a grit [`Repository`] for a harness work tree.
pub fn open_grit_repo(worktree: &Path) -> Repository {
    Repository::discover(Some(worktree), &RepositoryOptions::empty()).expect("discover grit repo")
}

/// Write a reachable commit-graph with system `git` (matches CI / user repos).
pub fn git_commit_graph_write(worktree: &Path) {
    let out = hermetic_git(
        worktree,
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    assert!(
        out.status.success(),
        "git commit-graph write: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Create an empty-tree commit with explicit author/committer dates (`commit-tree`).
pub fn commit_labeled_with_dates(
    worktree: &Path,
    label: &str,
    parents: &[ObjectId],
    author_date: &str,
    committer_date: &str,
) -> ObjectId {
    let tree = git(worktree, &["mktree"]).trim().to_string();
    let msg = format!("history harness {label}");
    let mut args: Vec<String> = vec!["commit-tree".to_owned(), tree, "-m".to_owned(), msg];
    for parent in parents {
        args.push("-p".to_owned());
        args.push(parent.to_hex());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = Command::new("git")
        .current_dir(worktree)
        .args(&arg_refs)
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env(
            "GIT_AUTHOR_DATE",
            if author_date.starts_with('@') {
                author_date.to_owned()
            } else {
                format!("@{author_date}")
            },
        )
        .env(
            "GIT_COMMITTER_DATE",
            if committer_date.starts_with('@') {
                committer_date.to_owned()
            } else {
                format!("@{committer_date}")
            },
        )
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .output()
        .expect("spawn git commit-tree");
    assert!(
        out.status.success(),
        "commit-tree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let refname = format!("refs/history/{label}");
    git(worktree, &["update-ref", &refname, &commit]);
    commit.parse().expect("commit oid")
}

/// Split CLI revision tokens into positive/negative specs (`A..B`, `^rev`, plain tips).
#[must_use]
pub fn expand_revision_tokens(tokens: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for token in tokens {
        let (pos, neg) = split_revision_token(token);
        positive.extend(pos);
        negative.extend(neg);
    }
    (positive, negative)
}

/// Expand one token that may use `A...B` symmetric-diff syntax.
pub fn expand_token_for_rev_list(
    repo: &Repository,
    token: &str,
    opts: &mut RevListOptions,
) -> Result<(Vec<String>, Vec<String>)> {
    let RevWalkParsedSpecs {
        positive,
        negative,
        symmetric_left,
        symmetric_right,
    } = parse_rev_walk_tokens(repo, &[token])?;
    opts.symmetric_left = symmetric_left;
    opts.symmetric_right = symmetric_right;
    Ok((positive, negative))
}

/// Assert grit commit order matches `git rev-list` for both commit-graph modes.
pub fn assert_rev_list_commits_match_git(
    worktree: &Path,
    repo: &Repository,
    git_args: &[&str],
    positive: &[String],
    negative: &[String],
    mut opts: RevListOptions,
    context: &str,
) {
    let git_oids = git_rev_list(worktree, git_args);
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(worktree);
        }
        opts.use_commit_graph = use_graph;
        let grit_oids = grit_rev_list_commits_in_repo(repo, positive, negative, &opts)
            .unwrap_or_else(|e| panic!("grit rev_list failed ({context}, graph={use_graph}): {e}"));
        assert_eq!(
            grit_oids, git_oids,
            "{context}: commit order mismatch (use_commit_graph={use_graph})"
        );
    }
}
