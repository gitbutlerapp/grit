//! Top-level `-C`, `-c`, `--git-dir` / `--work-tree`, and `-q` on clone and push.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn hermetic(mut cmd: Command, home: &Path) -> Command {
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "EMAIL",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000");
    cmd
}

fn grit(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    let mut cmd = hermetic(Command::new(GRIT), home);
    cmd.current_dir(cwd).args(args).output().expect("run grit")
}

fn git(cwd: &Path, home: &Path, args: &[&str]) -> String {
    let mut cmd = hermetic(Command::new("git"), home);
    let out = cmd.current_dir(cwd).args(args).output().expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim_end()
        .to_owned()
}

fn ok(out: &Output) -> String {
    assert!(out.status.success(), "grit failed: {out:?}");
    String::from_utf8(out.stdout.clone()).expect("utf8")
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&ok(out)).expect("json output")
}

/// `base/sub/r`: a repository with no identity configured and one untracked file.
fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("sub").join("r");
    fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, base.path(), &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").expect("write");
    (base, repo)
}

#[test]
fn dash_c_dir_runs_in_another_repository() {
    let (base, _repo) = setup();
    let status = json(&grit(
        base.path(),
        base.path(),
        &["-C", "sub/r", "status", "--json"],
    ));
    assert_eq!(status["branch"], "main");
    assert_eq!(status["untracked"][0], "a.txt");

    let chained = json(&grit(
        base.path(),
        base.path(),
        &["-C", "sub", "-C", "", "-C", "r", "--json", "status"],
    ));
    assert_eq!(chained["branch"], "main");
}

#[test]
fn dash_c_config_supplies_identity_seen_by_git() {
    let (base, repo) = setup();
    let home = base.path();
    ok(&grit(
        base.path(),
        home,
        &[
            "-C",
            "sub/r",
            "-c",
            "user.name=CI Bot",
            "-c",
            "user.email=ci@x.y",
            "commit",
            "-m",
            "x",
        ],
    ));
    assert_eq!(
        git(&repo, home, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
        "CI Bot <ci@x.y>|CI Bot <ci@x.y>"
    );
    git(&repo, home, &["fsck", "--strict"]);
}

#[test]
fn dash_c_config_beats_repository_config() {
    let (base, repo) = setup();
    let home = base.path();
    git(&repo, home, &["config", "user.name", "From Repo"]);
    git(&repo, home, &["config", "user.email", "repo@x.y"]);
    ok(&grit(
        &repo,
        home,
        &[
            "-c",
            "user.name=First",
            "-c",
            "user.name=From CLI",
            "commit",
            "-m",
            "x",
        ],
    ));
    assert_eq!(
        git(&repo, home, &["log", "-1", "--format=%an <%ae>"]),
        "From CLI <repo@x.y>"
    );
    assert_eq!(git(&repo, home, &["config", "user.name"]), "From Repo");
}

#[cfg(unix)]
#[test]
fn dash_c_config_reaches_hooks_that_run_git() {
    use std::os::unix::fs::PermissionsExt;

    let (base, repo) = setup();
    let home = base.path();
    git(&repo, home, &["config", "user.name", "From Repo"]);
    git(&repo, home, &["config", "user.email", "repo@x.y"]);
    let hook = repo.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\ngit config user.name > \"$(git rev-parse --git-dir)/hook-saw\"\n",
    )
    .expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");

    ok(&grit(
        base.path(),
        home,
        &[
            "-C",
            "sub/r",
            "-c",
            "user.name=Hook Sees Me",
            "commit",
            "-m",
            "x",
        ],
    ));
    assert_eq!(
        fs::read_to_string(repo.join(".git/hook-saw")).expect("hook ran"),
        "Hook Sees Me\n"
    );
}

#[test]
fn git_dir_and_work_tree_select_the_repository() {
    let (base, repo) = setup();
    let elsewhere = base.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("mkdir");
    let status = json(&grit(
        &elsewhere,
        base.path(),
        &[
            "--git-dir",
            "../sub/r/.git",
            "--work-tree=../sub/r",
            "--json",
            "status",
        ],
    ));
    assert_eq!(status["branch"], "main");
    assert_eq!(status["untracked"][0], "a.txt");

    let from_parent = json(&grit(
        base.path(),
        base.path(),
        &[
            "-C",
            "sub",
            "--git-dir",
            "r/.git",
            "--work-tree",
            "r",
            "status",
            "--json",
        ],
    ));
    assert_eq!(from_parent["untracked"][0], "a.txt");
    assert!(repo.join(".git").is_dir());
}

