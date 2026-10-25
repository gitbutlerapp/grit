//! `grit checkout <url>` against a running `grit serve`: a snapshot of some
//! paths becomes a one-commit repository on a new work branch, and a server
//! without the checkout endpoint is refused with a clear error.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit(home: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(GRIT);
    cmd.current_dir(dir)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "A U Thor")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .env_remove("GIT_ASKPASS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null());
    cmd
}

fn run(home: &Path, dir: &Path, args: &[&str]) -> Output {
    grit(home, dir, args).output().expect("spawn grit")
}

fn ok(home: &Path, dir: &Path, args: &[&str]) -> Output {
    let out = run(home, dir, args);
    assert!(
        out.status.success(),
        "grit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(home: &Path, root: &Path) -> (ServerGuard, String) {
    let mut child = grit(
        home,
        root,
        &[
            "serve",
            "--root",
            ".",
            "--bind",
            "127.0.0.1:0",
            "--no-bundles",
        ],
    )
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn grit serve");
    let stderr = child.stderr.take().expect("server stderr");
    let guard = ServerGuard(child);
    let mut stderr = BufReader::new(stderr);
    let mut first = String::new();
    stderr.read_line(&mut first).expect("read server banner");
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let url = first
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .unwrap_or_else(|| panic!("no URL in server banner: {first}"))
        .trim_end_matches('/')
        .to_owned();
    (guard, url)
}

/// A server that answers every request with 404, like a Git host without
/// snapshot checkouts.
fn plain_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 10\r\nConnection: close\r\n\r\nnot found\n",
            );
        }
    });
    format!("http://{addr}")
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let repos = tmp.path().join("repos");
    let demo = repos.join("demo");
    for dir in [&home, &demo.join("src/deep"), &demo.join("docs")] {
        std::fs::create_dir_all(dir).expect("mkdir");
    }
    ok(&home, &demo, &["init"]);
    for (path, text) in [
        ("README", "top\n"),
        ("src/a.rs", "a\n"),
        ("src/deep/b.rs", "b\n"),
        ("docs/d.md", "d\n"),
    ] {
        std::fs::write(demo.join(path), text).expect("write");
    }
    ok(&home, &demo, &["add", "."]);
    ok(&home, &demo, &["commit", "-m", "Lay out the tree"]);
    let mut child = grit(
        &home,
        &repos,
        &[
            "serve",
            "--root",
            ".",
            "--setup",
            "--email",
            "admin@example.com",
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("setup");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"admin password\n")
        .expect("password");
    assert!(child.wait_with_output().expect("setup").status.success());
    let made: Value = serde_json::from_slice(
        &ok(
            &home,
            &repos,
            &[
                "--json",
                "serve",
                "--root",
                ".",
                "token",
                "admin@example.com",
            ],
        )
        .stdout,
    )
    .expect("token JSON");
    let helper = format!("store --file={}", home.join("credentials").display());
    ok(
        &home,
        &home,
        &["config", "--global", "credential.helper", &helper],
    );
    ok(
        &home,
        &home,
        &["config", "--global", "user.email", "alice@example.com"],
    );
    let token = made["token"].as_str().expect("token").to_owned();
    std::fs::write(home.join("token"), token).expect("token file");
    (tmp, home, repos)
}

