//! End-to-end `grit serve` behaviour: the system `git` client fetching and
//! pushing through [`grit_http_server::server::Server`] with tokens from the
//! [`grit_http_server::store::AuthStore`], plus the HTML pages.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Output};

use grit_http_server::server::{ServeConfig, Server, ServerError};
use grit_http_server::store::{Access, AuthStore, Token};

const ADMIN_TOKEN: &str = "grit_admin_token";
const READER_TOKEN: &str = "grit_reader_token";
const OUTSIDER_TOKEN: &str = "grit_outsider_token";

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "A U Thor")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Some git builds configure a credential helper below the system config
        // (Apple's osxkeychain); clear it so tests neither reuse nor store tokens.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_ASKPASS", "/bin/false");
    cmd
}

fn git_output(dir: &Path, args: &[&str]) -> Output {
    git_cmd(dir, args).output().expect("spawn git")
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_fails(dir: &Path, args: &[&str]) -> String {
    let out = git_output(dir, args);
    assert!(!out.status.success(), "git {args:?} unexpectedly succeeded");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = git_output(dir, &["rev-parse", rev]);
    assert!(out.status.success(), "git rev-parse {rev} failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A root with a bare `project.git` holding one commit on `main`, and a database
/// with an admin, a read-only `reader` and an `outsider` with no grants.
fn fixture(root: &Path) -> std::path::PathBuf {
    let seed = root.join("seed");
    std::fs::create_dir(&seed).expect("mkdir seed");
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README"), "hello\n").expect("write");
    git(&seed, &["add", "README"]);
    git(&seed, &["commit", "-q", "-m", "Initial <commit>"]);

    let repos = root.join("repos");
    std::fs::create_dir(&repos).expect("mkdir repos");
    git(
        root,
        &["clone", "-q", "--bare", "seed", "repos/project.git"],
    );

    let database = root.join("serve.db");
    let mut store = AuthStore::open(&database).expect("open store");
    store
        .setup("admin", &Token::from_secret(ADMIN_TOKEN))
        .expect("setup");
    let reader = store.add_user("reader", false).expect("add reader");
    store
        .add_token(&reader, &Token::from_secret(READER_TOKEN))
        .expect("reader token");
    store
        .grant(&reader, "project", Access::Read)
        .expect("grant reader");
    let outsider = store.add_user("outsider", false).expect("add outsider");
    store
        .add_token(&outsider, &Token::from_secret(OUTSIDER_TOKEN))
        .expect("outsider token");
    database
}

fn start(root: &Path, database: &Path) -> SocketAddr {
    let server = Server::bind(
        ServeConfig {
            root: root.to_path_buf(),
            database: database.to_path_buf(),
            repository_options: grit_protocol::repository_options_from_process(),
        },
        "127.0.0.1:0",
    )
    .expect("bind server");
    let addr = server.local_addr().expect("local addr");
    std::thread::spawn(move || server.run());
    addr
}

fn url(addr: SocketAddr, token: Option<&str>, repo: &str) -> String {
    match token {
        Some(token) => format!("http://user:{token}@{addr}/{repo}"),
        None => format!("http://{addr}/{repo}"),
    }
}

/// A plain HTTP/1.0 GET returning the status line and body.
fn http_get(addr: SocketAddr, path: &str, token: Option<&str>) -> (String, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n"
    )
    .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.lines().next().unwrap_or_default().to_owned();
    (status, body.to_owned())
}

#[test]
fn tokens_gate_clone_fetch_and_push() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);
    let head = rev_parse(&root.join("seed"), "HEAD");

    // Admin clones with protocol v2 and v0, and with or without `.git`.
    let admin_v2 = url(addr, Some(ADMIN_TOKEN), "project.git");
    git(
        root,
        &[
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            &admin_v2,
            "admin",
        ],
    );
    assert_eq!(rev_parse(&root.join("admin"), "HEAD"), head);
    let admin_v0 = url(addr, Some(ADMIN_TOKEN), "project");
    git(
        root,
        &[
            "-c",
            "protocol.version=0",
            "clone",
            "-q",
            &admin_v0,
            "admin-v0",
        ],
    );
    assert_eq!(rev_parse(&root.join("admin-v0"), "HEAD"), head);

    // Admin pushes a new commit; a v2 fetch sees it.
    let admin = root.join("admin");
    std::fs::write(admin.join("README"), "hello again\n").expect("write");
    git(&admin, &["commit", "-q", "-am", "Second"]);
    git(&admin, &["push", "-q", "origin", "main"]);
    let pushed = rev_parse(&admin, "HEAD");
    let v0 = root.join("admin-v0");
    git(&v0, &["-c", "protocol.version=2", "fetch", "-q", "origin"]);
    assert_eq!(rev_parse(&v0, "origin/main"), pushed);

    // The read-only user can clone but not push.
    let reader_url = url(addr, Some(READER_TOKEN), "project.git");
    git(root, &["clone", "-q", &reader_url, "reader"]);
    let reader = root.join("reader");
    assert_eq!(rev_parse(&reader, "HEAD"), pushed);
    git(&reader, &["commit", "-q", "--allow-empty", "-m", "Nope"]);
    let stderr = git_fails(&reader, &["push", "-q", "origin", "main"]);
    assert!(stderr.contains("403"), "push by reader: {stderr}");

    // No token, a wrong token, or no grant: nothing to clone.
    git_fails(
        root,
        &["clone", "-q", &url(addr, None, "project.git"), "anon"],
    );
    git_fails(
        root,
        &[
            "clone",
            "-q",
            &url(addr, Some("grit_wrong"), "project.git"),
            "wrong",
        ],
    );
    let stderr = git_fails(
        root,
        &[
            "clone",
            "-q",
            &url(addr, Some(OUTSIDER_TOKEN), "project.git"),
            "outsider",
        ],
    );
    assert!(stderr.contains("not found"), "clone by outsider: {stderr}");
}

