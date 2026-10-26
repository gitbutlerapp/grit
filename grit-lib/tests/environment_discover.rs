//! Explicit [`Environment`] discovery and config (task 503).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use tempfile::TempDir;

#[test]
fn cloned_environment_config_overrides_match_git() {
    use grit_lib::repo::InitOptions;
    use std::ffi::OsString;

    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    let original = Environment::from_vars(
        [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "user.name"),
            ("GIT_CONFIG_VALUE_0", "Original"),
            ("BUILD_SETTING", "captured"),
        ]
        .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        td.path().to_path_buf(),
    );
    let mut changed = original.clone();
    changed.push_config_override(&"user.name=Changed".parse().expect("override"));
    let unchanged = original.clone();
    drop(original);

    for (env, expected) in [(unchanged, "Original"), (changed, "Changed")] {
        let options = RepositoryOptions::with_environment(env);
        let repo = Repository::discover(None, &options.clone()).expect("discover");
        assert_eq!(
            repo.config().expect("config").get("user.name").as_deref(),
            Some(expected)
        );
        assert_eq!(
            repo.environment().var("BUILD_SETTING").as_deref(),
            Some("captured")
        );
        let git = Command::new("git")
            .current_dir(td.path())
            .env_clear()
            .envs(repo.environment().subprocess_environment())
            .args(["config", "--get", "user.name"])
            .output()
            .expect("git config");
        assert!(git.status.success());
        assert_eq!(
            String::from_utf8(git.stdout).expect("utf8").trim(),
            expected
        );
    }
}

#[test]
fn discover_with_global_config_from_environment_not_process() {
    let td = TempDir::new().expect("tempdir");
    let global = td.path().join("global.cfg");
    fs::write(&global, "[user]\n\tname = FromEnvGlobal\n").expect("write global");

    let repo_root = td.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).expect("git dir");
    fs::write(repo_root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::create_dir_all(repo_root.join(".git/objects")).expect("objects");
    fs::write(
        repo_root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .expect("config");

    let mut env = Environment::empty();
    env.cwd = repo_root.clone();
    env.git_config_global = Some(global.to_string_lossy().into_owned());
    env.git_config_nosystem = Some("true".into());

    let repo = Repository::discover(Some(&repo_root), &RepositoryOptions::with_environment(env))
        .expect("discover");
    let cfg = repo.config().expect("config");
    assert_eq!(cfg.get("user.name").as_deref(), Some("FromEnvGlobal"));

    assert!(
        std::env::var("GIT_CONFIG_GLOBAL").is_err(),
        "process env must stay untouched"
    );
}

#[test]
fn discover_with_git_dir_and_work_tree() {
    let td = TempDir::new().expect("tempdir");
    let outer = td.path().join("outer");
    let inner = outer.join("inner");
    fs::create_dir_all(&inner).expect("inner");
    let git_dir = outer.join(".git");
    fs::create_dir_all(&git_dir).expect("git");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .expect("config");

    let wt = outer.join("wt");
    fs::create_dir_all(&wt).expect("wt");

    let mut env = Environment::empty();
    env.cwd = inner.clone();
    env.git_dir = Some(git_dir.to_string_lossy().into_owned());
    env.git_work_tree = Some(wt.to_string_lossy().into_owned());

    let repo = Repository::discover(Some(&inner), &RepositoryOptions::with_environment(env))
        .expect("discover");
    assert_eq!(
        repo.work_tree()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf())),
        Some(wt.canonicalize().unwrap_or(wt))
    );
}