#[test]
fn checkouts_are_one_commit_of_the_paths_asked_for() {
    let (_tmp, home, repos) = setup();
    let (_server, url) = serve(&home, &repos);
    let token = std::fs::read_to_string(home.join("token")).expect("token");
    ok(&home, &home, &["login", &url, "--token", &token]);
    let demo = repos.join("demo");
    let head = git(&demo, &["rev-parse", "HEAD"]);

    let out: Value = serde_json::from_slice(
        &ok(
            &home,
            &home,
            &[
                "--json",
                "checkout",
                &format!("{url}/demo.git"),
                "--path",
                "src/deep",
                "--into",
                "co",
            ],
        )
        .stdout,
    )
    .expect("checkout JSON");
    assert_eq!(out["branch"], "main");
    assert_eq!(out["base"], Value::String(head.clone()));
    assert_eq!(out["paths"], serde_json::json!(["src/deep"]));
    assert_eq!(out["files"], 3, "{out}");
    let work = out["work"].as_str().expect("work").to_owned();
    assert!(work.starts_with("alice/"), "{work}");

    let co = home.join("co");
    // Cone mode: the path, plus the files in the directories above it.
    for present in ["README", "src/a.rs", "src/deep/b.rs"] {
        assert!(co.join(present).is_file(), "{present} is checked out");
    }
    assert!(!co.join("docs").exists());
    let snapshot = out["snapshot"].as_str().expect("snapshot");
    assert_eq!(git(&co, &["rev-parse", "HEAD"]), snapshot);
    assert_eq!(git(&co, &["rev-parse", "origin/main"]), snapshot);
    assert_eq!(git(&co, &["branch", "--show-current"]), work);
    assert_eq!(git(&co, &["rev-list", "--count", "HEAD"]), "1");
    assert!(
        git(&co, &["log", "-1", "--format=%B"]).contains(&format!("Grit-Checkout-Base: {head}"))
    );
    assert_eq!(git(&co, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&co, &["config", "remote.origin.url"]),
        format!("{url}/demo.git")
    );
    git(&co, &["fsck", "--strict"]);

    // The same paths at the same commit share the snapshot.
    let again: Value = serde_json::from_slice(
        &ok(
            &home,
            &home,
            &[
                "--json",
                "checkout",
                &format!("{url}/demo"),
                "-p",
                "src/deep/",
                "--into",
                "co2",
            ],
        )
        .stdout,
    )
    .expect("checkout JSON");
    assert_eq!(again["snapshot"], out["snapshot"]);
    assert_ne!(again["id"], out["id"]);

    // A path the repository doesn't have is refused, and nothing is made.
    let missing = run(
        &home,
        &home,
        &[
            "checkout",
            &format!("{url}/demo.git"),
            "-p",
            "nope",
            "--into",
            "co3",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("nope is not in the repository"));
    assert!(!home.join("co3").exists());
}

#[test]
fn servers_without_the_checkout_endpoint_are_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().to_path_buf();
    let url = plain_server();
    let out = run(&home, &home, &["checkout", &format!("{url}/demo.git")]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("offers no grit checkout")
            && stderr.contains(&format!("{url}/.grit/checkouts/demo")),
        "{stderr}"
    );
    assert!(!home.join("demo").exists());
}

fn json(home: &Path, dir: &Path, args: &[&str]) -> Value {
    serde_json::from_slice(&ok(home, dir, args).stdout).expect("JSON output")
}

fn commit_file(home: &Path, dir: &Path, path: &str, text: &str, message: &str) {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(dir.join(parent)).expect("mkdir");
    }
    std::fs::write(dir.join(path), text).expect("write");
    ok(home, dir, &["add", path]);
    ok(home, dir, &["commit", "-m", message]);
}