#[test]
fn bad_global_options_are_reported() {
    let (base, _repo) = setup();
    let bad_key = grit(
        base.path(),
        base.path(),
        &["--json", "-c", "nosection=x", "status"],
    );
    assert_eq!(bad_key.status.code(), Some(2), "{bad_key:?}");
    let err: serde_json::Value = serde_json::from_slice(&bad_key.stdout).expect("json error");
    assert_eq!(err["kind"], "usage");
    assert!(err["error"].as_str().unwrap_or("").contains("nosection"));

    let missing_dir = grit(base.path(), base.path(), &["-C", "nope", "status"]);
    assert_eq!(missing_dir.status.code(), Some(1), "{missing_dir:?}");
    assert!(String::from_utf8_lossy(&missing_dir.stderr).contains("nope"));

    let after_subcommand = grit(base.path(), base.path(), &["status", "-C", "sub/r"]);
    assert_eq!(after_subcommand.status.code(), Some(2));
}

#[test]
fn quiet_clone_and_push_print_nothing() {
    let base = tempfile::tempdir().expect("tempdir");
    let home = base.path();
    git(
        home,
        home,
        &["init", "-q", "--bare", "-b", "main", "srv.git"],
    );
    git(home, home, &["clone", "-q", "srv.git", "seed"]);
    let seed = home.join("seed");
    fs::write(seed.join("f"), "1\n").expect("write");
    git(&seed, home, &["add", "f"]);
    git(
        &seed,
        home,
        &[
            "-c",
            "user.name=S",
            "-c",
            "user.email=s@x",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(&seed, home, &["push", "-q", "origin", "HEAD:main"]);

    let clone = grit(home, home, &["clone", "-q", "srv.git", "work"]);
    assert!(clone.status.success(), "{clone:?}");
    assert!(clone.stdout.is_empty(), "{clone:?}");
    assert!(clone.stderr.is_empty(), "{clone:?}");
    let work = home.join("work");
    assert!(work.join("f").is_file());

    fs::write(work.join("f"), "2\n").expect("write");
    ok(&grit(
        &work,
        home,
        &[
            "-c",
            "user.name=W",
            "-c",
            "user.email=w@x",
            "commit",
            "-m",
            "two",
        ],
    ));
    let push = grit(&work, home, &["push", "-q"]);
    assert!(push.status.success(), "{push:?}");
    assert!(push.stdout.is_empty(), "{push:?}");
    assert!(push.stderr.is_empty(), "{push:?}");
    assert_eq!(
        git(&home.join("srv.git"), home, &["log", "-1", "--format=%s"]),
        "two"
    );
    git(&home.join("srv.git"), home, &["fsck", "--strict"]);

    let json_clone = json(&grit(
        home,
        home,
        &["--json", "clone", "-q", "srv.git", "w2"],
    ));
    assert_eq!(json_clone["branch"], "main");
}

#[test]
fn quiet_push_still_reports_rejections() {
    let base = tempfile::tempdir().expect("tempdir");
    let home = base.path();
    git(
        home,
        home,
        &["init", "-q", "--bare", "-b", "main", "srv.git"],
    );
    for name in ["one", "two"] {
        git(home, home, &["clone", "-q", "srv.git", name]);
        let dir = home.join(name);
        fs::write(dir.join("f"), name).expect("write");
        ok(&grit(
            &dir,
            home,
            &[
                "-c",
                "user.name=W",
                "-c",
                "user.email=w@x",
                "commit",
                "-m",
                name,
            ],
        ));
    }
    ok(&grit(
        &home.join("one"),
        home,
        &["push", "-q", "origin", "main"],
    ));
    let rejected = grit(&home.join("two"), home, &["push", "-q", "origin", "main"]);
    assert_eq!(rejected.status.code(), Some(1), "{rejected:?}");
    assert!(
        String::from_utf8_lossy(&rejected.stdout).contains("main"),
        "{rejected:?}"
    );
}