#[test]
fn pages_list_readable_repositories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);

    let (status, _) = http_get(addr, "/", None);
    assert!(status.contains("401"), "{status}");

    let (status, body) = http_get(addr, "/", Some(READER_TOKEN));
    assert!(status.contains("200"), "{status}");
    assert!(body.contains("href=\"/project\""), "{body}");
    assert!(body.contains(">read<"), "{body}");

    let (_, body) = http_get(addr, "/", Some(OUTSIDER_TOKEN));
    assert!(body.contains("No repositories you can read."), "{body}");

    let (status, body) = http_get(addr, "/project", Some(ADMIN_TOKEN));
    assert!(status.contains("200"), "{status}");
    assert!(body.contains("Recent commits on main"), "{body}");
    assert!(body.contains("Initial &lt;commit&gt;"), "{body}");
    assert!(body.contains("2005-04-07"), "{body}");

    let (status, _) = http_get(addr, "/project", Some(OUTSIDER_TOKEN));
    assert!(status.contains("404"), "{status}");
}

#[test]
fn a_single_repository_root_is_served_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos/project.git"), &database);

    git(
        root,
        &[
            "clone",
            "-q",
            &url(addr, Some(ADMIN_TOKEN), "project.git"),
            "single",
        ],
    );
    assert_eq!(
        rev_parse(&root.join("single"), "HEAD"),
        rev_parse(&root.join("seed"), "HEAD")
    );
}

#[test]
fn a_database_without_users_refuses_to_start() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let result = Server::bind(
        ServeConfig {
            root: tmp.path().to_path_buf(),
            database: tmp.path().join("empty.db"),
            repository_options: grit_protocol::repository_options_from_process(),
        },
        "127.0.0.1:0",
    );
    assert!(matches!(result, Err(ServerError::NotSetUp)));
}
