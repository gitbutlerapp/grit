//! `grit auth <url>` against a running `grit serve`: the user approves the
//! sign-in in the "browser" (plain HTTP here), the token is stored with the
//! credential helper, used by `grit clone`, and removed by
//! `grit auth logout <url>`. Also the `--token` path with a token from
//! `grit serve token`, and the users `grit serve` creates.

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
