//! `grit checkout <url>` against a running `grit serve`: a snapshot of some
//! paths becomes a one-commit repository on a new work branch, and a server
//! without the checkout endpoint is refused with a clear error.

#![cfg(unix)]

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
