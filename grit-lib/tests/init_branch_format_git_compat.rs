//! Init initial branch, separate git dir, and object format (task 3527 / t0001-init core cases).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::error::Error;
use grit_lib::objects::HashAlgo;
use grit_lib::progress::NullProgress;
use grit_lib::repo::{resolve_initial_branch, InitOptions, Repository};
use tempfile::TempDir;

fn git_fsck_ok(git_dir: &std::path::Path) {
    let output = Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "fsck"])
        .output()
        .expect("spawn git fsck");
    assert!(
        output.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_global_config(dir: &TempDir, contents: &str) {
    let path = dir.path().join("gitconfig");
    fs::write(&path, contents).expect("write gitconfig");
}

fn env_with_global(global_config: &std::path::Path) -> Environment {
    let mut env = Environment::empty();
    env.git_config_global = Some(global_config.to_string_lossy().into_owned());
    env
}

#[test]
fn init_uses_explicit_initial_branch() {
    let td = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        td.path(),
        &InitOptions::new().with_initial_branch("develop"),
    )
    .expect("init");
    let head = fs::read_to_string(repo.git_dir().join("HEAD")).expect("read HEAD");
    assert_eq!(head.trim(), "ref: refs/heads/develop");
}

#[test]
fn init_honors_init_default_branch_from_global_config() {
    let cfg_home = TempDir::new().expect("tempdir");
    write_global_config(&cfg_home, "[init]\n\tdefaultBranch = production\n");
    let env = env_with_global(&cfg_home.path().join("gitconfig"));
    assert_eq!(
        resolve_initial_branch(&env, None).expect("resolve"),
        "production"
    );
    let wt = TempDir::new().expect("tempdir");
    let repo =
        Repository::init(wt.path(), &InitOptions::new().with_environment(env)).expect("init");
    let head = fs::read_to_string(repo.git_dir().join("HEAD")).expect("read HEAD");
    assert_eq!(head.trim(), "ref: refs/heads/production");
}

#[test]
fn init_falls_back_to_main() {
    let env = Environment::empty();
    assert_eq!(resolve_initial_branch(&env, None).expect("resolve"), "main");
}

#[test]
fn init_invalid_branch_name_is_typed_error() {
    let err = resolve_initial_branch(&Environment::empty(), Some("-bad")).unwrap_err();
    assert!(matches!(err, Error::InvalidRef(_)));
}

#[test]
fn reinit_keeps_existing_head_branch() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::new().with_initial_branch("first")).expect("init");
    let (_, outcome) = Repository::init_with_outcome(
        td.path(),
        &InitOptions::new().with_initial_branch("ignored"),
    )
    .expect("reinit");
    assert!(outcome.reinitialized);
    assert_eq!(outcome.initial_branch, "first");
    let head = fs::read_to_string(td.path().join(".git/HEAD")).expect("read HEAD");
    assert_eq!(head.trim(), "ref: refs/heads/first");
}

#[test]
fn init_separate_git_dir_git_status_works() {
    let wt = TempDir::new().expect("tempdir");
    let separate = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        wt.path(),
        &InitOptions::new()
            .with_initial_branch("main")
            .with_separate_git_dir(Some(separate.path().to_path_buf())),
    )
    .expect("init");
    git_fsck_ok(repo.git_dir());
    let status = Command::new("git")
        .current_dir(wt.path())
        .args(["status"])
        .output()
        .expect("git status");
    assert!(
        status.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let gitfile = fs::read_to_string(wt.path().join(".git")).expect("gitfile");
    assert!(gitfile.starts_with("gitdir:"));
}

#[test]
fn init_sha256_then_commit_git_fsck_clean() {
    let td = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        td.path(),
        &InitOptions::new()
            .with_initial_branch("main")
            .with_object_format(HashAlgo::Sha256),
    )
    .expect("init");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
    let format = Command::new("git")
        .current_dir(td.path())
        .args(["rev-parse", "--show-object-format"])
        .output()
        .expect("rev-parse");
    assert!(
        format.status.success(),
        "{}",
        String::from_utf8_lossy(&format.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&format.stdout).trim(), "sha256");

    fs::write(td.path().join("file.txt"), "hello\n").expect("write");
    grit_lib::porcelain::add::stage(
        &repo,
        &grit_lib::porcelain::add::StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");
    let req = {
        let mut o = grit_lib::porcelain::commit::CommitRequest::default();
        o.message = "test".into();
        o.author = Some("T <t@e.com> 1 +0000".parse().expect("signature"));
        o.committer = Some("T <t@e.com> 1 +0000".parse().expect("signature"));
        o.allow_empty = false;
        o.sign_override = None;
        o.amend = false;
        o.assume_worktree_synced = false;
        o
    };
    grit_lib::porcelain::commit::create_commit(
        &repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");
    git_fsck_ok(repo.git_dir());
}

#[test]
fn init_reinit_preserves_sha256_when_object_format_omitted() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(
        td.path(),
        &InitOptions::new()
            .with_initial_branch("main")
            .with_object_format(HashAlgo::Sha256),
    )
    .expect("init sha256");
    let (_, outcome) = Repository::init_with_outcome(td.path(), &InitOptions::new())
        .expect("reinit without object format");
    assert!(outcome.reinitialized);
    let repo = Repository::init(td.path(), &InitOptions::new()).expect("open");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
}

#[test]
fn init_rejects_bare_with_separate_git_dir() {
    let wt = TempDir::new().expect("tempdir");
    let separate = TempDir::new().expect("tempdir");
    let err = Repository::init_with_outcome(
        wt.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_separate_git_dir(Some(separate.path().to_path_buf())),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InitBareWithSeparateGitDir));
}

#[test]
fn init_reinit_rediscovers_gitfile_without_separate_flag() {
    let wt = TempDir::new().expect("tempdir");
    let separate = TempDir::new().expect("tempdir");
    Repository::init(
        wt.path(),
        &InitOptions::new()
            .with_initial_branch("main")
            .with_separate_git_dir(Some(separate.path().to_path_buf())),
    )
    .expect("init separate");
    let (_, outcome) =
        Repository::init_with_outcome(wt.path(), &InitOptions::new()).expect("reinit via gitfile");
    assert!(outcome.reinitialized);
    assert_eq!(outcome.initial_branch, "main");
    let gitfile = fs::read_to_string(wt.path().join(".git")).expect("gitfile");
    assert!(gitfile.starts_with("gitdir:"));
}

#[test]
fn reinit_allows_detached_head() {
    let td = TempDir::new().expect("tempdir");
    let repo =
        Repository::init(td.path(), &InitOptions::new().with_initial_branch("main")).expect("init");
    fs::write(
        repo.git_dir().join("HEAD"),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
    )
    .expect("write detached HEAD");
    let (_, outcome) =
        Repository::init_with_outcome(td.path(), &InitOptions::new()).expect("reinit detached");
    assert!(outcome.reinitialized);
    assert_eq!(outcome.initial_branch, "deadbee");
}

#[test]
fn grit_opens_git_init_sha256_repo() {
    let td = TempDir::new().expect("tempdir");
    let status = Command::new("git")
        .current_dir(td.path())
        .args(["init", "-b", "main", "--object-format=sha256"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    assert!(status.success());
    let repo = Repository::open(
        &td.path().join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty()
            .with_work_tree(td.path().to_path_buf()),
    )
    .expect("open git-init sha256 repo");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
}
