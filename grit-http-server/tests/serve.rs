//! End-to-end `grit serve` behaviour: the system `git` client fetching and
//! pushing through [`grit_http_server::server::Server`] with tokens from the
//! [`grit_http_server::store::AuthStore`], the web pages with password
//! sign-in, and the browser-approved command-line sign-in.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Output};

use grit_http_server::bundles::BundleSettings;
use grit_http_server::server::{ServeConfig, Server, ServerError};
use grit_http_server::store::{Access, AuthStore, HashedPassword, Token};

const ADMIN_EMAIL: &str = "admin@example.com";
const ADMIN_PASSWORD: &str = "admin password";
const READER_EMAIL: &str = "reader@example.com";
const READER_PASSWORD: &str = "reader password";
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
/// with an admin, a read-only reader and an outsider with no grants, each with
/// a token.
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
    let password = |p: &str| HashedPassword::new(p).expect("hash password");
    let admin = store
        .setup(ADMIN_EMAIL, &password(ADMIN_PASSWORD), false)
        .expect("setup");
    store
        .add_token(&admin, &Token::from_secret(ADMIN_TOKEN))
        .expect("admin token");
    let reader = store
        .add_user(READER_EMAIL, false, &password(READER_PASSWORD), false)
        .expect("add reader");
    store
        .add_token(&reader, &Token::from_secret(READER_TOKEN))
        .expect("reader token");
    store
        .grant(&reader, "project", Access::Read)
        .expect("grant reader");
    let outsider = store
        .add_user(
            "outsider@example.com",
            false,
            &password("outsider password"),
            false,
        )
        .expect("add outsider");
    store
        .add_token(&outsider, &Token::from_secret(OUTSIDER_TOKEN))
        .expect("outsider token");
    database
}

fn start(root: &Path, database: &Path) -> SocketAddr {
    start_with(
        root,
        database,
        BundleSettings {
            enabled: false,
            ..BundleSettings::default()
        },
    )
}