#[test]
fn ceiling_directories_stop_discovery() {
    let td = TempDir::new().expect("tempdir");
    let parent = td.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).expect("child");
    fs::create_dir_all(parent.join(".git/objects")).expect("objects");
    fs::write(parent.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::write(
        parent.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .expect("config");

    let mut env = Environment::empty();
    env.cwd = child.clone();
    env.git_ceiling_directories = Some(
        child
            .canonicalize()
            .unwrap_or(child.clone())
            .to_string_lossy()
            .into_owned(),
    );

    let err = Repository::discover(Some(&child), &RepositoryOptions::with_environment(env))
        .expect_err("ceiling blocks parent .git");
    assert!(matches!(err, grit_lib::error::Error::NotARepository(_)));
}

#[test]
fn git_round_trip_same_git_dir_with_matching_env() {
    let td = TempDir::new().expect("tempdir");
    let repo_root = td.path().join("repo");
    Command::new("git")
        .args(["init", repo_root.to_str().expect("utf8")])
        .output()
        .expect("git init");

    let mut env = Environment::empty();
    env.cwd = repo_root.clone();
    env.git_dir = Some(repo_root.join(".git").to_string_lossy().into_owned());

    let grit = Repository::discover(None, &RepositoryOptions::with_environment(env.clone()))
        .expect("grit discover");
    let grit_git = grit
        .git_dir()
        .canonicalize()
        .unwrap_or_else(|_| grit.git_dir().to_path_buf());

    let git_out = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .env("GIT_DIR", env.git_dir.as_ref().expect("dir"))
        .current_dir(&repo_root)
        .output()
        .expect("git rev-parse");
    assert!(git_out.status.success(), "git failed: {:?}", git_out);
    let git_dir = PathBuf::from(String::from_utf8_lossy(&git_out.stdout).trim());
    let git_dir = if git_dir.is_relative() {
        repo_root.join(git_dir)
    } else {
        git_dir
    };
    let git_dir = git_dir.canonicalize().unwrap_or(git_dir);
    assert_eq!(grit_git, git_dir);

    let git_wt = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_DIR", env.git_dir.as_ref().expect("dir"))
        .current_dir(&repo_root)
        .output()
        .expect("git rev-parse toplevel");
    assert!(git_wt.status.success(), "git failed: {:?}", git_wt);
    let git_wt = PathBuf::from(String::from_utf8_lossy(&git_wt.stdout).trim());
    let git_wt = git_wt.canonicalize().unwrap_or(git_wt);
    let grit_wt = grit
        .work_tree()
        .expect("work tree")
        .canonicalize()
        .unwrap_or_else(|_| {
            grit.work_tree()
                .map(|p| p.to_path_buf())
                .expect("work tree")
        });
    assert_eq!(grit_wt, git_wt);
}

#[test]
fn relative_global_config_resolved_from_environment_cwd() {
    let td = TempDir::new().expect("tempdir");
    let repo_root = td.path().join("repo");
    fs::create_dir_all(&repo_root).expect("repo");
    fs::write(
        repo_root.join("global.cfg"),
        "[user]\n\tname = RelativeGlobal\n",
    )
    .expect("global");
    fs::create_dir_all(repo_root.join(".git/objects")).expect("objects");
    fs::write(repo_root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::write(
        repo_root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .expect("config");

    let mut env = Environment::empty();
    env.cwd = repo_root.clone();
    env.git_config_global = Some("global.cfg".into());
    env.git_config_nosystem = Some("true".into());

    let repo = Repository::discover(
        Some(&repo_root),
        &RepositoryOptions::with_environment(env.clone()),
    )
    .expect("discover");
    let cfg = repo.config().expect("config");
    assert_eq!(cfg.get("user.name").as_deref(), Some("RelativeGlobal"));

    let git_out = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(&repo_root)
        .env("GIT_CONFIG_NOSYSTEM", "true")
        .env("GIT_CONFIG_GLOBAL", "global.cfg")
        .output()
        .expect("git config");
    assert!(git_out.status.success(), "git: {:?}", git_out);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout).trim(),
        "RelativeGlobal"
    );
}

#[test]
fn safe_directory_from_environment_global_config() {
    let td = TempDir::new().expect("tempdir");
    let global = td.path().join("global.cfg");
    fs::write(&global, "[safe]\n\tdirectory = *\n").expect("write global");

    let repo_root = td.path().join("repo");
    Command::new("git")
        .args(["init", "-q", repo_root.to_str().expect("utf8")])
        .output()
        .expect("git init");

    let mut env = Environment::empty();
    env.cwd = repo_root.clone();
    env.git_config_global = Some(global.to_string_lossy().into_owned());
    env.git_config_nosystem = Some("true".into());

    let git_status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_TEST_ASSUME_DIFFERENT_OWNER")
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_NOSYSTEM", "true")
        .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
        .output()
        .expect("git status");
    assert!(
        git_status.status.success(),
        "git status: {}",
        String::from_utf8_lossy(&git_status.stderr)
    );

    assert!(
        std::env::var("GIT_CONFIG_GLOBAL").is_err(),
        "process env must stay untouched"
    );
    assert!(std::env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER").is_err());

    let mut options = RepositoryOptions::with_environment(env);
    options.test_assume_different_owner = true;
    let repo = Repository::discover(Some(&repo_root), &options)
        .expect("discover must honor safe.directory from Environment global config");
    repo.enforce_safe_directory()
        .expect("safe.directory allows");
}
