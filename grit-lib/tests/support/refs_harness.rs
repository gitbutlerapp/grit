//! Backend-parameterized refs tests with grit ↔ git round-trip helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use grit_lib::objects::ObjectId;
use grit_lib::ref_namespace::storage_ref_name;
use grit_lib::refs::list_refs;
use grit_lib::repo::init_repository;

const AUTHOR_NAME: &str = "Refs Harness Author";
const AUTHOR_EMAIL: &str = "refs-harness@example.com";
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

/// Loose ref file path under the git directory for `refname`.
#[must_use]
pub fn loose_ref_path(git_dir: &Path, refname: &str) -> PathBuf {
    git_dir.join(storage_ref_name(refname))
}

/// Run `git fsck --strict` and panic on failure.
pub fn assert_git_fsck_strict(worktree: &Path) {
    assert!(
        git_fsck_strict(worktree),
        "git fsck --strict failed under {}",
        worktree.display()
    );
}

/// Run `git update-ref` with a hex oid.
pub fn git_update_ref(worktree: &Path, refname: &str, oid: &ObjectId) {
    git(worktree, &["update-ref", refname, &oid.to_hex()]);
}

/// Run `git check-ref-format` on a full ref name.
#[must_use]
pub fn git_check_ref_format(refname: &str) -> bool {
    Command::new("git")
        .args(["check-ref-format", refname])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