fn start_with(root: &Path, database: &Path, bundles: BundleSettings) -> SocketAddr {
    let server = Server::bind(
        ServeConfig {
            root: root.to_path_buf(),
            database: database.to_path_buf(),
            repository_options: grit_protocol::repository_options_from_process(),
            bundles,
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

/// An HTTP response: status line, headers and body.
struct Reply {
    status: String,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    /// The session cookie a sign-in set, as a `Cookie` header value.
    fn session_cookie(&self) -> String {
        let set = self.header("set-cookie").expect("Set-Cookie header");
        set.split(';').next().expect("cookie pair").to_owned()
    }

    /// The CSRF value of the first form on the page.
    fn csrf(&self) -> String {
        let (_, rest) = self
            .body
            .split_once("name=\"csrf\" value=\"")
            .unwrap_or_else(|| panic!("no csrf in {}", self.body));
        rest.split('"').next().unwrap_or_default().to_owned()
    }
}

/// A plain HTTP/1.0 request.
fn http(addr: SocketAddr, method: &str, path: &str, headers: &[String], body: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let mut request = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n");
    for header in headers {
        request.push_str(header);
        request.push_str("\r\n");
    }
    if method == "POST" {
        request.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let (status, head) = head.split_once("\r\n").unwrap_or((head, ""));
    Reply {
        status: status.to_owned(),
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

/// A GET with an optional bearer token, returning the status line and body.
fn http_get(addr: SocketAddr, path: &str, token: Option<&str>) -> (String, String) {
    let headers: Vec<String> = token
        .map(|t| format!("Authorization: Bearer {t}"))
        .into_iter()
        .collect();
    let reply = http(addr, "GET", path, &headers, "");
    (reply.status, reply.body)
}

/// Percent-encode a form value.
fn form_value(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={}", form_value(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Sign in through the form and return the `Cookie` header for the session.
fn sign_in(addr: SocketAddr, email: &str, password: &str) -> Reply {
    http(
        addr,
        "POST",
        "/.grit/signin",
        &[],
        &form(&[("email", email), ("password", password), ("next", "/")]),
    )
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

    let anonymous = http(addr, "GET", "/project", &[], "");
    assert!(anonymous.status.contains("303"), "{}", anonymous.status);
    assert_eq!(
        anonymous.header("location"),
        Some("/.grit/signin?next=/project")
    );

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
fn the_identity_endpoint_names_the_token_user() {
    use grit_lib::remote::grit_server_auth::{
        fetch_identity, ServerAuthError, ServerUrl, TokenCredentialProvider,
    };
    use grit_lib::remote::http_client::UreqHttpClient;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);

    let (status, _) = http_get(addr, "/.grit/user", None);
    assert!(status.contains("401"), "{status}");
    let (status, body) = http_get(addr, "/.grit/user", Some(READER_TOKEN));
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, r#"{"email":"reader@example.com","admin":false}"#);

    // The client side: a 401 challenge, then basic auth with the token.
    let server = ServerUrl::parse(&format!("http://{addr}/project.git")).expect("server url");
    let signed_in = |token: &str| {
        let http = UreqHttpClient::with_credentials(Box::new(TokenCredentialProvider::new(token)));
        fetch_identity(&http, &server)
    };
    let admin = signed_in(ADMIN_TOKEN).expect("admin identity");
    assert_eq!((admin.email.as_str(), admin.admin), (ADMIN_EMAIL, true));
    assert!(matches!(
        signed_in("grit_wrong"),
        Err(ServerAuthError::TokenRejected)
    ));
}

#[test]
fn people_sign_in_with_email_and_password() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);

    let page = http(addr, "GET", "/.grit/signin?next=/project", &[], "");
    assert!(page.status.contains("200"), "{}", page.status);
    assert!(
        page.body.contains("name=\"next\" value=\"/project\""),
        "{}",
        page.body
    );

    let wrong = sign_in(addr, READER_EMAIL, "not it at all");
    assert!(wrong.status.contains("401"), "{}", wrong.status);
    assert!(
        wrong.body.contains("Wrong email or password."),
        "{}",
        wrong.body
    );
    assert!(wrong.header("set-cookie").is_none());
    let nobody = sign_in(addr, "nobody@example.com", READER_PASSWORD);
    assert!(nobody.status.contains("401"), "{}", nobody.status);

    let signed_in = sign_in(addr, "Reader@Example.com", READER_PASSWORD);
    assert!(signed_in.status.contains("303"), "{}", signed_in.status);
    assert_eq!(signed_in.header("location"), Some("/"));
    let set_cookie = signed_in.header("set-cookie").unwrap_or_default();
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    let cookie = format!("Cookie: {}", signed_in.session_cookie());

    let home = http(addr, "GET", "/", &[cookie.clone()], "");
    assert!(home.status.contains("200"), "{}", home.status);
    assert!(home.body.contains("href=\"/project\""), "{}", home.body);
    assert!(home.body.contains(READER_EMAIL), "{}", home.body);
    assert!(
        !home.body.contains("/.grit/users"),
        "no admin link for readers"
    );
    let users = http(addr, "GET", "/.grit/users", &[cookie.clone()], "");
    assert!(users.status.contains("403"), "{}", users.status);

    // Forms need the session's CSRF value.
    let forged = http(
        addr,
        "POST",
        "/.grit/signout",
        &[cookie.clone()],
        "csrf=nope",
    );
    assert!(forged.status.contains("303"));
    assert!(http(addr, "GET", "/", &[cookie.clone()], "")
        .status
        .contains("200"));
    let out = http(
        addr,
        "POST",
        "/.grit/signout",
        &[cookie.clone()],
        &form(&[("csrf", &home.csrf())]),
    );
    assert!(out.status.contains("303"), "{}", out.status);
    let after = http(addr, "GET", "/", &[cookie], "");
    assert!(after.status.contains("303"), "signed out: {}", after.status);
}

#[test]
fn admins_add_users_who_then_choose_a_password() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);

    let admin = format!(
        "Cookie: {}",
        sign_in(addr, ADMIN_EMAIL, ADMIN_PASSWORD).session_cookie()
    );
    let users = http(addr, "GET", "/.grit/users", &[admin.clone()], "");
    assert!(users.status.contains("200"), "{}", users.status);
    assert!(users.body.contains(READER_EMAIL), "{}", users.body);
    let csrf = users.csrf();

    let added = http(
        addr,
        "POST",
        "/.grit/users",
        &[admin.clone()],
        &form(&[("csrf", &csrf), ("email", "carol@example.com")]),
    );
    assert!(added.status.contains("200"), "{}", added.status);
    let password = added
        .body
        .split_once("Password: ")
        .and_then(|(_, rest)| rest.split('<').next())
        .unwrap_or_else(|| panic!("no password in {}", added.body))
        .trim()
        .to_owned();
    assert_eq!(password.len(), 19, "{password}");
    assert!(added.body.contains("temporary password"), "{}", added.body);

    let dup = http(
        addr,
        "POST",
        "/.grit/users",
        &[admin.clone()],
        &form(&[("csrf", &csrf), ("email", "carol@example.com")]),
    );
    assert!(dup.body.contains("already exists"), "{}", dup.body);
    let granted = http(
        addr,
        "POST",
        "/.grit/grants",
        &[admin],
        &form(&[
            ("csrf", &csrf),
            ("email", "carol@example.com"),
            ("repository", "project"),
            ("access", "write"),
        ]),
    );
    assert!(
        granted
            .body
            .contains("carol@example.com now has write access to project"),
        "{}",
        granted.body
    );

    // Carol's first sign-in goes to the password page, and pages wait for it.
    let first = sign_in(addr, "carol@example.com", &password);
    assert_eq!(first.header("location"), Some("/.grit/password?next=/"));
    let carol = format!("Cookie: {}", first.session_cookie());
    let held = http(addr, "GET", "/project", &[carol.clone()], "");
    assert_eq!(
        held.header("location"),
        Some("/.grit/password?next=/project")
    );

    let form_page = http(
        addr,
        "GET",
        "/.grit/password?next=/project",
        &[carol.clone()],
        "",
    );
    assert!(
        !form_page.body.contains("name=\"current\""),
        "{}",
        form_page.body
    );
    let csrf = form_page.csrf();
    let mismatch = http(
        addr,
        "POST",
        "/.grit/password",
        &[carol.clone()],
        &form(&[
            ("csrf", &csrf),
            ("new", "carol's own"),
            ("confirm", "different"),
        ]),
    );
    assert!(
        mismatch.body.contains("The two passwords differ."),
        "{}",
        mismatch.body
    );
    let short = http(
        addr,
        "POST",
        "/.grit/password",
        &[carol.clone()],
        &form(&[("csrf", &csrf), ("new", "short"), ("confirm", "short")]),
    );
    assert!(
        short.body.contains("at least 8 characters"),
        "{}",
        short.body
    );
    let changed = http(
        addr,
        "POST",
        "/.grit/password",
        &[carol.clone()],
        &form(&[
            ("csrf", &csrf),
            ("next", "/project"),
            ("new", "carol's own password"),
            ("confirm", "carol's own password"),
        ]),
    );
    assert_eq!(changed.header("location"), Some("/project"));
    let page = http(addr, "GET", "/project", &[carol], "");
    assert!(page.status.contains("200"), "{}", page.status);
    assert!(page.body.contains(">write<"), "{}", page.body);

    assert!(sign_in(addr, "carol@example.com", &password)
        .status
        .contains("401"));
    assert_eq!(
        sign_in(addr, "carol@example.com", "carol's own password").header("location"),
        Some("/")
    );
}

#[test]
fn the_command_line_signs_in_through_the_browser() {
    use grit_lib::remote::github_auth::SystemPollClock;
    use grit_lib::remote::grit_server_auth::{LoginFlow, ServerAuthError, ServerUrl};
    use grit_lib::remote::http_client::UreqHttpClient;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);
    let server = ServerUrl::parse(&format!("http://{addr}/project.git")).expect("server url");
    let http_client = UreqHttpClient::new();
    let flow = LoginFlow::new(&http_client, &server);

    let login = flow.start().expect("start login");
    let authorize = server.authorize_url(&login.user_code);
    let path = authorize
        .split_once(&addr.to_string())
        .map(|(_, path)| path.to_owned())
        .expect("authorize path");

    // Not signed in: the page sends the browser to sign in first, then back.
    let anonymous = http(addr, "GET", &path, &[], "");
    let location = anonymous.header("location").unwrap_or_default().to_owned();
    assert!(
        location.starts_with("/.grit/signin?next=/.grit/authorize"),
        "{location}"
    );

    let cookie = format!(
        "Cookie: {}",
        sign_in(addr, READER_EMAIL, READER_PASSWORD).session_cookie()
    );
    let page = http(addr, "GET", &path, &[cookie.clone()], "");
    assert!(page.body.contains(&login.user_code), "{}", page.body);
    assert!(page.body.contains(READER_EMAIL), "{}", page.body);
    let approved = http(
        addr,
        "POST",
        "/.grit/authorize",
        &[cookie.clone()],
        &form(&[
            ("csrf", &page.csrf()),
            ("code", &login.user_code),
            ("decision", "approve"),
        ]),
    );
    assert!(approved.body.contains("Signed in"), "{}", approved.body);

    let grant = flow
        .wait(&login, &mut SystemPollClock)
        .expect("approved login");
    assert_eq!(grant.email, READER_EMAIL);
    assert!(grant.token.starts_with(Token::PREFIX));

    // The token works for Git, and the login cannot be collected twice.
    git(
        root,
        &[
            "clone",
            "-q",
            &url(addr, Some(&grant.token), "project.git"),
            "via-login",
        ],
    );
    assert!(matches!(
        flow.wait(&login, &mut SystemPollClock),
        Err(ServerAuthError::LoginExpired)
    ));

    // A declined login reports so.
    let declined = flow.start().expect("start login");
    let page = http(
        addr,
        "GET",
        &format!("/.grit/authorize?code={}", declined.user_code),
        &[cookie.clone()],
        "",
    );
    http(
        addr,
        "POST",
        "/.grit/authorize",
        &[cookie],
        &form(&[
            ("csrf", &page.csrf()),
            ("code", &declined.user_code),
            ("decision", "deny"),
        ]),
    );
    assert!(matches!(
        flow.wait(&declined, &mut SystemPollClock),
        Err(ServerAuthError::LoginDenied)
    ));
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
            bundles: BundleSettings::default(),
        },
        "127.0.0.1:0",
    );
    assert!(matches!(result, Err(ServerError::NotSetUp)));
}

