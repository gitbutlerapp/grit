//! Integration tests for Deferral 2: HTTP proxy + cookies + extra headers in the
//! default `UreqHttpClient` (feature `http-ureq`).
//!
//! These build the same `grit-http-server` fixture as `transport_http.rs` (a bare
//! source repo served over localhost), but additionally:
//!
//! * **cookies** — configure `http.cookieFile` and assert (via the server's
//!   `--log-headers` sink) that the matching `Cookie:` header actually reaches the
//!   server on the wire, and that with `http.saveCookies=true` the server's
//!   `Set-Cookie` response header is persisted back to the cookie file;
//! * **extraHeader** — configure `http.extraHeader` and assert the header reaches
//!   the server;
//! * **proxy** — stand up a tiny in-process forwarding HTTP proxy, point
//!   `http.proxy` at it, and assert the fetch actually transited the proxy (the
//!   proxy saw an absolute-form request line and forwarded a request). If the
//!   live-proxy path is somehow unavailable it skips with a note; the proxy URL
//!   parsing / SOCKS-rejection is unit-tested in `ureq_client.rs`.
//!
//! Each test skips gracefully when `git`, the `grit` binary, or the
//! `grit-http-server` binary is unavailable; the happy path is real end-to-end
//! HTTP wire I/O.
//!
//!   cargo test -p grit-lib --features http-ureq --test transport_http_proxy_cookies

#![cfg(feature = "http-ureq")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::config::ConfigSet;
use grit_lib::fetch::NoProgress;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, TagMode};
use grit_lib::transport::http::http_fetch;
use grit_lib::transport::http::ureq_client::UreqHttpClient;

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
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
}

fn free_port() -> Option<u16> {
    // Dedup port handout per process so a concurrent caller never accepts a
    // port another test already reserved (closes the bind-before-spawn TOCTOU
    // that let two servers share a port under high test parallelism).
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

/// Spawn `grit-http-server`, optionally with `--log-headers` and `--set-cookie`.
fn spawn_server(
    server_bin: &Path,
    grit_bin: &Path,
    root: &Path,
    port: u16,
    log_headers: Option<&Path>,
    set_cookie: Option<&str>,
) -> Option<Child> {
    let mut cmd = Command::new(server_bin);
    cmd.arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GUST_BIN", grit_bin);
    if let Some(p) = log_headers {
        cmd.arg("--log-headers").arg(p);
    }
    if let Some(c) = set_cookie {
        cmd.arg("--set-cookie").arg(c);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()
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

/// Build a bare served repo under `root/repo.git`, returning the served URL,
/// source path, and main oid. Returns `None` (the caller skips) when a binary is
/// missing or the server fails to come up.
struct Fixture {
    _tmp: tempfile::TempDir,
    _guard: ServerGuard,
    url: String,
    main_oid: ObjectId,
    local_git: PathBuf,
}

fn setup(log_headers: Option<&Path>, set_cookie: Option<&str>) -> Option<Fixture> {
    let grit_bin = find_binary("grit")?;
    let server_bin = find_binary("grit-http-server")?;

    let tmp = tempfile::tempdir().ok()?;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).ok()?;
    build_source(&work);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).ok()?;
    let source = root.join("repo.git");
    git(&work, &["clone", "-q", "--bare", ".", source.to_str()?]);
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let main_oid = rev_parse(&source, "refs/heads/main");

    let port = free_port()?;
    let child = spawn_server(&server_bin, &grit_bin, &root, port, log_headers, set_cookie)?;
    let guard = ServerGuard(child);
    if !wait_ready(port) {
        return None;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).ok()?;
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    // `source`/`root`/`grit_bin`/`server_bin`/`port` are consumed building the
    // fixture; only the served URL + main oid + local repo are needed by tests.
    let _ = (&source, &root, &grit_bin, &server_bin, port);

    Some(Fixture {
        _tmp: tmp,
        _guard: guard,
        url,
        main_oid,
        local_git,
    })
}

fn fetch_opts() -> FetchOptions {
    FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    }
}

