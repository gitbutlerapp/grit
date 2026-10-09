//! Backend-parameterized refs tests with grit ↔ git round-trip helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use grit_lib::objects::ObjectId;
use grit_lib::refs::list_refs;
use grit_lib::repo::init_repository;

pub const AUTHOR_NAME: &str = "Refs Harness Author";
pub const AUTHOR_EMAIL: &str = "refs-harness@example.com";
const DETERMINISTIC_DATE: &str = "1700000000 +0000";

/// Loose files backend or reftable ref storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Files,
    Reftable,
}

/// Temporary repository initialized with grit for one ref backend.
pub struct TestRepo {
    _root: tempfile::TempDir,
    worktree: PathBuf,
    backend: Backend,
}

impl TestRepo {
    /// Working tree path (contains `.git`).
    #[must_use]
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// Path to the git directory.
    #[must_use]
    pub fn git_dir(&self) -> PathBuf {
        self.worktree.join(".git")
    }

    /// Ref storage backend used at init.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }
}

fn ref_storage_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Files => "files",
        Backend::Reftable => "reftable",
    }
}

fn hermetic_git(worktree: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(worktree)
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", DETERMINISTIC_DATE)
        .env("GIT_COMMITTER_DATE", DETERMINISTIC_DATE)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn git")
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Whether the system `git` supports `git init --ref-format=reftable` (Git ≥ 2.45).
#[must_use]
pub fn git_supports_reftable() -> bool {
    static SUPPORTS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q", "--ref-format=reftable"])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Run `f` for [`Backend::Files`] and, when supported, [`Backend::Reftable`].
///
/// Reftable runs are skipped (with `SKIP: git lacks reftable` on stderr) when system git
/// cannot initialize a reftable repository.
pub fn each_backend(f: impl Fn(Backend, &TestRepo)) {
    for backend in [Backend::Files, Backend::Reftable] {
        if backend == Backend::Reftable && !git_supports_reftable() {
            eprintln!("SKIP: git lacks reftable");
            continue;
        }
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().to_path_buf();
        init_repository(&worktree, false, "main", None, ref_storage_name(backend))
            .expect("init_repository");
        let repo = TestRepo {
            _root: root,
            worktree,
            backend,
        };
        f(backend, &repo);
    }
}

/// Run `git` in `dir` with hermetic config and fixed identity; returns stdout on success.
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

/// Run `git` in `dir`; returns whether the command succeeded.
pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    hermetic_git(dir, args).status.success()
}

/// Run `git fsck --strict` in the repository rooted at `dir`.
pub fn git_fsck_strict(dir: &Path) -> bool {
    git_ok(dir, &["fsck", "--strict"])
}

/// List refs under `refs/` via grit [`list_refs`], as a name → oid map.
pub fn grit_refs(worktree: &Path) -> BTreeMap<String, ObjectId> {
    let git_dir = worktree.join(".git");
    list_refs(&git_dir, "refs/")
        .expect("list_refs")
        .into_iter()
        .collect()
}

/// Create an empty commit with system git and return its oid.
pub fn git_empty_commit_oid(worktree: &Path) -> ObjectId {
    git(
        worktree,
        &["commit", "--allow-empty", "-q", "-m", "refs harness seed"],
    );
    git(worktree, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD oid")
}

/// Parse `git show-ref` output into a name → oid map (deduplicated, sorted).
pub fn git_show_ref(worktree: &Path) -> BTreeMap<String, ObjectId> {
    let out = git(worktree, &["show-ref"]);
    let mut map = BTreeMap::new();
    for line in out.lines() {
        let Some((oid, name)) = line.split_once(' ') else {
            continue;
        };
        let oid: ObjectId = oid.parse().expect("show-ref oid");
        map.insert(name.to_owned(), oid);
    }
    map
}

/// Reflog author identity with a Unix timestamp (matches hermetic git env layout).
#[must_use]
pub fn reflog_identity(timestamp: i64) -> String {
    format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}> {timestamp} +0000")
}

/// Deep-copy a work tree (including `.git`) for side-by-side grit vs git experiments.
pub fn copy_worktree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir dest worktree");
    let status = Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ])
        .status()
        .expect("spawn cp");
    assert!(status.success(), "cp -a failed");
}

/// Append `fragment` to `.git/config` (creates `[core]` when missing).
pub fn append_repo_config(worktree: &Path, fragment: &str) {
    let config_path = worktree.join(".git/config");
    let mut body = std::fs::read_to_string(&config_path).unwrap_or_default();
    if !body.ends_with('\n') && !body.is_empty() {
        body.push('\n');
    }
    body.push_str(fragment);
    if !body.ends_with('\n') {
        body.push('\n');
    }
    std::fs::write(&config_path, body).expect("write config");
}

/// Replace `.git/config` entirely.
pub fn write_repo_config(worktree: &Path, body: &str) {
    std::fs::write(worktree.join(".git/config"), body).expect("write config");
}

/// Collect every file under `git_dir/logs/` with path relative to `logs/` and raw bytes.
pub fn reflog_tree_bytes(git_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let logs = git_dir.join("logs");
    let mut out = BTreeMap::new();
    collect_reflog_bytes_recursive(&logs, &logs, &mut out);
    out
}

fn collect_reflog_bytes_recursive(
    logs_root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_reflog_bytes_recursive(logs_root, &path, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(logs_root)
                .expect("under logs")
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path).expect("read reflog file");
            out.insert(rel, bytes);
        }
    }
}

/// Byte-compare all loose reflog files under `git_dir/logs/`.
pub fn assert_reflog_tree_matches(expected_git_dir: &Path, actual_git_dir: &Path) {
    let expected = reflog_tree_bytes(expected_git_dir);
    let actual = reflog_tree_bytes(actual_git_dir);
    assert_eq!(
        expected, actual,
        "reflog tree mismatch\nexpected: {expected:?}\nactual: {actual:?}"
    );
}

/// List ref names that have a reflog according to `git for-each-ref` + `git reflog`.
pub fn git_reflog_refs(worktree: &Path) -> Vec<String> {
    let out = git(worktree, &["for-each-ref", "--format=%(refname)"]);
    let mut refs: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    refs.sort();
    refs.dedup();
    refs.retain(|r| git_ok(worktree, &["reflog", "exists", r]));
    if git_ok(worktree, &["reflog", "exists", "HEAD"]) && !refs.iter().any(|r| r == "HEAD") {
        refs.push("HEAD".to_string());
    }
    refs.sort();
    refs
}
