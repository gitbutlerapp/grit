//! End-to-end smart HTTP against `grit-http-server` using only the system `git`
//! client. The server must not rely on a `grit` binary on `PATH` or `GUST_BIN`.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) {
    let status = git_cmd(dir, args).status().expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_cmd(dir, args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_output(dir: &Path, args: &[&str]) -> Output {
    git_cmd(dir, args).output().expect("spawn git")
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = git_output(dir, &["rev-parse", rev]);
    assert!(out.status.success(), "git rev-parse {rev} failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
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

fn require_binary(name: &str) -> PathBuf {
    find_binary(name).unwrap_or_else(|| {
        panic!("{name} binary not found (build grit-http-server with `cargo build -p grit-http-server`)")
    })
}

fn require_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn path_without_cargo_bins() -> String {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|entry| {
            !entry.contains("/target/debug")
                && !entry.contains("/target/release")
                && !entry.ends_with("/.cargo/bin")
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn wait_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(15);
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

fn spawn_server(server_bin: &Path, root: &Path, port: u16, home: Option<&Path>) -> Child {
    let mut cmd = Command::new(server_bin);
    cmd.arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env_remove("GUST_BIN")
        .env("PATH", path_without_cargo_bins());
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not spawn grit-http-server")
}

fn git_expect_failure(dir: &Path, args: &[&str]) {
    let status = git_cmd(dir, args).status().expect("spawn git");
    assert!(!status.success(), "git {args:?} should have failed");
}

#[test]
fn system_git_clone_fetch_v0_v2_and_push_over_grit_http_server() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main", "."]);
    std::fs::write(origin.join("README"), "hello\n").unwrap();
    git(&origin, &["add", "README"]);
    git(&origin, &["commit", "-q", "-m", "initial"]);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        &origin,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.to_str().expect("utf8 path"),
        ],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

    let url = format!("http://127.0.0.1:{port}/project.git");

    let clone_dir = tmp.path().join("clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            url.as_str(),
            clone_dir.to_str().expect("utf8"),
        ],
    );
    assert!(git_ok(&clone_dir, &["fsck"]));

    // New commit on the served bare repo, then explicit protocol v0 fetch.
    std::fs::write(origin.join("v0-fetch.txt"), "v0\n").unwrap();
    git(&origin, &["add", "v0-fetch.txt"]);
    git(&origin, &["commit", "-q", "-m", "for v0 fetch"]);
    git(&origin, &["push", "-q", url.as_str(), "main"]);
    let v0_tip = rev_parse(&bare, "refs/heads/main");
    git(&clone_dir, &["-c", "protocol.version=0", "fetch", "origin"]);
    assert_eq!(
        rev_parse(&clone_dir, "origin/main"),
        v0_tip,
        "protocol v0 fetch must update origin/main to the remote tip"
    );

    // Another remote commit, then explicit protocol v2 fetch.
    std::fs::write(origin.join("v2-fetch.txt"), "v2\n").unwrap();
    git(&origin, &["add", "v2-fetch.txt"]);
    git(&origin, &["commit", "-q", "-m", "for v2 fetch"]);
    git(&origin, &["push", "-q", url.as_str(), "main"]);
    let v2_tip = rev_parse(&bare, "refs/heads/main");
    git(&clone_dir, &["-c", "protocol.version=2", "fetch", "origin"]);
    assert_eq!(
        rev_parse(&clone_dir, "origin/main"),
        v2_tip,
        "protocol v2 fetch must update origin/main to the remote tip"
    );

    std::fs::write(clone_dir.join("branch.txt"), "topic\n").unwrap();
    git(&clone_dir, &["checkout", "-q", "-b", "topic"]);
    git(&clone_dir, &["add", "branch.txt"]);
    git(&clone_dir, &["commit", "-q", "-m", "topic work"]);
    git(&clone_dir, &["push", "-q", "origin", "topic:topic"]);

    assert!(git_ok(&bare, &["fsck"]));
    assert!(git_ok(&clone_dir, &["fsck"]));
}

/// Deterministic incompressible bytes, so the pushed pack is as large as the blob.
fn pseudo_random_bytes(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Push one commit holding a single random blob of each size, then check the
/// served repository with the system `git`.
fn push_large_blobs_over_grit_http_server(sizes: &[usize], extra_push_args: &[&str]) {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().expect("utf8 path")],
    );

    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );
    let url = format!("http://127.0.0.1:{port}/project.git");

    for (i, &size) in sizes.iter().enumerate() {
        std::fs::write(
            work.join("blob"),
            pseudo_random_bytes(size, 0x9e37_79b9 + i as u64),
        )
        .unwrap();
        git(&work, &["add", "blob"]);
        git(
            &work,
            &["commit", "-q", "-m", &format!("blob of {size} bytes")],
        );
        let refspec = format!("HEAD:refs/heads/b{size}");
        let mut args = extra_push_args.to_vec();
        args.extend(["push", "-q", url.as_str(), refspec.as_str()]);
        let out = git_output(&work, &args);
        assert!(
            out.status.success(),
            "push of a {size}-byte blob failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            rev_parse(&bare, &format!("refs/heads/b{size}")),
            rev_parse(&work, "HEAD"),
            "served branch must point at the pushed commit"
        );
    }

    let fsck = git_output(&bare, &["fsck", "--full", "--strict"]);
    assert!(
        fsck.status.success(),
        "git fsck on the served repo failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn push_larger_than_axum_default_body_limit_succeeds() {
    push_large_blobs_over_grit_http_server(&[1_500_000, 3_000_000], &[]);
}

#[test]
fn push_larger_than_fetch_request_cap_succeeds() {
    push_large_blobs_over_grit_http_server(&[40 * 1024 * 1024], &[]);
}

#[test]
fn large_push_with_content_length_body_succeeds() {
    // A post buffer bigger than the pack makes git send one sized body instead
    // of a chunked stream.
    push_large_blobs_over_grit_http_server(&[3_000_000], &["-c", "http.postBuffer=67108864"]);
}

/// POST `body` to `path` with `Content-Encoding: gzip` and return the HTTP status code.
fn post_gzip_status(port: u16, path: &str, body: &[u8]) -> u16 {
    use std::io::{Read, Write};

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("read timeout");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Type: application/x-git-receive-pack-request\r\n\
         Content-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    // The server may answer and close before reading the whole body.
    let _ = stream.write_all(body);
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let status_line = String::from_utf8_lossy(&response);
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status in response: {status_line:?}"))
}

/// A gzip body that decodes to `decoded_len` bytes of repeated, well-formed push
/// commands, so the server keeps parsing until the decoder's expansion limit trips.
fn gzip_command_flood(decoded_len: usize) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    let line = format!("{} {} refs/heads/flood\n", "0".repeat(40), "1".repeat(40));
    let pkt = format!("{:04x}{line}", line.len() + 4);
    let chunk = pkt.repeat(1024 * 1024 / pkt.len());
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    let mut written = 0;
    while written < decoded_len {
        enc.write_all(chunk.as_bytes()).expect("gzip write");
        written += chunk.len();
    }
    enc.finish().expect("gzip finish")
}

