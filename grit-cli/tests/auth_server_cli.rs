//! `grit auth <url>` against a running `grit serve`: the token is checked,
//! stored with the credential helper, used by `grit clone`, and removed by
//! `grit auth logout <url>`.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

/// A `grit` command with config isolated to `home` (no system config, so no
/// platform keychain helper).
fn grit(home: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(GRIT);
    cmd.current_dir(dir)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
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

/// Kills the server when the test ends, pass or fail.
struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start `grit serve` on an ephemeral port and return its root URL.
fn serve(home: &Path, root: &Path) -> (ServerGuard, String) {
    let mut child = grit(
        home,
        root,
        &["serve", "--root", ".", "--bind", "127.0.0.1:0"],
    )
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn grit serve");
    let stderr = child.stderr.take().expect("server stderr");
    let guard = ServerGuard(child);
    let mut stderr = BufReader::new(stderr);
    let mut first = String::new();
    stderr.read_line(&mut first).expect("read server banner");
    // Keep draining: the server writes more lines and must not hit a closed pipe.
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let url = first
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .unwrap_or_else(|| panic!("no URL in server banner: {first}"))
        .trim_end_matches('/')
        .to_owned();
    (guard, url)
}

#[test]
fn signing_in_stores_a_token_that_clone_uses() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(repos.join("demo")).expect("mkdir demo");
    let demo = repos.join("demo");
    ok(&home, &demo, &["init"]);
    std::fs::write(demo.join("README"), "hello\n").expect("write README");
    ok(&home, &demo, &["add", "README"]);
    ok(&home, &demo, &["commit", "-m", "Initial commit"]);

    ok(&home, &repos, &["serve", "--root", ".", "--setup"]);
    let bob: Value = serde_json::from_slice(
        &ok(
            &home,
            &repos,
            &["--json", "serve", "--root", ".", "add-user", "bob"],
        )
        .stdout,
    )
    .expect("add-user JSON");
    let token = bob["token"].as_str().expect("token").to_owned();
    ok(
        &home,
        &repos,
        &["serve", "--root", ".", "grant", "bob", "demo"],
    );

    let store = home.join("credentials");
    let helper = format!("store --file={}", store.display());
    ok(
        &home,
        &home,
        &["config", "--global", "credential.helper", &helper],
    );

    let (_server, url) = serve(&home, &repos);

    // A wrong token is refused and nothing is stored.
    let refused = run(&home, &home, &["auth", &url, "--token", "grit_wrong"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("rejected the token"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(std::fs::read_to_string(&store)
        .unwrap_or_default()
        .is_empty());

    // The token is read from stdin and stored under bob's name.
    let mut sign_in = grit(
        &home,
        &home,
        &["--json", "auth", &format!("{url}/demo.git")],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn grit auth");
    writeln!(sign_in.stdin.take().expect("stdin"), "{token}").expect("write token");
    let signed_in = sign_in.wait_with_output().expect("grit auth");
    assert!(
        signed_in.status.success(),
        "grit auth failed: {}",
        String::from_utf8_lossy(&signed_in.stderr)
    );
    let json: Value = serde_json::from_slice(&signed_in.stdout).expect("auth JSON");
    let host = url.trim_start_matches("http://");
    assert_eq!(json["authenticated"], Value::Bool(true));
    assert_eq!(json["host"], host);
    assert_eq!(json["user"], "bob");
    let stored = std::fs::read_to_string(&store).expect("credential store");
    assert!(stored.contains("bob:"), "{stored}");
    assert!(stored.contains(&token), "{stored}");

    // Clone finds the stored token without one in the URL.
    ok(
        &home,
        tmp.path(),
        &["clone", &format!("{url}/demo.git"), "copy"],
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("copy/README")).expect("cloned README"),
        "hello\n"
    );

    // Signing out erases it, and clone is refused again.
    let out: Value =
        serde_json::from_slice(&ok(&home, &home, &["--json", "auth", "logout", &url]).stdout)
            .expect("logout JSON");
    assert_eq!(out["logged_out"], Value::Bool(true));
    assert_eq!(out["host"], host);
    assert!(!std::fs::read_to_string(&store)
        .unwrap_or_default()
        .contains(&token));
    let refused = run(
        &home,
        tmp.path(),
        &["clone", &format!("{url}/demo.git"), "again"],
    );
    assert!(!refused.status.success());
}

#[test]
fn token_requires_a_url_and_github_urls_use_the_device_flow() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home: PathBuf = tmp.path().to_path_buf();
    let no_url = run(&home, &home, &["auth", "--token", "grit_x"]);
    assert_eq!(no_url.status.code(), Some(2), "{no_url:?}");
    let github = run(
        &home,
        &home,
        &["auth", "https://github.com", "--token", "grit_x"],
    );
    assert!(!github.status.success());
    assert!(
        String::from_utf8_lossy(&github.stderr).contains("--token is for `grit serve` hosts"),
        "{}",
        String::from_utf8_lossy(&github.stderr)
    );
}
