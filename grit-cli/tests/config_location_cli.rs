//! CLI tests for `grit config --local`, `-f`, and get origin JSON.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-config-cli-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit")
}

fn grit_ok(dir: &Path, args: &[&str]) {
    let out = grit(dir, args);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn config_get_missing_key_json_null_and_exit_code() -> TestResult {
    let scratch = Scratch::new("config-missing")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let out = grit(&repo, &["--json", "config", "no.such.key"]);
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["action"], "get");
    assert_eq!(v["key"], "no.such.key");
    assert!(v["value"].is_null());
    assert!(v["scope"].is_null());
    Ok(())
}

#[test]
fn config_file_flag_reads_and_writes_outside_repo() -> TestResult {
    let scratch = Scratch::new("config-file-flag")?;
    let cfg = scratch.path().join("alone.cfg");

    let set = grit(
        scratch.path(),
        &["config", "-f", cfg.to_str().unwrap(), "demo.value", "42"],
    );
    assert!(set.status.success());

    let got = grit(
        scratch.path(),
        &[
            "config",
            "-f",
            cfg.to_str().unwrap(),
            "--json",
            "demo.value",
        ],
    );
    assert!(got.status.success());
    let v: serde_json::Value = serde_json::from_slice(&got.stdout)?;
    assert_eq!(v["value"], "42");
    assert_eq!(v["scope"], "command");
    assert!(v["file"].as_str().unwrap().ends_with("alone.cfg"));
    assert!(v["line"].as_u64().unwrap() >= 1);
    Ok(())
}

#[test]
fn config_get_git_config_parameters_has_command_scope_no_file() -> TestResult {
    let scratch = Scratch::new("config-cmd-env")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_CONFIG_PARAMETERS", "'demo.value=42'")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--json", "demo.value"])
        .current_dir(&repo);
    let out = cmd.output()?;
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["value"], "42");
    assert_eq!(v["scope"], "command");
    assert!(v["file"].is_null());
    assert!(v["line"].is_null());
    Ok(())
}

#[test]
fn config_hot_path_honors_git_dir_from_outside_worktree() -> TestResult {
    let scratch = Scratch::new("config-git-dir")?;
    let outside = scratch.child("outside");
    fs::create_dir_all(&outside)?;
    let repo = scratch.child("target-repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let git_dir = repo.join(".git");
    let key = "review.gitdir";
    let value = "from-outside";

    let mut set = Command::new(GRIT);
    set.env("GIT_DIR", git_dir.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", key, value])
        .current_dir(&outside);
    let set_out = set.output()?;
    assert!(
        set_out.status.success(),
        "set stderr: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );

    let mut get = Command::new(GRIT);
    get.env("GIT_DIR", git_dir.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", key])
        .current_dir(&outside);
    let get_out = get.output()?;
    assert!(get_out.status.success());
    assert_eq!(String::from_utf8_lossy(&get_out.stdout).trim(), value);

    let git_check = Command::new("git")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "config",
            "--local",
            "--get",
            key,
        ])
        .output()?;
    assert!(git_check.status.success());
    assert_eq!(String::from_utf8_lossy(&git_check.stdout).trim(), value);

    // cwd is another repo: must still write GIT_DIR target, not cwd repo.
    let other = scratch.child("other-repo");
    fs::create_dir_all(&other)?;
    grit_ok(&other, &["init", "."]);
    let other_key = "review.other";
    grit_ok(&other, &["config", other_key, "must-not-leak"]);

    let mut set_gitdir = Command::new(GRIT);
    set_gitdir
        .env("GIT_DIR", git_dir.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "review.cwd-isolation", "ok"])
        .current_dir(&other);
    assert!(set_gitdir.output()?.status.success());

    let other_cfg = other.join(".git").join("config");
    let other_text = fs::read_to_string(&other_cfg)?;
    assert!(
        !other_text.contains("review.cwd-isolation"),
        "must not write cwd repo config when GIT_DIR selects another repository"
    );

    Ok(())
}

#[test]
fn config_unset_with_extra_value_rejected_on_hot_path() -> TestResult {
    let scratch = Scratch::new("config-unset-value")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let key = "review.validation";
    grit_ok(&repo, &["config", key, "keep"]);

    let out = grit(&repo, &["config", "--unset", key, "ignored"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--unset does not take a value"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    grit_ok(&repo, &["config", key]);
    let got = grit(&repo, &["config", key]);
    assert!(got.status.success());
    assert_eq!(String::from_utf8_lossy(&got.stdout).trim(), "keep");

    // Same check with GIT_DIR from outside the worktree (human hot path + env routing).
    let outside = scratch.child("outside");
    fs::create_dir_all(&outside)?;
    let git_dir = repo.join(".git");
    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_DIR", git_dir.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--unset", key, "ignored"])
        .current_dir(&outside);
    let out = cmd.output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--unset does not take a value"));
    grit_ok(&repo, &["config", key]);
    assert_eq!(
        String::from_utf8_lossy(&grit(&repo, &["config", key]).stdout).trim(),
        "keep"
    );

    Ok(())
}

#[test]
fn config_local_flag() -> TestResult {
    let scratch = Scratch::new("config-local-flag")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let global_cfg = scratch.path().join("global.cfg");
    fs::write(&global_cfg, "[user]\n\tname = Global Only\n")?;

    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_CONFIG_GLOBAL", global_cfg.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--local", "--json", "user.name"])
        .current_dir(&repo);
    let out = cmd.output()?;
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert!(v["value"].is_null());

    grit_ok(&repo, &["config", "user.name", "Local Name"]);
    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_CONFIG_GLOBAL", global_cfg.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--local", "--json", "user.name"])
        .current_dir(&repo);
    let out = cmd.output()?;
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["value"], "Local Name");
    assert_eq!(v["scope"], "local");
    Ok(())
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn config_set_from_subdirectory_rewrites_local_file_for_git() -> TestResult {
    let scratch = Scratch::new("config-set-subdir")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(repo.join("a/b"))?;
    git_ok(&repo, &["init", "-q", "."]);
    let config = repo.join(".git/config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
    }

    grit_ok(&repo.join("a/b"), &["config", "review.subdir", "first"]);
    grit_ok(&repo.join("a/b"), &["config", "review.subdir", "second"]);
    assert_eq!(git_ok(&repo, &["config", "review.subdir"]), "second");
    assert_eq!(
        git_ok(&repo, &["config", "--get-all", "review.subdir"]),
        "second"
    );
    assert!(!repo.join(".git/config.lock").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&config)?.permissions().mode() & 0o777, 0o600);
    }

    let out = grit(&repo.join("a"), &["config", "review.subdir"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "second");
    Ok(())
}