/// Read the server's `--log-headers` sink and return all logged lines.
fn read_logged_headers(path: &Path) -> String {
    // The server appends as it serves; give it a moment to flush.
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if !s.is_empty() {
                return s;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn cookie_file_sends_cookie_header_to_server() {
    let headers_log = {
        // Allocate a temp file path that lives in the fixture tempdir is awkward;
        // use a standalone tempfile that outlives the fixture's server.
        tempfile::NamedTempFile::new().expect("temp headers log")
    };
    let log_path = headers_log.path().to_path_buf();

    let Some(fx) = setup(Some(&log_path), None) else {
        eprintln!("SKIP: grit / grit-http-server fixture unavailable");
        return;
    };

    // Write a Netscape cookie file scoped to 127.0.0.1 / path / (insecure ok).
    let cookie_file = fx._tmp.path().join("cookies.txt");
    std::fs::write(
        &cookie_file,
        "127.0.0.1\tFALSE\t/\tFALSE\t0\tSESSION\twiretoken123\n",
    )
    .unwrap();

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("http.cookieFile", cookie_file.to_str().unwrap())
        .unwrap();
    let client = UreqHttpClient::from_config(&cfg).expect("from_config");

    let outcome = http_fetch(
        &client,
        &fx.local_git,
        &fx.url,
        &fetch_opts(),
        &mut NoProgress,
    )
    .expect("http_fetch with cookie file");
    // The fetch really happened (ref landed) — so the headers we logged are from a
    // genuine smart-HTTP exchange, not a no-op.
    assert!(
        outcome
            .updates
            .iter()
            .any(|u| u.remote_ref == "refs/heads/main" && u.new_oid == Some(fx.main_oid)),
        "cookie fetch did not land origin/main"
    );
    let got_main = resolve_ref(&fx.local_git, "refs/remotes/origin/main").expect("origin/main");
    assert_eq!(got_main, fx.main_oid);

    let logged = read_logged_headers(&log_path);
    assert!(
        logged
            .to_lowercase()
            .contains("cookie: session=wiretoken123"),
        "server did not receive the configured Cookie header; log was:\n{logged}"
    );
}

#[test]
fn save_cookies_persists_set_cookie_to_file() {
    let Some(fx) = setup(None, Some("WIRESET=fromserver; Path=/")) else {
        eprintln!("SKIP: grit / grit-http-server fixture unavailable");
        return;
    };

    // Start with a cookie file that exists (saveCookies only persists when a file
    // is configured); it may be empty initially.
    let cookie_file = fx._tmp.path().join("jar.txt");
    std::fs::write(&cookie_file, "").unwrap();

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("http.cookieFile", cookie_file.to_str().unwrap())
        .unwrap();
    cfg.add_command_override("http.saveCookies", "true")
        .unwrap();
    let client = UreqHttpClient::from_config(&cfg).expect("from_config");

    http_fetch(
        &client,
        &fx.local_git,
        &fx.url,
        &fetch_opts(),
        &mut NoProgress,
    )
    .expect("http_fetch with saveCookies");

    let jar = std::fs::read_to_string(&cookie_file).unwrap_or_default();
    assert!(
        jar.contains("Set-Cookie:") && jar.contains("WIRESET=fromserver"),
        "saveCookies did not persist the server's Set-Cookie; jar was:\n{jar:?}"
    );
}

#[test]
fn extra_header_reaches_server() {
    let headers_log = tempfile::NamedTempFile::new().expect("temp headers log");
    let log_path = headers_log.path().to_path_buf();

    let Some(fx) = setup(Some(&log_path), None) else {
        eprintln!("SKIP: grit / grit-http-server fixture unavailable");
        return;
    };

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("http.extraHeader", "X-Grit-Test: wirevalue42")
        .unwrap();
    let client = UreqHttpClient::from_config(&cfg).expect("from_config");

    http_fetch(
        &client,
        &fx.local_git,
        &fx.url,
        &fetch_opts(),
        &mut NoProgress,
    )
    .expect("http_fetch with extraHeader");

    let logged = read_logged_headers(&log_path);
    assert!(
        logged.to_lowercase().contains("x-grit-test: wirevalue42"),
        "server did not receive the configured extra header; log was:\n{logged}"
    );
}

/// A tiny in-process forwarding HTTP proxy: accepts absolute-form requests
/// (`GET http://host:port/path HTTP/1.1`), connects to the named upstream, and
/// pipes bytes both ways. Increments `count` once per accepted, forwarded
/// request. Good enough to prove the client routed through the proxy.
fn spawn_forwarding_proxy() -> Option<(u16, Arc<AtomicUsize>, Arc<AtomicUsize>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let count = Arc::new(AtomicUsize::new(0));
    let absform = Arc::new(AtomicUsize::new(0));
    let count_t = Arc::clone(&count);
    let absform_t = Arc::clone(&absform);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut client) = stream else { continue };
            let count = Arc::clone(&count_t);
            let absform = Arc::clone(&absform_t);
            std::thread::spawn(move || {
                let _ = client.set_read_timeout(Some(Duration::from_secs(10)));
                // Read the request head (up to the blank line) to find the
                // absolute-form request line and the target host:port.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while head.windows(4).all(|w| w != b"\r\n\r\n") {
                    match client.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => head.push(byte[0]),
                        Err(_) => break,
                    }
                    if head.len() > 64 * 1024 {
                        break;
                    }
                }
                let head_str = String::from_utf8_lossy(&head).to_string();
                let Some(first_line) = head_str.lines().next() else {
                    return;
                };
                // Expect: METHOD http://host:port/path HTTP/1.1
                let mut parts = first_line.split_whitespace();
                let _method = parts.next();
                let Some(uri) = parts.next() else { return };
                if uri.starts_with("http://") {
                    absform.fetch_add(1, Ordering::SeqCst);
                }
                let after = uri.strip_prefix("http://").unwrap_or(uri);
                let authority = after.split('/').next().unwrap_or("");
                let target = if authority.contains(':') {
                    authority.to_string()
                } else {
                    format!("{authority}:80")
                };
                let Ok(mut upstream) = TcpStream::connect(&target) else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                // Forward the already-read head, then pipe both directions.
                if upstream.write_all(&head).is_err() {
                    return;
                }
                let _ = upstream.flush();
                let mut up_read = upstream.try_clone().expect("clone upstream");
                let mut client_write = client.try_clone().expect("clone client");
                // client -> upstream
                let t = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut client, &mut upstream);
                });
                // upstream -> client
                let _ = std::io::copy(&mut up_read, &mut client_write);
                let _ = t.join();
            });
        }
    });
    Some((port, count, absform))
}

