//! Cross-authority redirect regression tests for smart HTTP (issue #895).
//!
//! A tiny redirect listener on one port sends `302` to a `grit-http-server` on
//! another port (a different HTTP origin). Asserts cached Basic auth and
//! host-only cookies from the gateway origin are not forwarded on redirected
//! GET/POST exchanges to the target authority.
//!
//!   cargo test -p grit-lib --features http-ureq --test transport_http_redirect_auth

#![cfg(feature = "http-ureq")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use grit_lib::config::ConfigSet;
use grit_lib::credentials::{Credential, CredentialProvider};
use grit_lib::fetch::NoProgress;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, TagMode};
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::{http_client_arc, http_fetch};

const USER: &str = "alice";
const PASS: &str = "s3cr3t";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn build_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
}

fn free_port() -> Option<u16> {
    static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..200 {
        let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let p = l.local_addr().ok()?.port();
        drop(l);
        if !used.contains(&p) {
            used.push(p);
            return Some(p);
        }
    }
    None
}

fn find_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?;
    let profile = deps.parent()?;
    for cand in [profile.join(name), deps.join(name)] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn spawn_authed_server(
    server_bin: &Path,
    root: &Path,
    port: u16,
    log_headers: &Path,
) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--require-auth")
        .arg(format!("{USER}:{PASS}"))
        .arg("--log-headers")
        .arg(log_headers)
        .env_remove("GUST_BIN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

fn wait_ready(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct RedirectGuard {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for RedirectGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Accept HTTP/1.x on `listen_port` and redirect `/gateway/repo.git/*` to the
/// target origin (`target_port`) under `/secure/repo.git`.
fn spawn_cross_origin_redirect(listen_port: u16, target_port: u16) -> RedirectGuard {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let listener = TcpListener::bind(("127.0.0.1", listen_port)).expect("bind redirect");
        listener
            .set_nonblocking(true)
            .expect("redirect nonblocking");
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = handle_redirect_request(&mut stream, target_port);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    RedirectGuard {
        stop,
        handle: Some(handle),
    }
}

fn handle_redirect_request(stream: &mut TcpStream, target_port: u16) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if method != "GET" || !path.starts_with("/gateway/repo.git") {
        let body = b"not found";
        let resp = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(resp.as_bytes())?;
        stream.write_all(body)?;
        return Ok(());
    }
    let (path_only, query) = match path.split_once('?') {
        Some((p, q)) => (p, format!("?{q}")),
        None => (path, String::new()),
    };
    let suffix = path_only.strip_prefix("/gateway/repo.git").unwrap_or("");
    let location = format!("http://127.0.0.1:{target_port}/secure/repo.git{suffix}{query}");
    let resp = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(resp.as_bytes())?;
    Ok(())
}

struct StaticProvider {
    username: String,
    password: String,
}

impl CredentialProvider for StaticProvider {
    fn fill(&self, input: &Credential) -> grit_lib::error::Result<Credential> {
        let mut out = input.clone();
        out.username = Some(self.username.clone());
        out.password = Some(self.password.clone());
        Ok(out)
    }

    fn approve(&self, _cred: &Credential) -> grit_lib::error::Result<()> {
        Ok(())
    }

    fn reject(&self, _cred: &Credential) -> grit_lib::error::Result<()> {
        Ok(())
    }
}

fn read_logged_headers(path: &Path) -> String {
    for _ in 0..80 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if !s.is_empty() {
                return s;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    _target: ServerGuard,
    _redirect: RedirectGuard,
    gateway_url: String,
    local_git: PathBuf,
    main_oid: ObjectId,
    headers_log: PathBuf,
}

fn setup_fixture() -> Option<Fixture> {
    let grit_bin = find_binary("grit")?;
    let server_bin = find_binary("grit-http-server")?;

    let tmp = tempfile::tempdir().ok()?;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).ok()?;
    build_source(&work);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).ok()?;
    let source = root.join("secure/repo.git");
    git(&work, &["clone", "-q", "--bare", ".", source.to_str()?]);
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let main_oid = rev_parse(&source, "refs/heads/main");

    let target_port = free_port()?;
    let gateway_port = free_port()?;
    let headers_log = tmp.path().join("headers.log");
    let child = spawn_authed_server(&server_bin, &root, target_port, &headers_log)?;
    let target = ServerGuard(child);
    if !wait_ready(target_port) {
        return None;
    }
    let redirect = spawn_cross_origin_redirect(gateway_port, target_port);
    if !wait_ready(gateway_port) {
        return None;
    }

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).ok()?;
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let gateway_url = format!("http://127.0.0.1:{gateway_port}/gateway/repo.git");

    Some(Fixture {
        _tmp: tmp,
        _target: target,
        _redirect: redirect,
        gateway_url,
        local_git,
        main_oid,
        headers_log,
    })
}

fn fetch_opts() -> FetchOptions {
    FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    }
}

#[test]
fn cross_authority_redirect_does_not_forward_cached_basic_auth() {
    let Some(fx) = setup_fixture() else {
        eprintln!("SKIP: redirect auth fixture unavailable");
        return;
    };

    let wrong_basic = base64::engine::general_purpose::STANDARD.encode("wronguser:wrongpass");
    let client = UreqHttpClient::new()
        .with_credential_provider(Box::new(StaticProvider {
            username: USER.to_owned(),
            password: PASS.to_owned(),
        }))
        .with_git_protocol("version=2");
    client.seed_cached_basic_auth(&fx.gateway_url, "wronguser", "wrongpass");

    http_fetch(
        client.clone(),
        &fx.local_git,
        &fx.gateway_url,
        &fetch_opts(),
        &mut NoProgress,
    )
    .expect("fetch through cross-origin redirect");

    let got = resolve_ref(&fx.local_git, "refs/remotes/origin/main").expect("origin/main");
    assert_eq!(got, fx.main_oid);

    let logged = read_logged_headers(&fx.headers_log);
    assert!(
        !logged.contains(&wrong_basic),
        "redirected target must not receive cached auth from the gateway origin; log:\n{logged}"
    );
    let ok_basic = base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}"));
    assert!(
        logged.contains(&ok_basic),
        "target should receive credentials filled for its own URL; log:\n{logged}"
    );
}

#[test]
fn cross_authority_redirect_does_not_forward_host_only_cookies() {
    let Some(fx) = setup_fixture() else {
        eprintln!("SKIP: redirect auth fixture unavailable");
        return;
    };

    let cookie_file = fx._tmp.path().join("cookies.txt");
    std::fs::write(&cookie_file, "Set-Cookie: HostOnly=leaktoken\n").unwrap();

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("http.cookieFile", cookie_file.to_str().unwrap())
        .unwrap();
    let client = UreqHttpClient::from_config(&cfg)
        .expect("from_config")
        .with_credential_provider(Box::new(StaticProvider {
            username: USER.to_owned(),
            password: PASS.to_owned(),
        }))
        .with_git_protocol("version=2");

    http_fetch(
        client.clone(),
        &fx.local_git,
        &fx.gateway_url,
        &fetch_opts(),
        &mut NoProgress,
    )
    .expect("fetch with host-only cookie file");

    let logged = read_logged_headers(&fx.headers_log).to_lowercase();
    assert!(
        !logged.contains("hostonly=leaktoken"),
        "host-only cookie must not reach the redirected authority; log:\n{logged}"
    );
}
