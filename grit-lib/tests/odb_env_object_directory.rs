//! `GIT_OBJECT_DIRECTORY` places the repository's primary object store (issue #1907).
//!
//! Covers repository open, add/commit, fetch (local, `git daemon`, bundle) and push.
//!
//! Objects are read from and written to the environment's directory instead of
//! `<git-dir>/objects`, checked against the system `git` with the same variable set.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::Remote;
use grit_lib::repo::{Repository, RepositoryBuilder};

const IDENT: &str = "Odb Env <odb-env@example.com> 1700000000 +0000";

/// Variables that would point the oracle `git` at another repository or object store.
const SELECTION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
];

fn git_with(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Odb Env")
        .env("GIT_AUTHOR_EMAIL", "odb-env@example.com")
        .env("GIT_COMMITTER_NAME", "Odb Env")
        .env("GIT_COMMITTER_EMAIL", "odb-env@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args(args);
    for key in SELECTION_VARS {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn git")
}

fn git_ok(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> String {
    let out = git_with(dir, env, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Work tree `r/` (git-initialized) and an empty object directory `objq/` beside it.
fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonical tempdir");
    let work = root.join("r");
    let objq = root.join("objq");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&objq).unwrap();
    git_ok(&work, &[], &["init", "-q", "-b", "main"]);
    (tmp, work, objq)
}

fn environment(cwd: &Path, object_dir: &str) -> Environment {
    let mut env = Environment::empty();
    env.cwd = cwd.to_path_buf();
    env.git_config_global = Some("/dev/null".to_owned());
    env.git_config_nosystem = Some("1".to_owned());
    env.git_object_directory = Some(object_dir.to_owned());
    env
}

fn discover(env: Environment) -> grit_lib::error::Result<Repository> {
    let cwd = env.cwd.clone();
    Repository::discover(Some(&cwd), &RepositoryOptions::with_environment(env))
}

/// Loose object files below `objects_dir` (fan-out directories only).
fn loose_objects(objects_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.len() == 2))
        .flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten())
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

fn commit_request(message: &str) -> CommitRequest {
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(IDENT.parse().expect("signature"));
        o.committer = Some(IDENT.parse().expect("signature"));
        o
    }
}

#[test]
fn reads_blob_written_by_git_into_env_object_directory() {
    let (_tmp, work, objq) = layout();
    fs::write(work.join("fb"), "fresh blob\n").unwrap();
    let hex = git_ok(
        &work,
        &[("GIT_OBJECT_DIRECTORY", &objq)],
        &["hash-object", "-w", "fb"],
    );
    let oid: ObjectId = hex.parse().unwrap();
    assert_eq!(loose_objects(&objq).len(), 1);
    assert!(loose_objects(&work.join(".git/objects")).is_empty());

    let repo = discover(environment(&work, objq.to_str().unwrap())).expect("discover");
    assert_eq!(repo.odb().objects_dir(), objq.as_path());
    let object = repo
        .odb()
        .read(&oid)
        .expect("blob from env object directory");
    assert_eq!(object.kind, ObjectKind::Blob);
    assert_eq!(object.data, b"fresh blob\n");

    let mut plain = Environment::empty();
    plain.cwd = work.clone();
    let without_env = discover(plain).expect("discover without env");
    assert!(!without_env.odb().exists(&oid));
}

