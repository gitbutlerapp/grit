//! `GIT_OBJECT_DIRECTORY` places the repository's primary object store (issue #1907).
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
use grit_lib::progress::NullProgress;
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
    CommitRequest {
        message: message.to_owned(),
        author: Some(IDENT.parse().expect("signature")),
        committer: Some(IDENT.parse().expect("signature")),
        ..Default::default()
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
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    assert_eq!(loose_objects(&objq).len(), 1, "blob lands in the env dir");
    assert!(loose_objects(&dot_git_objects).is_empty());

    fs::write(work.join("second"), "two\n").unwrap();
    let outcome = commit_worktree(&repo, &commit_request("x"), &mut NullProgress).expect("commit");
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
    let outcome =
        commit_worktree(&repo, &commit_request("on top"), &mut NullProgress).expect("commit");
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
fn empty_env_object_directory_falls_back_to_git_dir_objects() {
    let (_tmp, work, _objq) = layout();
    let repo = discover(environment(&work, "")).expect("discover");
    assert_eq!(
        repo.odb().objects_dir(),
        work.join(".git/objects").as_path()
    );
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