#[test]
fn gzip_bomb_push_is_refused_and_server_keeps_serving() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().expect("utf8 path")],
    );

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

    let bomb = gzip_command_flood(64 * 1024 * 1024);
    assert!(
        bomb.len() < 1024 * 1024,
        "test body must be a small request ({} bytes)",
        bomb.len()
    );
    assert_eq!(
        post_gzip_status(port, "/project.git/git-receive-pack", &bomb),
        413,
        "a gzip push body that expands far beyond its size must be refused"
    );

    assert_eq!(
        post_gzip_status(port, "/project.git/git-receive-pack", b"not gzip at all"),
        400,
        "a push body that is not valid gzip is a client error"
    );

    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("a.txt"), "after the bomb\n").unwrap();
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "after"]);
    let url = format!("http://127.0.0.1:{port}/project.git");
    git(&work, &["push", "-q", url.as_str(), "main"]);
    assert_eq!(
        rev_parse(&bare, "refs/heads/main"),
        rev_parse(&work, "HEAD")
    );
    assert!(
        !git_ok(&bare, &["rev-parse", "-q", "--verify", "refs/heads/flood"]),
        "the refused push must not create a ref"
    );
}

#[test]
fn global_receive_deny_non_fast_forward_accepts_fast_forward_rejects_force_push() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    std::fs::write(
        fake_home.join(".gitconfig"),
        "[receive]\n\tdenyNonFastForwards = true\n",
    )
    .unwrap();

    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main", "."]);
    std::fs::write(origin.join("a.txt"), "1\n").unwrap();
    git(&origin, &["add", "a.txt"]);
    git(&origin, &["commit", "-q", "-m", "c1"]);
    std::fs::write(origin.join("b.txt"), "2\n").unwrap();
    git(&origin, &["add", "b.txt"]);
    git(&origin, &["commit", "-q", "-m", "c2"]);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        &origin,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.to_str().expect("utf8 path"),
        ],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let advanced_tip = rev_parse(&bare, "refs/heads/main");

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, Some(&fake_home));
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

    let url = format!("http://127.0.0.1:{port}/project.git");
    let clone_dir = tmp.path().join("clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            url.as_str(),
            clone_dir.to_str().expect("utf8"),
        ],
    );
    std::fs::write(clone_dir.join("c.txt"), "3\n").unwrap();
    git(&clone_dir, &["add", "c.txt"]);
    git(&clone_dir, &["commit", "-q", "-m", "c3"]);
    git(&clone_dir, &["push", "-q", "origin", "main:main"]);
    let fast_forward_tip = rev_parse(&clone_dir, "HEAD");
    assert_eq!(
        rev_parse(&bare, "refs/heads/main"),
        fast_forward_tip,
        "a fast-forward push must be accepted under denyNonFastForwards"
    );
    assert_ne!(fast_forward_tip, advanced_tip);
    assert!(git_ok(&bare, &["fsck", "--strict"]));

    git(&clone_dir, &["reset", "--hard", "HEAD~2"]);
    git_expect_failure(&clone_dir, &["push", "--force", "origin", "main:main"]);
    assert_eq!(
        rev_parse(&bare, "refs/heads/main"),
        fast_forward_tip,
        "non-fast-forward force push must not rewind the served branch"
    );
}