#[test]
fn add_and_commit_write_into_env_object_directory() {
    let (_tmp, work, objq) = layout();
    let dot_git_objects = work.join(".git/objects");
    fs::write(work.join("nb"), "new\n").unwrap();

    let repo = discover(environment(&work, objq.to_str().unwrap())).expect("discover");
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    assert_eq!(loose_objects(&objq).len(), 1, "blob lands in the env dir");
    assert!(loose_objects(&dot_git_objects).is_empty());

    fs::write(work.join("second"), "two\n").unwrap();
    let outcome = commit_worktree(&repo, &commit_request("x"), &mut NullProgress, &NeverCancel)
        .expect("commit");
    // Two blobs, one tree, one commit.
    assert_eq!(loose_objects(&objq).len(), 4);
    assert!(
        loose_objects(&dot_git_objects).is_empty(),
        "nothing leaks into .git/objects"
    );

    let env = [("GIT_OBJECT_DIRECTORY", objq.as_path())];
    git_ok(&work, &env, &["fsck", "--strict", "--no-dangling"]);
    let commit_hex = outcome.oid.to_hex();
    let tree = git_ok(&work, &env, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(git_ok(&work, &env, &["rev-parse", "HEAD"]), commit_hex);
    assert_eq!(git_ok(&work, &env, &["cat-file", "-p", "HEAD:nb"]), "new");
    assert!(!tree.is_empty());
    let without_env = git_with(&work, &[], &["cat-file", "-e", &commit_hex]);
    assert!(
        !without_env.status.success(),
        "commit must not exist in .git/objects"
    );
}

#[test]
fn relative_env_object_directory_resolves_against_environment_cwd() {
    let (_tmp, work, objq) = layout();
    fs::write(work.join("f"), "relative\n").unwrap();

    let repo = discover(environment(&work, "../objq")).expect("discover");
    assert_eq!(repo.odb().objects_dir(), work.join("../objq").as_path());
    let oid = repo.odb().write(ObjectKind::Blob, b"relative\n").unwrap();
    assert_eq!(loose_objects(&objq).len(), 1);
    assert!(loose_objects(&work.join(".git/objects")).is_empty());
    assert_eq!(
        git_ok(
            &work,
            &[("GIT_OBJECT_DIRECTORY", &objq)],
            &["cat-file", "-p", &oid.to_hex()]
        ),
        "relative"
    );
}

#[test]
fn env_object_directory_with_alternate_main_store_keeps_history_readable() {
    let (_tmp, work, objq) = layout();
    let dot_git_objects = work.join(".git/objects");
    fs::write(work.join("base"), "base\n").unwrap();
    git_ok(&work, &[], &["add", "base"]);
    git_ok(&work, &[], &["commit", "-q", "-m", "base"]);
    let base = git_ok(&work, &[], &["rev-parse", "HEAD"]);
    let main_store_before = loose_objects(&dot_git_objects);

    // Without the alternate the existing history is invisible, as in Git.
    let isolated = discover(environment(&work, objq.to_str().unwrap())).expect("discover");
    assert!(!isolated.odb().exists(&base.parse().unwrap()));

    let mut env = environment(&work, objq.to_str().unwrap());
    env.git_alternate_object_directories = Some(dot_git_objects.to_str().unwrap().to_owned());
    let repo = discover(env).expect("discover");
    fs::write(work.join("base"), "changed\n").unwrap();
    let outcome = commit_worktree(
        &repo,
        &commit_request("on top"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    assert_eq!(outcome.parents, vec![base.parse::<ObjectId>().unwrap()]);
    assert_eq!(loose_objects(&dot_git_objects), main_store_before);
    assert_eq!(loose_objects(&objq).len(), 3, "blob, tree and commit");

    let git_env = [
        ("GIT_OBJECT_DIRECTORY", objq.as_path()),
        (
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            dot_git_objects.as_path(),
        ),
    ];
    git_ok(&work, &git_env, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git_ok(&work, &git_env, &["rev-parse", "HEAD^"]),
        base,
        "git walks from the env store into the main store"
    );

    // An `info/alternates` file inside the env directory works the same way.
    fs::create_dir_all(objq.join("info")).unwrap();
    fs::write(
        objq.join("info/alternates"),
        format!("{}\n", dot_git_objects.display()),
    )
    .unwrap();
    let via_file = discover(environment(&work, objq.to_str().unwrap())).expect("discover");
    assert!(via_file.odb().exists(&base.parse().unwrap()));
    assert!(via_file.odb().exists(&outcome.oid));
}

#[test]
fn missing_env_object_directory_is_not_a_repository() {
    let (_tmp, work, objq) = layout();
    let missing = objq.join("missing");
    let err = discover(environment(&work, missing.to_str().unwrap()))
        .expect_err("missing GIT_OBJECT_DIRECTORY");
    assert!(matches!(err, Error::NotARepository(_)), "{err:?}");
    // Git rejects the repository the same way.
    let out = git_with(
        &work,
        &[("GIT_OBJECT_DIRECTORY", &missing)],
        &["rev-parse", "--git-dir"],
    );
    assert!(!out.status.success());
}

#[test]
fn empty_env_object_directory_is_not_a_repository() {
    let (_tmp, work, _objq) = layout();
    let err = discover(environment(&work, "")).expect_err("empty GIT_OBJECT_DIRECTORY");
    assert!(matches!(err, Error::NotARepository(_)), "{err:?}");
    let out = git_with(
        &work,
        &[("GIT_OBJECT_DIRECTORY", Path::new(""))],
        &["rev-parse", "--git-dir"],
    );
    assert!(!out.status.success(), "git rejects an empty value too");
}

#[test]
fn odb_builder_must_name_env_object_directory() {
    let (_tmp, work, objq) = layout();
    let git_dir = work.join(".git");
    let options = || {
        RepositoryOptions::with_environment(environment(&work, objq.to_str().unwrap()))
            .with_work_tree(&work)
    };

    let repo = RepositoryBuilder::new(options())
        .odb_builder(OdbBuilder::files(&objq))
        .open(&git_dir, Some(&work))
        .expect("builder rooted at the env dir");
    assert_eq!(repo.odb().objects_dir(), objq.as_path());
    let oid = repo.odb().write(ObjectKind::Blob, b"builder\n").unwrap();
    assert_eq!(loose_objects(&objq).len(), 1);
    assert!(Repository::discover(
        Some(&work),
        &RepositoryOptions::with_environment(environment(&work, objq.to_str().unwrap()))
    )
    .unwrap()
    .odb()
    .exists(&oid));

    let mismatch = RepositoryBuilder::new(options())
        .odb_builder(OdbBuilder::files(git_dir.join("objects")))
        .open(&git_dir, Some(&work));
    assert!(
        matches!(
            mismatch,
            Err(Error::OdbObjectsDirMismatch { ref repository, .. }) if repository == &objq
        ),
        "{mismatch:?}"
    );
}

/// `up/` (work tree), `remote.git` (bare, from `up`) and the client `r/` cloned from it by
/// system git, so `r/.git/objects` holds the base history. `up` then gains a commit that is
/// pushed to `remote.git`. Returns the layout root and the new tip.
fn transport_layout() -> (tempfile::TempDir, PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonical tempdir");
    let up = root.join("up");
    fs::create_dir_all(&up).unwrap();
    git_ok(&up, &[], &["init", "-q", "-b", "main"]);
    fs::write(up.join("base"), "base\n").unwrap();
    git_ok(&up, &[], &["add", "base"]);
    git_ok(&up, &[], &["commit", "-q", "-m", "base"]);
    git_ok(&root, &[], &["clone", "-q", "--bare", "up", "remote.git"]);
    git_ok(&root, &[], &["clone", "-q", "remote.git", "r"]);
    fs::create_dir_all(root.join("objq")).unwrap();

    fs::write(up.join("next"), "next\n").unwrap();
    git_ok(&up, &[], &["add", "next"]);
    git_ok(&up, &[], &["commit", "-q", "-m", "next"]);
    git_ok(&up, &[], &["push", "-q", "../remote.git", "main"]);
    let tip = git_ok(&up, &[], &["rev-parse", "HEAD"]);
    (tmp, root, tip)
}

/// Client environment: primary store `objq`, main store reachable as an alternate.
fn quarantined(root: &Path) -> Environment {
    let mut env = environment(&root.join("r"), root.join("objq").to_str().unwrap());
    env.git_alternate_object_directories =
        Some(root.join("r/.git/objects").to_str().unwrap().to_owned());
    env
}

fn quarantined_git_env(root: &Path) -> [(&'static str, PathBuf); 2] {
    [
        ("GIT_OBJECT_DIRECTORY", root.join("objq")),
        (
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            root.join("r/.git/objects"),
        ),
    ]
}

/// Every file under `dir` (loose objects, packs, indexes), sorted.
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn fetch_into_quarantine(root: &Path, url: &str) {
    let repo = discover(quarantined(root)).expect("discover");
    let remote = Remote::from_url(url).expect("remote url");
    repo.remotes()
        .fetch(
            &remote,
            {
                let mut o = FetchOptions::default();
                o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
                o
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("grit fetch");
}

/// After a fetch into the quarantine: the main store is untouched, the new objects are in
/// `objq`, and system git (with the same variables) sees a consistent repository.
fn assert_fetched_into_quarantine(root: &Path, main_before: &[PathBuf], tip: &str) {
    let work = root.join("r");
    assert_eq!(
        all_files(&work.join(".git/objects")),
        main_before,
        "fetch must not write into .git/objects"
    );
    assert!(!all_files(&root.join("objq")).is_empty());
    let env = quarantined_git_env(root);
    let env: Vec<(&str, &Path)> = env.iter().map(|(k, v)| (*k, v.as_path())).collect();
    assert_eq!(
        git_ok(&work, &env, &["rev-parse", "refs/remotes/origin/main"]),
        tip
    );
    git_ok(&work, &env, &["fsck", "--strict", "--no-dangling"]);
    let plain = git_with(&work, &[], &["cat-file", "-e", tip]);
    assert!(
        !plain.status.success(),
        "fetched commit must not be in .git/objects"
    );
}

#[test]
fn local_fetch_writes_into_env_object_directory() {
    let (_tmp, root, tip) = transport_layout();
    let main_before = all_files(&root.join("r/.git/objects"));

    // System git leaves the main store alone too.
    let (_oracle_tmp, oracle_root, oracle_tip) = transport_layout();
    let oracle_main = all_files(&oracle_root.join("r/.git/objects"));
    let oracle_env = quarantined_git_env(&oracle_root);
    let oracle_env: Vec<(&str, &Path)> =
        oracle_env.iter().map(|(k, v)| (*k, v.as_path())).collect();
    git_ok(
        &oracle_root.join("r"),
        &oracle_env,
        &["fetch", "-q", "origin"],
    );
    assert_fetched_into_quarantine(&oracle_root, &oracle_main, &oracle_tip);

    fetch_into_quarantine(&root, root.join("remote.git").to_str().unwrap());
    assert_fetched_into_quarantine(&root, &main_before, &tip);
}

fn daemon_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port")
}

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// System `git daemon` serving `root` (upload-pack and receive-pack) on a free port.
fn git_daemon(root: &Path) -> (Daemon, String) {
    let port = daemon_port();
    let child = Command::new("git")
        .args([
            "daemon",
            "--listen=127.0.0.1",
            &format!("--port={port}"),
            "--reuseaddr",
            "--export-all",
            "--enable=receive-pack",
            &format!("--base-path={}", root.display()),
        ])
        .arg(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn git daemon");
    let daemon = Daemon(child);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200))
        .is_err()
    {
        assert!(std::time::Instant::now() < deadline, "git daemon not ready");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    (daemon, format!("git://127.0.0.1:{port}/remote.git"))
}

#[test]
fn wire_fetch_from_git_daemon_writes_into_env_object_directory() {
    let (_tmp, root, tip) = transport_layout();
    let main_before = all_files(&root.join("r/.git/objects"));
    let (_daemon, url) = git_daemon(&root);
    fetch_into_quarantine(&root, &url);
    assert_fetched_into_quarantine(&root, &main_before, &tip);
}

#[test]
fn bundle_fetch_writes_into_env_object_directory_without_pack_dir() {
    let (_tmp, root, tip) = transport_layout();
    let main_before = all_files(&root.join("r/.git/objects"));
    git_ok(
        &root.join("up"),
        &[],
        &["bundle", "create", "../next.bundle", "main~1..main"],
    );
    assert!(!root.join("objq/pack").exists());
    fetch_into_quarantine(&root, root.join("next.bundle").to_str().unwrap());
    assert_fetched_into_quarantine(&root, &main_before, &tip);
}

/// Commit on the client with the quarantine set, then push `main` to `url`.
fn commit_and_push_from_quarantine(root: &Path, url: &str) -> ObjectId {
    let work = root.join("r");
    let repo = discover(quarantined(root)).expect("discover");
    fs::write(work.join("local"), "local\n").unwrap();
    let outcome = commit_worktree(
        &repo,
        &commit_request("local"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    let plain = git_with(&work, &[], &["cat-file", "-e", &outcome.oid.to_hex()]);
    assert!(!plain.status.success(), "the new commit lives only in objq");

    let remote = Remote::from_url(url).expect("remote url");
    let pushed = repo
        .remotes()
        .push(
            &remote,
            &[PushRefSpec {
                src: Some(outcome.oid),
                src_ref: Some("refs/heads/main".to_owned()),
                dst: "refs/heads/main".to_owned(),
                force: true,
                ..Default::default()
            }],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("grit push");
    assert!(
        pushed.results.iter().all(|r| r.status == PushRefStatus::Ok),
        "{:?}",
        pushed.results
    );
    outcome.oid
}

fn assert_remote_has(root: &Path, oid: ObjectId) {
    let bare = root.join("remote.git");
    assert_eq!(
        git_ok(&bare, &[], &["rev-parse", "refs/heads/main"]),
        oid.to_hex()
    );
    git_ok(&bare, &[], &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn local_push_reads_env_object_directory() {
    let (_tmp, root, _tip) = transport_layout();
    let oid = commit_and_push_from_quarantine(&root, root.join("remote.git").to_str().unwrap());
    assert_remote_has(&root, oid);
}

#[test]
fn wire_push_to_git_daemon_reads_env_object_directory() {
    let (_tmp, root, _tip) = transport_layout();
    let (_daemon, url) = git_daemon(&root);
    let oid = commit_and_push_from_quarantine(&root, &url);
    assert_remote_has(&root, oid);
}