/// A raw HTTP/1.0 exchange with a binary body: status line, headers, body.
fn http_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[String],
    body: &[u8],
) -> (String, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let mut request = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n");
    for header in headers {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(request.as_bytes()).expect("write request");
    stream.write_all(body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let body = response.get(split + 4..).unwrap_or_default().to_vec();
    let (status, head) = head.split_once("\r\n").unwrap_or((&head, ""));
    (status.to_owned(), head.to_owned(), body)
}

fn header_of<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn bearer(token: &str) -> String {
    format!("Authorization: Bearer {token}")
}

#[test]
fn clone_bundles_are_built_served_with_ranges_and_used_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start_with(
        &root.join("repos"),
        &database,
        BundleSettings {
            check_every: std::time::Duration::from_millis(50),
            ..BundleSettings::default()
        },
    );
    let repo = root.join("repos/project.git");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let manifest = loop {
        if let Some(m) = grit_http_server::bundles::current(&repo) {
            break m;
        }
        assert!(std::time::Instant::now() < deadline, "no bundle was built");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let path = format!("/project/bundles/{}", manifest.file);
    let size = manifest.size;

    let (status, head, body) = http_raw(addr, "GET", &path, &[bearer(ADMIN_TOKEN)], b"");
    assert!(status.contains("200"), "{status}");
    assert_eq!(body.len() as u64, size);
    assert!(body.starts_with(b"# v2 git bundle\n"));
    let etag = header_of(&head, "etag").expect("ETag").to_owned();
    assert_eq!(header_of(&head, "accept-ranges"), Some("bytes"));

    // Ranges let a download resume; a changed file (other ETag) starts over.
    let (status, head, part) = http_raw(
        addr,
        "GET",
        &path,
        &[
            bearer(READER_TOKEN),
            "Range: bytes=10-".to_owned(),
            format!("If-Range: {etag}"),
        ],
        b"",
    );
    assert!(status.contains("206"), "{status}");
    assert_eq!(
        header_of(&head, "content-range").map(str::to_owned),
        Some(format!("bytes 10-{}/{size}", size - 1))
    );
    assert_eq!(part, body[10..]);
    let (status, _, whole) = http_raw(
        addr,
        "GET",
        &path,
        &[
            bearer(ADMIN_TOKEN),
            "Range: bytes=10-".to_owned(),
            "If-Range: \"other\"".to_owned(),
        ],
        b"",
    );
    assert!(status.contains("200"), "{status}");
    assert_eq!(whole.len() as u64, size);
    let (status, _, _) = http_raw(
        addr,
        "GET",
        &path,
        &[bearer(ADMIN_TOKEN), format!("Range: bytes={size}-")],
        b"",
    );
    assert!(status.contains("416"), "{status}");

    // Bundles are gated like the repository.
    let (status, _, _) = http_raw(addr, "GET", &path, &[], b"");
    assert!(status.contains("401"), "{status}");
    let (status, _, _) = http_raw(addr, "GET", &path, &[bearer(OUTSIDER_TOKEN)], b"");
    assert!(status.contains("404"), "{status}");
    let (status, _, _) = http_raw(
        addr,
        "GET",
        "/project/bundles/0-nope.bundle",
        &[bearer(ADMIN_TOKEN)],
        b"",
    );
    assert!(status.contains("404"), "{status}");

    // Git itself clones from the advertised bundle (and keeps its refs). The
    // bundle URL carries no credentials, so Git needs a helper for it.
    let helper = format!("!f() {{ echo username=user; echo password={ADMIN_TOKEN}; }}; f");
    git(
        root,
        &[
            "-c",
            "transfer.bundleURI=true",
            "-c",
            &format!("credential.helper={helper}"),
            "clone",
            "-q",
            &url(addr, None, "project.git"),
            "from-bundle",
        ],
    );
    let clone = root.join("from-bundle");
    assert_eq!(
        rev_parse(&clone, "HEAD"),
        rev_parse(&root.join("seed"), "HEAD")
    );
    assert_eq!(
        rev_parse(&clone, "refs/bundles/heads/main"),
        rev_parse(&root.join("seed"), "HEAD")
    );
}

#[test]
fn tus_uploads_resume_and_apply_as_a_push() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let addr = start(&root.join("repos"), &database);
    let old = rev_parse(&root.join("seed"), "HEAD");

    let (_, _, advertisement) = http_raw(
        addr,
        "GET",
        "/project/info/refs?service=git-receive-pack",
        &[bearer(ADMIN_TOKEN)],
        b"",
    );
    assert!(
        String::from_utf8_lossy(&advertisement).contains("pushBundle=/push"),
        "{}",
        String::from_utf8_lossy(&advertisement)
    );

    // A bundle of one new commit on top of the server's main.
    let work = root.join("work");
    git(
        root,
        &[
            "clone",
            "-q",
            &url(addr, Some(ADMIN_TOKEN), "project"),
            "work",
        ],
    );
    std::fs::write(work.join("NEW"), "new\n").expect("write");
    git(&work, &["add", "NEW"]);
    git(&work, &["commit", "-q", "-m", "New"]);
    let new = rev_parse(&work, "HEAD");
    git(
        &work,
        &["bundle", "create", "../push.bundle", "main", "^origin/main"],
    );
    let bundle = std::fs::read(root.join("push.bundle")).expect("bundle");
    let half = bundle.len() / 2;

    let (status, head, _) = http_raw(addr, "OPTIONS", "/project/push", &[], b"");
    assert!(status.contains("204"), "{status}");
    assert_eq!(header_of(&head, "tus-version"), Some("1.0.0"));

    let length = format!("Upload-Length: {}", bundle.len());
    let tus = "Tus-Resumable: 1.0.0".to_owned();
    let (status, _, _) = http_raw(
        addr,
        "POST",
        "/project/push",
        &[tus.clone(), length.clone()],
        b"",
    );
    assert!(status.contains("401"), "{status}");
    let (status, _, _) = http_raw(
        addr,
        "POST",
        "/project/push",
        &[bearer(READER_TOKEN), tus.clone(), length.clone()],
        b"",
    );
    assert!(status.contains("403"), "{status}");
    let (status, head, _) = http_raw(
        addr,
        "POST",
        "/project/push",
        &[bearer(ADMIN_TOKEN), tus.clone(), length],
        b"",
    );
    assert!(status.contains("201"), "{status}");
    let upload = header_of(&head, "location").expect("Location").to_owned();
    assert!(upload.starts_with("/project/push/"), "{upload}");

    let patch = |offset: usize, bytes: &[u8]| {
        http_raw(
            addr,
            "PATCH",
            &upload,
            &[
                bearer(ADMIN_TOKEN),
                tus.clone(),
                format!("Upload-Offset: {offset}"),
                "Content-Type: application/offset+octet-stream".to_owned(),
            ],
            bytes,
        )
    };
    let (status, head, _) = patch(0, &bundle[..half]);
    assert!(status.contains("204"), "{status}");
    assert_eq!(
        header_of(&head, "upload-offset"),
        Some(half.to_string().as_str())
    );
    let (status, head, _) = http_raw(
        addr,
        "HEAD",
        &upload,
        &[bearer(ADMIN_TOKEN), tus.clone()],
        b"",
    );
    assert!(status.contains("200"), "{status}");
    assert_eq!(
        header_of(&head, "upload-offset"),
        Some(half.to_string().as_str())
    );
    let (status, _, _) = patch(0, &bundle[..half]);
    assert!(
        status.contains("409"),
        "a stale offset is refused: {status}"
    );

    // Applying before every byte is in is refused.
    let mut commands = Vec::new();
    let line = format!("{old} {new} refs/heads/main\0report-status");
    commands.extend_from_slice(format!("{:04x}{line}\n", line.len() + 5).as_bytes());
    commands.extend_from_slice(b"0000");
    let receive = format!("{upload}/receive");
    let (status, _, _) = http_raw(addr, "POST", &receive, &[bearer(ADMIN_TOKEN)], &commands);
    assert!(status.contains("409"), "{status}");

    let (status, _, _) = patch(half, &bundle[half..]);
    assert!(status.contains("204"), "{status}");
    let (status, _, report) = http_raw(addr, "POST", &receive, &[bearer(ADMIN_TOKEN)], &commands);
    assert!(status.contains("200"), "{status}");
    let report = String::from_utf8_lossy(&report).into_owned();
    assert!(report.contains("unpack ok"), "{report}");
    assert!(report.contains("ok refs/heads/main"), "{report}");
    assert_eq!(rev_parse(&root.join("repos/project.git"), "main"), new);

    // The upload is gone once applied.
    let (status, _, _) = http_raw(addr, "HEAD", &upload, &[bearer(ADMIN_TOKEN), tus], b"");
    assert!(status.contains("404"), "{status}");
}

/// No HTTP promisor remotes here.
struct NoHttp;

impl grit_lib::remote::HttpClientFactory for NoHttp {
    fn create(
        &self,
        _: &grit_lib::config::ConfigSet,
    ) -> grit_lib::remote::RemoteResult<Box<dyn grit_lib::remote::transport::http::HttpClient>>
    {
        Err(grit_lib::remote::RemoteError::UnsupportedScheme)
    }

    fn create_for_probe(
        &self,
        _: &grit_lib::config::ConfigSet,
    ) -> grit_lib::remote::RemoteResult<Box<dyn grit_lib::remote::transport::http::HttpClient>>
    {
        Err(grit_lib::remote::RemoteError::UnsupportedScheme)
    }
}

#[test]
fn snapshot_checkouts_of_offloaded_blobs_follow_lazy_fetch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let database = fixture(root);
    let seed = root.join("seed");
    let big: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(seed.join("big.bin"), &big).expect("write");
    git(&seed, &["add", "big.bin"]);
    git(&seed, &["commit", "-q", "-m", "big"]);
    let project = root.join("repos/project.git");
    git(
        &seed,
        &["push", "-q", project.to_str().expect("utf-8"), "main"],
    );
    let lop = root.join("lop.git");
    git(root, &["init", "-q", "--bare", "lop.git"]);
    git(
        &project,
        &["remote", "add", "lop", lop.to_str().expect("utf-8")],
    );
    {
        let repo = grit_lib::repo::Repository::open(&project, &Default::default()).expect("open");
        let mut opts = grit_lib::maintenance::OffloadOptions::new("lop", 4_000_000_000);
        opts.filter = Some(grit_lib::remote::partial_clone::ObjectFilterSpec::BlobLimit(10 * 1024));
        opts.configure_lop = true;
        let report = grit_lib::maintenance::offload_blobs(&repo, &opts).expect("offload");
        assert_eq!(report.dropped_from_main, 1);
        grit_lib::objects::pack::clear_pack_cache();
    }

    let fetcher: std::sync::Arc<dyn grit_lib::objects::promisor::PromisorFetcher> =
        std::sync::Arc::new(grit_lib::objects::promisor::DefaultPromisorFetcher::new(
            std::sync::Arc::new(NoHttp),
        ));
    let server = Server::bind(
        ServeConfig {
            root: root.join("repos"),
            database,
            repository_options: grit_protocol::repository_options_from_process()
                .with_promisor_fetcher(Some(fetcher)),
            bundles: BundleSettings {
                enabled: false,
                ..BundleSettings::default()
            },
        },
        "127.0.0.1:0",
    )
    .expect("bind server");
    let addr = server.local_addr().expect("local addr");
    std::thread::spawn(move || server.run());
    let auth = [bearer(ADMIN_TOKEN)];
    let request = br#"{"paths":[]}"#;

    // Lazy fetch is off: the snapshot would need the offloaded blob.
    let (status, _, body) = http_raw(addr, "POST", "/.grit/checkouts/project", &auth, request);
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(status.contains("409"), "{status}: {body}");
    assert!(body.contains("uploadpack.lazyFetch"), "{body}");

    // With lazy fetch on, the server fetches it from the LOP and serves the bundle.
    git(&project, &["config", "uploadpack.lazyFetch", "true"]);
    let (status, _, body) = http_raw(addr, "POST", "/.grit/checkouts/project", &auth, request);
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(status.contains("200"), "{status}: {body}");
    let info: serde_json::Value = serde_json::from_str(&body).expect("checkout info");
    let id = info["id"].as_str().expect("id");
    let (status, _, bundle) = http_raw(
        addr,
        "GET",
        &format!("/.grit/checkouts/project/{id}/bundle"),
        &auth,
        b"",
    );
    assert!(status.contains("200"), "{status}");
    std::fs::write(root.join("snap.bundle"), bundle).expect("write bundle");
    git(root, &["clone", "-q", "-b", "main", "snap.bundle", "snap"]);
    assert_eq!(std::fs::read(root.join("snap/big.bin")).expect("big"), big);
}