/// Issue #1818: `receive.fsckObjects` in the served repository refuses a
/// malformed commit pushed over smart HTTP, before any ref or object lands.
#[test]
fn receive_fsck_objects_refuses_malformed_commit_over_http() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("f"), "x\n").unwrap();
    git(&work, &["add", "f"]);
    git(&work, &["commit", "-q", "-m", "base"]);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().expect("utf8 path")],
    );
    git(&bare, &["config", "receive.fsckObjects", "true"]);

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );
    let url = format!("http://127.0.0.1:{port}/project.git");
    git(&work, &["push", "-q", url.as_str(), "main"]);

    let body = format!(
        "tree {}\nparent {}\nauthor Bad Guy no-email 1700000000 +0000\ncommitter Bad <b@c> 1700000000 +0000\n\nbad\n",
        rev_parse(&work, "HEAD^{tree}"),
        rev_parse(&work, "HEAD")
    );
    let mut hash = git_cmd(
        &work,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "--stdin",
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .expect("hash-object");
    std::io::Write::write_all(hash.stdin.as_mut().expect("stdin"), body.as_bytes())
        .expect("write commit");
    let out = hash.wait_with_output().expect("hash-object");
    assert!(out.status.success());
    let bad = String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned();

    let push = git_output(
        &work,
        &["push", url.as_str(), &format!("{bad}:refs/heads/bad")],
    );
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(!push.status.success(), "push must be refused: {stderr}");
    assert!(stderr.contains("unpacker error"), "{stderr}");
    assert!(
        !git_ok(&bare, &["rev-parse", "-q", "--verify", "refs/heads/bad"]),
        "the refused push must not create a ref"
    );
    assert!(
        !git_ok(&bare, &["cat-file", "-e", &bad]),
        "the malformed commit must not be stored"
    );
    assert!(git_ok(&bare, &["fsck", "--strict"]));

    std::fs::write(work.join("g"), "y\n").unwrap();
    git(&work, &["add", "g"]);
    git(&work, &["commit", "-q", "-m", "clean"]);
    git(&work, &["push", "-q", url.as_str(), "main"]);
    assert_eq!(
        rev_parse(&bare, "refs/heads/main"),
        rev_parse(&work, "HEAD")
    );
}

