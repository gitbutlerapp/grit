//! Backend-parameterized refs tests with grit ↔ git round-trip helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::ref_namespace::storage_ref_name;
use grit_lib::refs::list_refs;
use grit_lib::repo::{init_repository, Repository};

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

/// Duplicate an initialized repo tree into a fresh temp directory (same backend tag).
pub fn duplicate_repo(repo: &TestRepo) -> TestRepo {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path().join("wt");
    fs::create_dir_all(&worktree).expect("mkdir wt");
    copy_tree(repo.worktree(), &worktree).expect("copy tree");
    TestRepo {
        _root: root,
        worktree,
        backend: repo.backend(),
    }
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst).map(|_| ())
    }
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

/// Whether system `git` can read/write refs in a reftable repository (`git init --ref-format=reftable`).
#[must_use]
pub fn git_interop_available(backend: Backend) -> bool {
    backend == Backend::Files || git_supports_reftable()
}

/// Run `f` for [`Backend::Files`] and [`Backend::Reftable`].
///
/// Both backends are initialized with grit (`init_repository`). Tests that shell out to system
/// `git` on reftable repos should guard with [`git_interop_available`].
pub fn each_backend(f: impl Fn(Backend, &TestRepo)) {
    for backend in [Backend::Files, Backend::Reftable] {
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

fn grit_empty_commit_oid(worktree: &Path) -> ObjectId {
    grit_empty_commit_with_message(worktree, "refs harness seed")
}

fn grit_empty_commit_with_message(worktree: &Path, message: &str) -> ObjectId {
    let repo = Repository::discover(Some(worktree)).expect("discover repo");
    let ident = format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}> {DETERMINISTIC_DATE}");
    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message: message.to_owned(),
            author: ident.clone(),
            committer: ident,
            allow_empty: true,
            sign_override: None,
        },
        &mut NullProgress,
    )
    .expect("grit empty commit");
    outcome.oid
}

/// Seed commit for refs tests: system git when interop is available, otherwise grit.
pub fn empty_commit_oid(repo: &TestRepo) -> ObjectId {
    if git_interop_available(repo.backend()) {
        git_empty_commit_oid(repo.worktree())
    } else {
        grit_empty_commit_oid(repo.worktree())
    }
}

/// Two distinct empty commits for transaction tests (git or grit, per backend interop).
pub fn two_commit_oids(repo: &TestRepo) -> (ObjectId, ObjectId) {
    if git_interop_available(repo.backend()) {
        git(
            repo.worktree(),
            &["commit", "--allow-empty", "-q", "-m", "refs-tx initial"],
        );
        let c = git(repo.worktree(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("C");
        git(
            repo.worktree(),
            &["commit", "--allow-empty", "-q", "-m", "refs-tx second"],
        );
        let d = git(repo.worktree(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("D");
        (c, d)
    } else {
        let c = grit_empty_commit_with_message(repo.worktree(), "refs-tx initial");
        let d = grit_empty_commit_with_message(repo.worktree(), "refs-tx second");
        (c, d)
    }
}

/// Run `git fsck --strict` when system git can read this backend's ref storage.
pub fn assert_fsck_when_git_interop(repo: &TestRepo) {
    if git_interop_available(repo.backend()) {
        assert_git_fsck_strict(repo.worktree());
    }
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

/// Initialize a **files** backend repository (no reftable).
#[must_use]
pub fn files_repo() -> TestRepo {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path().to_path_buf();
    init_repository(&worktree, false, "main", None, "files").expect("init_repository");
    TestRepo {
        _root: root,
        worktree,
        backend: Backend::Files,
    }
}

/// `git for-each-ref --format='%(refname) %(objectname)'` under `prefix`, sorted.
pub fn git_for_each_ref(worktree: &Path, prefix: &str) -> Vec<(String, ObjectId)> {
    let out = git(
        worktree,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    );
    let mut rows: Vec<(String, ObjectId)> = out
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.split_once(' ')?;
            Some((name.to_owned(), oid.parse().ok()?))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Run `git for-each-ref` without asserting success (for parity checks on corrupt repos).
pub fn git_for_each_ref_output(worktree: &Path, prefix: &str) -> Output {
    hermetic_git(
        worktree,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    )
}

/// Assert grit [`list_refs`] and `git for-each-ref` both succeed and agree.
pub fn assert_list_refs_match_git(worktree: &Path, prefix: &str) {
    let git_dir = worktree.join(".git");
    let grit_rows: Vec<(String, ObjectId)> = list_refs(&git_dir, prefix).expect("list_refs");
    let git_rows = git_for_each_ref(worktree, prefix);
    assert_eq!(
        grit_rows, git_rows,
        "list_refs({prefix:?}) must match git for-each-ref"
    );
}

/// Parse `git show-ref` output into a name → oid map (deduplicated, sorted).
/// Run `git update-ref --stdin` with hermetic env; returns whether it succeeded.
pub fn git_update_ref_stdin(worktree: &Path, script: &str) -> bool {
    let mut child = Command::new("git")
        .current_dir(worktree)
        .args(["update-ref", "--stdin"])
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", DETERMINISTIC_DATE)
        .env("GIT_COMMITTER_DATE", DETERMINISTIC_DATE)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git update-ref --stdin");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write stdin");
    child.wait().expect("wait").success()
}

/// Byte snapshot of every file under `logs/` (reflogs), keyed by path relative to `git_dir`.
pub fn reflog_bytes_snapshot(git_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let logs = git_dir.join("logs");
    let mut out = BTreeMap::new();
    if !logs.is_dir() {
        return out;
    }
    let mut stack = vec![logs];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(git_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let bytes = fs::read(&path).unwrap_or_default();
                out.insert(rel, bytes);
            }
        }
    }
    out
}

/// Loose + packed ref file bytes under `refs/` and `packed-refs` for files-backend repos.
pub fn ref_store_bytes_snapshot(git_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let packed = git_dir.join("packed-refs");
    if packed.is_file() {
        out.insert(
            "packed-refs".to_owned(),
            fs::read(&packed).unwrap_or_default(),
        );
    }
    let refs_root = git_dir.join("refs");
    if refs_root.is_dir() {
        let mut stack = vec![refs_root];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(git_dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    out.insert(rel, fs::read(&path).unwrap_or_default());
                }
            }
        }
    }
    out
}

/// `tables.list` contents and table file count for reftable repos.
pub fn reftable_tables_snapshot(git_dir: &Path) -> (String, usize) {
    let reftable_dir = git_dir.join("reftable");
    let list_path = reftable_dir.join("tables.list");
    let list = fs::read_to_string(&list_path).unwrap_or_default();
    let count = list.lines().filter(|l| !l.is_empty()).count();
    (list, count)
}

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