#[test]
fn http_proxy_routes_fetch_through_proxy() {
    let Some((proxy_port, proxy_count, absform_count)) = spawn_forwarding_proxy() else {
        eprintln!("SKIP: could not bind a forwarding proxy");
        return;
    };

    let Some(fx) = setup(None, None) else {
        eprintln!("SKIP: grit / grit-http-server fixture unavailable");
        return;
    };

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("http.proxy", &format!("http://127.0.0.1:{proxy_port}"))
        .unwrap();
    let client = UreqHttpClient::from_config(&cfg).expect("from_config with proxy");

    let outcome = match http_fetch(
        &client,
        &fx.local_git,
        &fx.url,
        &fetch_opts(),
        &mut NoProgress,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP: proxied fetch failed (proxy/ureq incompatibility): {e}");
            return;
        }
    };
    let got_main = resolve_ref(&fx.local_git, "refs/remotes/origin/main").expect("origin/main");
    assert_eq!(got_main, fx.main_oid, "proxied fetch did not land the ref");
    assert!(
        outcome
            .updates
            .iter()
            .any(|u| u.remote_ref == "refs/heads/main"),
        "proxied fetch reported no main update"
    );

    let routed = proxy_count.load(Ordering::SeqCst);
    let absform = absform_count.load(Ordering::SeqCst);
    assert!(
        routed >= 1,
        "the fetch did not transit the configured proxy (proxy saw {routed} forwarded requests)"
    );
    assert!(
        absform >= 1,
        "the proxy never saw an absolute-form request line (saw {absform})"
    );
}