/// Issue #2136: a push from a `--depth 1` clone announces its shallow boundary.
/// Over smart HTTP it used to fail with HTTP 500; it must apply like any push.
#[test]
fn push_from_shallow_clone_over_grit_http_server() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main", "."]);
    for i in 0..3 {
        std::fs::write(origin.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        &origin,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.to_str().expect("utf8 path"),
        ],
    );

    let client = tmp.path().join("sh1");
    let file_url = format!("file://{}", origin.display());
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            file_url.as_str(),
            client.to_str().expect("utf8"),
        ],
    );
    git(&client, &["commit", "-q", "--allow-empty", "-m", "x"]);

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );
    let url = format!("http://127.0.0.1:{port}/project.git");
    let out = git_output(&client, &["push", url.as_str(), "HEAD:refs/heads/t2"]);
    assert!(
        out.status.success(),
        "push from a shallow clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        rev_parse(&bare, "refs/heads/t2"),
        rev_parse(&client, "HEAD")
    );
    assert!(!bare.join("shallow").exists());
    assert!(git_ok(&bare, &["fsck", "--strict"]));
}

fn want_pkt_line(oid: &str) -> String {
    let line = format!("want {oid}\n");
    format!("{:04x}{line}", line.len() + 4)
}

fn post_upload_pack_status(port: u16, path: &str, body: &[u8], gzip: bool) -> u16 {
    use std::io::{Read, Write};

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .expect("read timeout");
    let encoding = if gzip {
        "Content-Encoding: gzip\r\n"
    } else {
        ""
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Type: application/x-git-upload-pack-request\r\n\
         {encoding}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(body).expect("write body");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let status_line = String::from_utf8_lossy(&response);
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status in response: {status_line:?}"))
}

#[test]
fn large_fetch_want_body_is_accepted_over_smart_http() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().expect("utf8 path")],
    );
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("a.txt"), "x\n").unwrap();
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(
        &work,
        &["push", "-q", bare.to_str().expect("utf8 path"), "main"],
    );
    let oid = rev_parse(&bare, "refs/heads/main");

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

    let pkt = want_pkt_line(&oid);
    let mut body = String::new();
    while body.len() < 40 * 1024 * 1024 {
        body.push_str(&pkt);
    }
    body.push_str("0000");

    assert_eq!(
        post_upload_pack_status(port, "/project.git/git-upload-pack", body.as_bytes(), false),
        200,
        "a ~40 MiB fetch want list must not hit HTTP 413"
    );
}

#[test]
fn gzip_fetch_want_body_past_32mib_decoded_is_accepted() {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().expect("utf8 path")],
    );
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &work,
        &["push", "-q", bare.to_str().expect("utf8 path"), "main"],
    );
    let oid = rev_parse(&bare, "refs/heads/main");

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

    let pkt = want_pkt_line(&oid);
    let mut plain = String::new();
    while plain.len() < 35 * 1024 * 1024 {
        plain.push_str(&pkt);
    }
    plain.push_str("0000");
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(plain.as_bytes()).expect("gzip write");
    let gzip = enc.finish().expect("gzip finish");

    assert_eq!(
        post_upload_pack_status(port, "/project.git/git-upload-pack", &gzip, true),
        200,
        "gzip-compressed repetitive wants past 32 MiB decoded must succeed"
    );
}
