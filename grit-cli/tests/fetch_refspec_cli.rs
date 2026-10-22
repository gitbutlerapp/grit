//! CLI tests for `grit fetch [<remote>] [<refspec>…]` and shared fetch flags.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?;
    assert!(status.success(), "git {args:?} failed");
    Ok(())
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn grit(dir: &Path, args: &[&str]) -> Output {
    Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("grit")
}

fn grit_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<String> = vec!["--json".into()];
    full.extend(args.iter().map(|s| (*s).to_string()));
    let out = grit(dir, &full.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    assert!(
        out.status.success(),
        "grit {:?} failed (cwd {}): stdout={} stderr={}",
        full,
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        stderr(&out)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn seed_remote(base: &Path) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let origin = base.join("origin.git");
    fs::create_dir_all(&origin)?;
    git(
        base,
        &["init", "--bare", "-b", "main", origin.to_str().unwrap()],
    )?;
    let seed = base.join("seed");
    git(
        base,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    )?;
    fs::write(seed.join("README.md"), "hello\n")?;
    git(&seed, &["add", "README.md"])?;
    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "init",
        ],
    )?;
    git(&seed, &["branch", "-M", "main"])?;
    git(&seed, &["push", "-u", "origin", "main"])?;
    Ok((origin, seed))
}

fn git_clone(base: &Path, origin: &Path, name: &str) -> PathBuf {
    let dir = base.join(name);
    git(
        base,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    )
    .expect("git clone client");
    dir
}

#[test]
fn fetch_origin_main_updates_tracking_and_fetch_head() {
    let base = tempfile::tempdir().expect("tempdir");
    let (origin, seed) = seed_remote(base.path()).expect("seed");
    let client = git_clone(base.path(), &origin, "client");

    fs::write(seed.join("README.md"), "hello world\n").expect("write");
    git(&seed, &["add", "README.md"]).expect("add");
    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "two",
        ],
    )
    .expect("commit");
    git(
        base.path(),
        &["-C", seed.to_str().unwrap(), "push", "origin", "main"],
    )
    .expect("push");

    let out = grit(&client, &["fetch", "origin", "main"]);
    assert!(
        out.status.success(),
        "grit fetch failed (bin={GS}): {}",
        stderr(&out)
    );

    let bare_tip = git_out(
        base.path(),
        &[
            "-C",
            origin.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ],
    );
    let grit_tip = git_out(&client, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(grit_tip, bare_tip);

    let git_client = base.path().join("git-client");
    git(
        base.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            git_client.to_str().unwrap(),
        ],
    )
    .expect("clone git-client");
    git(
        base.path(),
        &[
            "-C",
            git_client.to_str().unwrap(),
            "fetch",
            "-q",
            "origin",
            "main",
        ],
    )
    .expect("git fetch ref");
    let git_fh = fs::read_to_string(git_client.join(".git/FETCH_HEAD")).expect("git FETCH_HEAD");
    let grit_fh = fs::read_to_string(client.join(".git/FETCH_HEAD")).expect("grit FETCH_HEAD");
    let git_oid = git_fh.split('\t').next().expect("git FETCH_HEAD oid");
    let grit_oid = grit_fh.split('\t').next().expect("grit FETCH_HEAD oid");
    assert_eq!(git_oid, grit_oid, "FETCH_HEAD oid");
    assert!(
        grit_fh.contains("branch 'main'"),
        "grit FETCH_HEAD: {grit_fh}"
    );
    assert!(git_fh.contains("branch 'main'"), "git FETCH_HEAD: {git_fh}");
}

#[test]
fn fetch_pull_request_refspec_creates_local_branch() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, _seed) = seed_remote(base.path())?;
    let client = git_clone(base.path(), &origin, "client");

    let pr_oid = git_out(
        base.path(),
        &[
            "-C",
            origin.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ],
    );
    git(
        base.path(),
        &[
            "-C",
            origin.to_str().unwrap(),
            "update-ref",
            "refs/pull/123/head",
            &pr_oid,
        ],
    );

    let out = grit(&client, &["fetch", "origin", "pull/123/head:pr-123"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        git_out(&client, &["rev-parse", "refs/heads/pr-123"]),
        pr_oid
    );
    Ok(())
}