#[test]
fn work_is_checked_in_integrated_and_refreshed() {
    let (_tmp, home, repos) = setup();
    let demo = repos.join("demo");
    ok(
        &home,
        &demo,
        &["config", "receive.denyCurrentBranch", "ignore"],
    );
    let (_server, url) = serve(&home, &repos);
    let token = std::fs::read_to_string(home.join("token")).expect("token");
    ok(&home, &home, &["login", &url, "--token", &token]);
    let repo_url = format!("{url}/demo.git");
    let first = json(
        &home,
        &home,
        &["--json", "checkout", &repo_url, "-p", "src", "--into", "co"],
    );
    let work = first["work"].as_str().expect("work").to_owned();
    let co = home.join("co");

    // Nothing to check in yet.
    let nothing = run(&home, &co, &["checkin"]);
    assert!(!nothing.status.success());
    assert!(String::from_utf8_lossy(&nothing.stderr).contains("nothing to check in"));

    // A commit on the snapshot becomes a commit on the real history.
    commit_file(&home, &co, "src/a.rs", "a2\n", "Change a");
    let checkin = json(&home, &co, &["--json", "checkin"]);
    assert_eq!(checkin["refused"], Value::Bool(false), "{checkin}");
    assert_eq!(checkin["branches"][0]["name"], Value::String(work.clone()));
    assert_eq!(checkin["branches"][0]["status"], "created");
    let grafted = checkin["branches"][0]["grafted"]
        .as_str()
        .expect("grafted")
        .to_owned();
    assert_eq!(git(&demo, &["rev-parse", &work]), grafted);
    assert_eq!(
        git(&demo, &["rev-parse", &format!("{work}^")]),
        git(&demo, &["rev-parse", "main"])
    );
    assert_eq!(
        git(&demo, &["show", &format!("{work}:docs/d.md")]),
        "d",
        "paths outside the checkout are kept"
    );
    assert_eq!(git(&demo, &["show", &format!("{work}:src/a.rs")]), "a2");
    assert_eq!(
        git(&demo, &["log", "-1", "--format=%an %s", &work]),
        "A U Thor Change a"
    );
    // Checking in again changes nothing.
    let again = json(&home, &co, &["--json", "checkin", &work]);
    assert_eq!(again["branches"][0]["status"], "unchanged");

    // The target branch can't be checked in directly.
    ok(&home, &co, &["branch", "main"]);
    commit_file(&home, &co, "src/x.rs", "x\n", "On a side branch");
    ok(&home, &co, &["branch", "-f", "main"]);
    let target = run(&home, &co, &["--json", "checkin", "main"]);
    assert_eq!(target.status.code(), Some(1));
    let target: Value = serde_json::from_slice(&target.stdout).expect("JSON");
    assert_eq!(target["branches"][0]["status"], "refused");
    assert!(target["branches"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("target branch"));
    ok(&home, &co, &["reset", "--hard", "HEAD~1"]);
    ok(&home, &co, &["branch", "-D", "main"]);

    // Someone else moves the target; integrate merges the work, and the refresh
    // brings their change in and drops the integrated commit.
    let other = home.join("other");
    ok(&home, &home, &["clone", &repo_url, "other"]);
    commit_file(&home, &other, "src/deep/b.rs", "b2\n", "Bob changes b");
    ok(&home, &other, &["push"]);
    let integrated = json(&home, &co, &["--json", "integrate", &work]);
    assert_eq!(integrated["status"], "merged", "{integrated}");
    assert_eq!(integrated["commits"], 1);
    assert_eq!(
        integrated["refresh"]["refresh"]["moved"],
        Value::Bool(true),
        "{integrated}"
    );
    let main = git(&demo, &["rev-parse", "main"]);
    assert_eq!(integrated["head"], Value::String(main.clone()));
    assert_eq!(
        git(&demo, &["log", "-1", "--format=%s", "main"]),
        format!("Merge branch '{work}' into main")
    );
    assert_eq!(
        std::fs::read_to_string(co.join("src/deep/b.rs"))
            .ok()
            .as_deref(),
        Some("b2\n")
    );
    assert_eq!(
        git(&co, &["rev-parse", "HEAD"]),
        git(&co, &["rev-parse", "origin/main"]),
        "the integrated commit dropped out"
    );
    assert!(
        git(&co, &["log", "-1", "--format=%B"]).contains(&format!("Grit-Checkout-Base: {main}"))
    );

    // Work on top of the newer snapshot checks in onto the newer main.
    commit_file(&home, &co, "src/c.rs", "c\n", "Add c");
    let later = json(&home, &co, &["--json", "checkin"]);
    assert_eq!(later["branches"][0]["status"], "updated", "{later}");
    assert_eq!(git(&demo, &["rev-parse", &format!("{work}^")]), main);

    // A plain pull when the target hasn't moved.
    let pulled = json(&home, &co, &["--json", "pull"]);
    assert_eq!(pulled["refresh"]["moved"], Value::Bool(false), "{pulled}");
}

#[test]
fn slices_name_paths_on_the_target_branch() {
    let (_tmp, home, repos) = setup();
    let demo = repos.join("demo");
    let (_server, url) = serve(&home, &repos);
    let token = std::fs::read_to_string(home.join("token")).expect("token");
    ok(&home, &home, &["login", &url, "--token", &token]);
    let repo_url = format!("{url}/demo.git");

    let empty = json(
        &home,
        &home,
        &["--json", "slice", "list", "--url", &repo_url],
    );
    assert_eq!(empty["slices"], serde_json::json!([]));
    let outside = run(&home, &home, &["slice", "list"]);
    assert!(String::from_utf8_lossy(&outside.stderr).contains("--url"));

    let made = json(
        &home,
        &home,
        &[
            "--json",
            "slice",
            "new",
            "deep",
            "-p",
            "src/deep",
            "-d",
            "Deep code",
            "--url",
            &repo_url,
        ],
    );
    assert_eq!(made["action"], "new");
    assert_eq!(
        git(&demo, &["log", "-1", "--format=%an <%ae> %s", "main"]),
        "alice@example.com <alice@example.com> Add slice deep"
    );
    assert!(git(&demo, &["show", "main:.grit/slices.toml"]).contains("[slices.deep]"));
    let duplicate = run(
        &home,
        &home,
        &["slice", "new", "deep", "-p", "docs", "--url", &repo_url],
    );
    assert!(!duplicate.status.success());
    let missing = run(
        &home,
        &home,
        &["slice", "new", "nope", "-p", "nope", "--url", &repo_url],
    );
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not in main"));

    // Check out by slice name; inside the checkout, --url is not needed.
    let co = json(&home, &home, &["--json", "checkout", &repo_url, "deep"]);
    assert_eq!(co["paths"], serde_json::json!(["src/deep"]));
    assert!(home.join("deep/src/deep/b.rs").is_file());
    assert!(!home.join("deep/docs").exists());
    let edited = json(
        &home,
        &home.join("deep"),
        &["--json", "slice", "edit", "deep", "--add", "docs"],
    );
    assert_eq!(
        edited["slice"]["paths"],
        serde_json::json!(["docs", "src/deep"])
    );
    let listed = json(&home, &home.join("deep"), &["--json", "slice", "list"]);
    assert_eq!(listed["slices"][0]["description"], "Deep code");
    let unknown = run(&home, &home, &["checkout", &repo_url, "nosuch"]);
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("has no slice nosuch; it has deep"));

    let deleted = json(
        &home,
        &home.join("deep"),
        &["--json", "slice", "delete", "deep"],
    );
    assert!(deleted.get("slice").is_none());
    let none = json(
        &home,
        &home,
        &["--json", "slice", "list", "--url", &repo_url],
    );
    assert_eq!(none["slices"], serde_json::json!([]));
}

