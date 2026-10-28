//! `grit auth <url>` against a running `grit serve`: the user approves the
//! sign-in in the "browser" (plain HTTP here), the token is stored with the
//! credential helper, used by `grit clone`, and removed by
//! `grit auth logout <url>`. Also the `--token` path with a token from
//! `grit serve token`, the users `grit serve` creates, and
//! `--username`/`--token` for a Basic-auth host that is not a grit server.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
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

/// Run `grit` with `input` on stdin.
fn run_with_input(home: &Path, dir: &Path, args: &[&str], input: &str) -> Output {
    let mut child = grit(home, dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grit");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("grit")
}

fn json_ok(out: Output, args: &[&str]) -> Value {
    assert!(
        out.status.success(),
        "grit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("JSON output")
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

/// A plain HTTP/1.0 request to `url`'s host, as a browser would send it.
/// Returns the head (status line and headers) and the body.
fn browser(
    url: &str,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    form: &str,
) -> (String, String) {
    let host = url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(host).expect("connect");
    let mut request = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(cookie) = cookie {
        request.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    if method == "POST" {
        request.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            form.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(form);
    stream.write_all(request.as_bytes()).expect("send request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    (head.to_owned(), body.to_owned())
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| {
            let value: String = value
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                        char::from(b).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Approve the sign-in `code` as `email` the way a person would in the browser.
fn approve_in_browser(url: &str, email: &str, password: &str, code: &str) {
    let (head, _) = browser(
        url,
        "POST",
        "/.grit/signin",
        None,
        &form(&[("email", email), ("password", password), ("next", "/")]),
    );
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .and_then(|value| value.split(';').next())
        .unwrap_or_else(|| panic!("no session cookie in {head}"))
        .to_owned();
    let (_, page) = browser(
        url,
        "GET",
        &format!("/.grit/authorize?code={code}"),
        Some(&cookie),
        "",
    );
    assert!(page.contains(code), "{page}");
    let csrf = page
        .split_once("name=\"csrf\" value=\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .unwrap_or_else(|| panic!("no csrf in {page}"))
        .to_owned();
    let (_, done) = browser(
        url,
        "POST",
        "/.grit/authorize",
        Some(&cookie),
        &form(&[("csrf", &csrf), ("code", code), ("decision", "approve")]),
    );
    assert!(done.contains("Signed in"), "{done}");
}

#[test]
fn signing_in_in_the_browser_stores_a_token_that_clone_uses() {
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

    // Passwords come from stdin when there is no terminal; none means a
    // generated, temporary one.
    let setup_args = [
        "--json",
        "serve",
        "--root",
        ".",
        "--setup",
        "--email",
        "admin@example.com",
    ];
    let admin = json_ok(
        run_with_input(&home, &repos, &setup_args, "admin password\n"),
        &setup_args,
    );
    assert_eq!(admin["user"], "admin@example.com");
    assert_eq!(admin["temporary"], Value::Bool(false));
    assert!(admin.get("password").is_none(), "{admin}");

    let bob_args = [
        "--json",
        "serve",
        "--root",
        ".",
        "add-user",
        "Bob@Example.com",
    ];
    let bob = json_ok(
        run_with_input(&home, &repos, &bob_args, "bob password\n"),
        &bob_args,
    );
    assert_eq!(bob["user"], "bob@example.com");
    let carol_args = [
        "--json",
        "serve",
        "--root",
        ".",
        "add-user",
        "carol@example.com",
    ];
    let carol = json_ok(run_with_input(&home, &repos, &carol_args, ""), &carol_args);
    assert_eq!(carol["temporary"], Value::Bool(true));
    assert_eq!(
        carol["password"].as_str().map(str::len),
        Some(19),
        "{carol}"
    );
    let short = run_with_input(
        &home,
        &repos,
        &["serve", "--root", ".", "add-user", "dave@example.com"],
        "short\n",
    );
    assert!(!short.status.success());
    let not_email = run_with_input(
        &home,
        &repos,
        &["serve", "--root", ".", "add-user", "dave"],
        "",
    );
    assert!(!not_email.status.success());

    ok(
        &home,
        &repos,
        &["serve", "--root", ".", "grant", "bob@example.com", "demo"],
    );

    let store = home.join("credentials");
    let helper = format!("store --file={}", store.display());
    ok(
        &home,
        &home,
        &["config", "--global", "credential.helper", &helper],
    );

    let (_server, url) = serve(&home, &repos);
    let host = url.trim_start_matches("http://");

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

    // `grit auth <url>` prints the page and code, then waits for the approval.
    let mut sign_in = grit(
        &home,
        &home,
        &["--json", "auth", &format!("{url}/demo.git")],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn grit auth");
    let mut prompt = BufReader::new(sign_in.stderr.take().expect("stderr"));
    let mut shown = String::new();
    let code = loop {
        let mut line = String::new();
        if prompt.read_line(&mut line).expect("read prompt") == 0 {
            panic!("grit auth exited before showing a code: {shown}");
        }
        shown.push_str(&line);
        let word = line.trim();
        if word.len() == 9 && word.as_bytes()[4] == b'-' {
            break word.to_owned();
        }
    };
    assert!(
        shown.contains(&format!("{url}/.grit/authorize?code={code}")),
        "{shown}"
    );
    approve_in_browser(&url, "bob@example.com", "bob password", &code);
    let signed_in = sign_in.wait_with_output().expect("grit auth");
    assert!(signed_in.status.success(), "grit auth failed");
    let json: Value = serde_json::from_slice(&signed_in.stdout).expect("auth JSON");
    assert_eq!(json["authenticated"], Value::Bool(true));
    assert_eq!(json["host"], host);
    assert_eq!(json["user"], "bob@example.com");
    let stored = std::fs::read_to_string(&store).expect("credential store");
    assert!(stored.contains("bob%40example.com:grit_"), "{stored}");
    assert!(stored.contains(&host.replace(':', "%3a")), "{stored}");

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
        .contains("grit_"));
    let refused = run(
        &home,
        tmp.path(),
        &["clone", &format!("{url}/demo.git"), "again"],
    );
    assert!(!refused.status.success());

    // A token from `grit serve token` signs in without a browser
    // (`grit login` is the same command as `grit auth`).
    let token_args = ["--json", "serve", "--root", ".", "token", "bob@example.com"];
    let made = json_ok(run(&home, &repos, &token_args), &token_args);
    let token = made["token"].as_str().expect("token").to_owned();
    let json: Value = serde_json::from_slice(
        &ok(&home, &home, &["--json", "login", &url, "--token", &token]).stdout,
    )
    .expect("login JSON");
    assert_eq!(json["user"], "bob@example.com");
    assert!(std::fs::read_to_string(&store)
        .unwrap_or_default()
        .contains(&token));
    ok(
        &home,
        tmp.path(),
        &["clone", &format!("{url}/demo.git"), "with-token"],
    );
}

#[test]
fn a_token_without_a_url_goes_to_the_default_host_and_github_urls_use_the_device_flow() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home: PathBuf = tmp.path().to_path_buf();
    // Without a URL the token is checked with the default host (grithub, here
    // a port nothing listens on), not refused as a usage error.
    let no_url = grit(&home, &home, &["auth", "--token", "grit_x"])
        .env("GRIT_DEFAULT_HOST", "http://127.0.0.1:1")
        .output()
        .expect("spawn grit");
    assert!(!no_url.status.success(), "{no_url:?}");
    assert_ne!(no_url.status.code(), Some(2), "{no_url:?}");
    assert!(
        String::from_utf8_lossy(&no_url.stderr).contains("127.0.0.1:1"),
        "{}",
        String::from_utf8_lossy(&no_url.stderr)
    );
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
    let no_token = run(
        &home,
        &home,
        &["auth", "https://github.com", "--username", "octocat"],
    );
    assert_eq!(no_token.status.code(), Some(2), "{no_token:?}");
}

/// `grit-http-server --require-auth alice:s3cret` on an ephemeral port: a
/// Basic-auth host that is not a `grit serve` host. Returns its root URL.
fn basic_auth_server(root: &Path) -> (ServerGuard, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let child = Command::new(grit_test_support::grit_http_server_binary())
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--require-auth")
        .arg("alice:s3cret")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn grit-http-server");
    let guard = ServerGuard(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "grit-http-server did not listen"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    (guard, format!("http://127.0.0.1:{port}"))
}

fn system_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The `--json` error object a failed command prints on stdout.
fn json_error(out: &Output) -> Value {
    assert!(!out.status.success(), "expected a failure");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON error: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn signing_in_to_a_host_that_is_not_a_grit_server_points_at_username_and_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(&root).expect("mkdir srv");
    let (_server, url) = basic_auth_server(&root);
    let repo_url = format!("{url}/auth.git");

    for args in [
        vec!["auth", repo_url.as_str()],
        vec!["auth", repo_url.as_str(), "--token", "s3cret"],
    ] {
        let out = run(&home, &home, &args);
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("is not a `grit serve` host"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "grit auth {url}/ --username <user> --token <token>"
            )),
            "{stderr}"
        );
        assert!(stderr.contains("credential helper"), "{stderr}");
        assert!(stderr.contains("GIT_ASKPASS"), "{stderr}");
        assert!(!stderr.contains("HTTP 404"), "{stderr}");
        assert!(!stderr.contains(".grit/"), "{stderr}");
    }
    assert!(
        !home.join(".gitconfig").exists(),
        "a failed sign-in must not set up a credential helper"
    );
}

#[test]
fn username_and_token_are_stored_for_any_host_and_push_uses_them() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let root = tmp.path().join("srv");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(&work).expect("mkdir work");
    system_git(
        &root.parent().expect("parent"),
        &["init", "-q", "--bare", "srv/auth.git"],
    );
    let (_server, url) = basic_auth_server(&root);
    let host = url.trim_start_matches("http://").to_owned();
    let repo_url = format!("{url}/auth.git");

    ok(&home, &work, &["init"]);
    std::fs::write(work.join("a"), "a\n").expect("write a");
    ok(&home, &work, &["commit", "-m", "a"]);
    ok(&home, &work, &["remote", "add", "origin", &repo_url]);

    // Nothing stored: the hint names `grit auth <url> --username`, not a helper to configure.
    let missing = run(&home, &work, &["--json", "push", "-u", "origin", "main"]);
    assert!(!missing.status.success());
    let err = json_error(&missing);
    assert_eq!(err["kind"], "auth_required", "{err}");
    let hint = err["hint"].as_str().expect("hint");
    assert!(
        hint.contains(&format!(
            "grit auth {repo_url} --username <user> --token <token>"
        )),
        "{hint}"
    );

    let store = home.join("credentials");
    let helper = format!("store --file={}", store.display());
    ok(
        &home,
        &home,
        &["config", "--global", "credential.helper", &helper],
    );

    // The helper is configured but empty: say so instead of asking to configure one.
    let empty = run(&home, &work, &["--json", "push", "-u", "origin", "main"]);
    let err = json_error(&empty);
    assert_eq!(err["kind"], "auth_required", "{err}");
    let hint = err["hint"].as_str().expect("hint");
    assert!(
        hint.contains("your credential helper has no credentials"),
        "{hint}"
    );
    assert!(!hint.contains(&helper), "{hint}");
    assert!(!hint.contains("configure a credential helper"), "{hint}");

    let stored = json_ok(
        run(
            &home,
            &home,
            &[
                "--json",
                "auth",
                &repo_url,
                "--username",
                "alice",
                "--token",
                "s3cret",
            ],
        ),
        &["auth"],
    );
    assert_eq!(stored["authenticated"], Value::Bool(true));
    assert_eq!(stored["verified"], Value::Bool(false));
    assert_eq!(stored["host"], host.as_str());
    assert_eq!(stored["user"], "alice");
    let entry = format!("http://alice:s3cret@{}", host.replace(':', "%3a"));
    assert_eq!(
        std::fs::read_to_string(&store).expect("store").trim(),
        entry
    );
    let markdown = ok(
        &home,
        &home,
        &[
            "--markdown",
            "auth",
            &url,
            "--username",
            "alice",
            "--token",
            "s3cret",
        ],
    );
    assert!(String::from_utf8_lossy(&markdown.stdout).contains("Stored a token for **`alice`**"));

    ok(&home, &work, &["push", "-u", "origin", "main"]);
    let bare = root.join("auth.git");
    system_git(&bare, &["fsck", "--strict"]);
    assert_eq!(
        system_git(&bare, &["rev-parse", "refs/heads/main"]),
        system_git(&work, &["rev-parse", "HEAD"])
    );

    // A stored credential the host refuses is reported as rejected, then erased (as Git does).
    std::fs::write(
        &store,
        format!("http://alice:WRONG@{}\n", host.replace(':', "%3a")),
    )
    .expect("write store");
    std::fs::write(work.join("b"), "b\n").expect("write b");
    ok(&home, &work, &["commit", "-m", "b"]);
    let rejected = run(&home, &work, &["--json", "push", "origin", "main"]);
    assert!(!rejected.status.success());
    let err = json_error(&rejected);
    assert_eq!(err["kind"], "auth_rejected", "{err}");
    assert!(
        err["error"]
            .as_str()
            .is_some_and(|e| e.contains("rejected the credentials")),
        "{err}"
    );
    assert!(
        err["hint"]
            .as_str()
            .is_some_and(|h| h.contains("update the credentials your credential helper stores")),
        "{err}"
    );
    assert!(!err.to_string().contains("WRONG"), "{err}");
    assert_eq!(std::fs::read_to_string(&store).expect("store"), "");
}

/// A repository with one commit whose `origin` is `repo_url`.
fn repo_with_origin(home: &Path, work: &Path, repo_url: &str) {
    std::fs::create_dir_all(work).expect("mkdir work");
    ok(home, work, &["init"]);
    std::fs::write(work.join("a"), "a\n").expect("write a");
    ok(home, work, &["commit", "-m", "a"]);
    ok(home, work, &["remote", "add", "origin", repo_url]);
}

#[test]
fn credential_helper_commands_never_appear_in_auth_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&home).expect("mkdir home");
    system_git(tmp.path(), &["init", "-q", "--bare", "srv/auth.git"]);
    let (_server, url) = basic_auth_server(&root);
    let work = tmp.path().join("work");
    repo_with_origin(&home, &work, &format!("{url}/auth.git"));
    // Supplies a password but no user name, so the credential stays incomplete.
    ok(
        &home,
        &home,
        &[
            "config",
            "--global",
            "credential.helper",
            "!echo password=literal-secret",
        ],
    );

    for mode in [None, Some("--json"), Some("--markdown")] {
        let mut args: Vec<&str> = mode.into_iter().collect();
        args.extend(["push", "-u", "origin", "main"]);
        let out = run(&home, &work, &args);
        assert!(!out.status.success(), "{args:?}");
        let shown = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(shown.contains("has no credentials"), "{args:?}: {shown}");
        assert!(!shown.contains("literal-secret"), "{args:?}: {shown}");
        assert!(!shown.contains("!echo"), "{args:?}: {shown}");
    }

    // The helper exits without reading its request, so writing it can hit a closed pipe;
    // that must fail the request, never kill grit with SIGPIPE. Repeat to catch the race.
    for _ in 0..20 {
        for args in [
            ["--json", "push", "origin", "main"],
            ["--json", "fetch", "origin", "main"],
        ] {
            let out = run(&home, &work, &args);
            assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("has no credentials"),
                "{args:?}: {out:?}"
            );
        }
    }
}

#[test]
fn username_and_token_follow_use_http_path_and_push_uses_them() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&home).expect("mkdir home");
    system_git(tmp.path(), &["init", "-q", "--bare", "srv/auth.git"]);
    let (_server, url) = basic_auth_server(&root);
    let host = url.trim_start_matches("http://").replace(':', "%3a");
    let repo_url = format!("{url}/auth.git");
    let work = tmp.path().join("work");
    repo_with_origin(&home, &work, &repo_url);

    let store = home.join("credentials");
    let helper = format!("store --file={}", store.display());
    ok(
        &home,
        &home,
        &["config", "--global", "credential.helper", &helper],
    );
    ok(
        &home,
        &home,
        &["config", "--global", "credential.useHttpPath", "true"],
    );

    ok(
        &home,
        &home,
        &[
            "auth",
            &repo_url,
            "--username",
            "alice",
            "--token",
            "s3cret",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(&store).expect("store").trim(),
        format!("http://alice:s3cret@{host}/auth.git")
    );

    // System git finds the same per-repository entry.
    let fill = Command::new("git")
        .args(["-c", &format!("credential.helper={helper}")])
        .args(["-c", "credential.useHttpPath=true", "credential", "fill"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(format!("url={repo_url}\n\n").as_bytes())?;
            child.wait_with_output()
        })
        .expect("git credential fill");
    assert!(fill.status.success(), "git credential fill failed");
    assert!(
        String::from_utf8_lossy(&fill.stdout).contains("password=s3cret"),
        "{}",
        String::from_utf8_lossy(&fill.stdout)
    );

    ok(&home, &work, &["push", "-u", "origin", "main"]);
    let bare = root.join("auth.git");
    system_git(&bare, &["fsck", "--strict"]);
    assert_eq!(
        system_git(&bare, &["rev-parse", "refs/heads/main"]),
        system_git(&work, &["rev-parse", "HEAD"])
    );
}