#[test]
fn config_set_from_linked_worktree_writes_common_config() -> TestResult {
    let scratch = Scratch::new("config-set-linked")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    git_ok(&repo, &["init", "-q", "."]);
    git_ok(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c",
        ],
    );
    let linked = scratch.child("linked");
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "side",
        ],
    );

    grit_ok(&linked, &["config", "review.linked", "yes"]);
    assert_eq!(
        git_ok(&repo, &["config", "--local", "review.linked"]),
        "yes"
    );
    assert_eq!(git_ok(&linked, &["config", "review.linked"]), "yes");
    Ok(())
}

#[test]
fn config_set_rejects_unsupported_repository_format() -> TestResult {
    let scratch = Scratch::new("config-set-format")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    git_ok(&repo, &["init", "-q", "."]);
    let config = repo.join(".git/config");
    fs::write(&config, "[core]\n\trepositoryformatversion = 99\n")?;

    let out = grit(&repo, &["config", "review.format", "x"]);
    assert!(!out.status.success());
    assert_eq!(
        fs::read_to_string(&config)?,
        "[core]\n\trepositoryformatversion = 99\n"
    );
    Ok(())
}

/// `grit config <key> <value>` touches only the repository's own config file: no global or
/// system config, no refs, and no object database.
#[cfg(target_os = "linux")]
#[test]
fn config_set_opens_only_the_local_config() -> TestResult {
    if Command::new("strace").arg("-V").output().is_err() {
        eprintln!("skipping: strace not installed");
        return Ok(());
    }
    let scratch = Scratch::new("config-set-strace")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    git_ok(&repo, &["init", "-q", "."]);
    let global = scratch.child("global.cfg");
    fs::write(&global, "[user]\n\tname = Global\n")?;
    let log = scratch.child("strace.txt");

    let status = Command::new("strace")
        .args(["-f", "-e", "trace=file", "-o", log.to_str().unwrap(), GRIT])
        .args(["config", "review.strace", "v"])
        .current_dir(&repo)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", &global)
        .status()?;
    assert!(status.success());

    let trace = fs::read_to_string(&log)?;
    for needle in [
        "global.cfg",
        "/dev/null",
        "packed-refs",
        "refs/replace",
        "refs/heads",
        "objects/pack",
        "objects/info",
    ] {
        assert!(
            !trace.lines().any(|l| l.contains(needle)),
            "config set touched {needle}:\n{trace}"
        );
    }
    let config_reads = trace
        .lines()
        .filter(|l| l.contains("openat(") && l.contains(".git/config\""))
        .count();
    assert!(
        config_reads <= 2,
        "expected at most 2 opens of .git/config, got {config_reads}:\n{trace}"
    );
    assert_eq!(git_ok(&repo, &["config", "review.strace"]), "v");
    Ok(())
}