#[test]
fn fetch_dry_run_writes_nothing() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, seed) = seed_remote(base.path())?;
    let client = git_clone(base.path(), &origin, "client");

    fs::write(seed.join("two.txt"), "x\n")?;
    git(&seed, &["add", "two.txt"])?;
    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "two",
        ],
    )?;
    git(&seed, &["push", "origin", "main"])?;

    let before_tip = git_out(&client, &["rev-parse", "refs/remotes/origin/main"]);
    let objects_before = fs::read_dir(client.join(".git/objects"))?.count();
    let fetch_head_before = fs::read_to_string(client.join(".git/FETCH_HEAD")).unwrap_or_default();

    let json = grit_json(&client, &["fetch", "--dry-run", "origin", "main"]);
    assert_eq!(json["dry_run"], true);
    assert!(json["updates"].as_array().is_some_and(|a| !a.is_empty()));

    let after_tip = git_out(&client, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(before_tip, after_tip);
    assert_eq!(
        fs::read_dir(client.join(".git/objects"))?.count(),
        objects_before
    );
    assert_eq!(
        fs::read_to_string(client.join(".git/FETCH_HEAD")).unwrap_or_default(),
        fetch_head_before
    );
    Ok(())
}

#[test]
fn fetch_tags_flags_match_git() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, seed) = seed_remote(base.path())?;

    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "tag",
            "-a",
            "v1",
            "-m",
            "v1",
        ],
    )?;
    git(&seed, &["push", "origin", "v1"])?;

    let grit_client = git_clone(base.path(), &origin, "grit-c");
    let git_client = base.path().join("git-c");
    git(
        base.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            git_client.to_str().unwrap(),
        ],
    )?;

    git(&git_client, &["fetch", "-q", "--no-tags", "origin"])?;
    grit(&grit_client, &["fetch", "--no-tags", "origin"])
        .status
        .success();

    let grit_tags = git_out(&grit_client, &["tag"]);
    let git_tags = git_out(&git_client, &["tag"]);
    assert_eq!(grit_tags, git_tags);

    git(&git_client, &["fetch", "-q", "--tags", "origin"])?;
    grit(&grit_client, &["fetch", "--tags", "origin"])
        .status
        .success();

    let grit_tags = git_out(&grit_client, &["tag"]);
    let git_tags = git_out(&git_client, &["tag"]);
    assert_eq!(grit_tags, git_tags);
    Ok(())
}

#[test]
fn fetch_quiet_produces_empty_output() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, seed) = seed_remote(base.path())?;
    let client = git_clone(base.path(), &origin, "client");

    fs::write(seed.join("n.txt"), "n\n")?;
    git(&seed, &["add", "n.txt"])?;
    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "n",
        ],
    )?;
    git(&seed, &["push", "origin", "main"])?;

    let out = grit(&client, &["fetch", "-q", "origin", "main"]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
    Ok(())
}

#[test]
fn fetch_rejected_non_fast_forward_exit_one_json() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, _seed) = seed_remote(base.path())?;
    let client = git_clone(base.path(), &origin, "client");

    git(&client, &["checkout", "-q", "-b", "forceme"])?;
    git(
        &client,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "diverge",
        ],
    )?;
    git(&client, &["checkout", "-q", "main"])?;

    let remote_main = git_out(
        origin.parent().unwrap(),
        &[
            "-C",
            origin.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ],
    );

    let out = grit(
        &client,
        &[
            "--json",
            "fetch",
            "origin",
            &format!("refs/heads/main:refs/heads/forceme"),
        ],
    );
    assert!(!out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["rejected"], true);
    let rejected = json["updates"]
        .as_array()
        .and_then(|a| a.iter().find(|u| u["status"] == "rejected"))
        .expect("rejected entry");
    assert_eq!(rejected["ref"], "refs/heads/forceme");
    assert!(rejected["reason"].is_string());

    let forceme = git_out(&client, &["rev-parse", "refs/heads/forceme"]);
    assert_ne!(forceme, remote_main);
    Ok(())
}