#[test]
fn refresh_after_a_merge_pull_keeps_the_local_commit() {
    let (_tmp, home, repos) = setup();
    let demo = repos.join("demo");
    let (_server, url) = serve(&home, &repos);
    let token = std::fs::read_to_string(home.join("token")).expect("token");
    ok(&home, &home, &["login", &url, "--token", &token]);
    let repo_url = format!("{url}/demo.git");
    ok(
        &home,
        &home,
        &["checkout", &repo_url, "-p", "src", "--into", "co"],
    );
    let co = home.join("co");
    commit_file(&home, &co, "src/a.rs", "a2\n", "Change a");

    // The target moves; a merge pull brings the new snapshot in under a merge commit.
    commit_file(&home, &demo, "src/deep/b.rs", "b2\n", "Bob changes b");
    ok(&home, &co, &["pull", "--no-rebase"]);
    assert_eq!(
        git(&co, &["rev-list", "--parents", "-n", "1", "HEAD"])
            .split_whitespace()
            .count(),
        3,
        "the merge pull made a merge commit"
    );

    // The target moves again; the default (rebase) refresh replays the local commit onto the
    // newest snapshot instead of landing on the merged-in one.
    commit_file(&home, &demo, "src/deep/b.rs", "b3\n", "Bob changes b again");
    let pulled = json(&home, &co, &["--json", "pull"]);
    assert_eq!(pulled["result"], "rebased", "{pulled}");
    assert_eq!(pulled["refresh"]["moved"], Value::Bool(true), "{pulled}");
    assert_eq!(
        git(&co, &["rev-parse", "HEAD^"]),
        git(&co, &["rev-parse", "origin/main"]),
        "the local commit sits on the newest snapshot"
    );
    assert_eq!(git(&co, &["log", "-1", "--format=%s"]), "Change a");
    assert_eq!(
        git(&co, &["rev-list", "--parents", "-n", "1", "HEAD"])
            .split_whitespace()
            .count(),
        2,
        "the merge is gone"
    );
    assert_eq!(
        std::fs::read_to_string(co.join("src/a.rs")).ok().as_deref(),
        Some("a2\n")
    );
    assert_eq!(
        std::fs::read_to_string(co.join("src/deep/b.rs"))
            .ok()
            .as_deref(),
        Some("b3\n")
    );
}
