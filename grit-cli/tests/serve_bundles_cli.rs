//! Resumable transfers against a running `grit serve`: clones start from the
//! server's prebuilt bundle (advertised with `bundle-uri`) and resume a
//! partial download of it, and large pushes go to the `pushBundle` endpoint
//! as a bundle uploaded over TUS, after which the server rebuilds its bundle.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::progress::NeverCancel;
use grit_lib::remote::bundle_uri::bundle_cache_path;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::resumable::{download_resumable, ResumableError};
use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
use grit_lib::remote::transport::http::{HttpClient, HttpRequest, HttpResponse};
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

fn ok(home: &Path, dir: &Path, args: &[&str]) -> Output {
    let out = grit(home, dir, args).output().expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn json(home: &Path, dir: &Path, args: &[&str]) -> Value {
    serde_json::from_slice(&ok(home, dir, args).stdout).expect("JSON output")
}

fn with_input(home: &Path, dir: &Path, args: &[&str], input: &str) -> Output {
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

fn head_of(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git rev-parse");
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
            "--bundle-interval",
            "0s",
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

/// The server's current bundle manifest for `repo`, once there is one that
/// differs from `previous`.
fn wait_for_bundle(repo: &Path, previous: Option<&Value>) -> Value {
    let manifest = repo.join(".git/grit-serve/bundles/current.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(bytes) = std::fs::read(&manifest) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if Some(&value) != previous {
                    return value;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no new bundle for {}",
            repo.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Bytes that do not compress, so the bundle has some size.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xff) as u8
        })
        .collect()
}

/// A server repository `demo` with an admin and a stored token for it.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repos: PathBuf,
    demo: PathBuf,
    token: String,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let repos = tmp.path().join("repos");
    let demo = repos.join("demo");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(&demo).expect("mkdir demo");
    ok(&home, &demo, &["init"]);
    ok(
        &home,
        &demo,
        &["config", "receive.denyCurrentBranch", "ignore"],
    );
    std::fs::write(demo.join("big.bin"), noise(600_000, 1)).expect("write blob");
    ok(&home, &demo, &["add", "big.bin"]);
    ok(&home, &demo, &["commit", "-m", "Add a big file"]);

    let setup = with_input(
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
        "admin password\n",
    );
    assert!(setup.status.success(), "{setup:?}");
    let made = json(
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
    );
    let token = made["token"].as_str().expect("token").to_owned();
    let helper = format!("store --file={}", home.join("credentials").display());
    ok(
        &home,
        &home,
        &["config", "--global", "credential.helper", &helper],
    );
    Fixture {
        _tmp: tmp,
        home,
        repos,
        demo,
        token,
    }
}

/// A client whose connection drops for good once `budget` bytes came down.
struct Cut {
    inner: UreqHttpClient,
    token: String,
    budget: Arc<AtomicU64>,
}

impl HttpClient for Cut {
    fn get(&self, url: &str, gp: Option<&str>) -> grit_lib::error::Result<Vec<u8>> {
        self.inner.get(url, gp)
    }

    fn post(
        &self,
        url: &str,
        ct: &str,
        accept: &str,
        body: &[u8],
        gp: Option<&str>,
    ) -> grit_lib::error::Result<Vec<u8>> {
        self.inner.post(url, ct, accept, body, gp)
    }

    fn send(&self, request: &HttpRequest<'_>) -> grit_lib::error::Result<HttpResponse> {
        let request = request
            .clone()
            .header("Authorization", format!("Bearer {}", self.token));
        let mut response = self.inner.send(&request)?;
        response.body = Box::new(Dropping {
            inner: response.body,
            budget: self.budget.clone(),
        });
        Ok(response)
    }
}

struct Dropping {
    inner: Box<dyn Read + Send>,
    budget: Arc<AtomicU64>,
}

impl Read for Dropping {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.budget.load(Ordering::SeqCst);
        if left == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "dropped",
            ));
        }
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.budget.fetch_sub(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

#[test]
fn clone_with_url_credentials_uses_the_server_bundle() {
    let f = fixture();
    let (_server, url) = serve(&f.home, &f.repos);
    wait_for_bundle(&f.demo, None);
    let url_home = tempfile::tempdir().expect("url clone home");
    let host = url.trim_start_matches("http://");
    let clone_url = format!("http://admin%40example.com:{}@{host}/demo.git", f.token);
    let out = grit(
        url_home.path(),
        url_home.path(),
        &["--json", "clone", &clone_url, "copy"],
    )
    .output()
    .expect("spawn grit clone");
    assert!(
        out.status.success(),
        "grit clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).expect("JSON output");
    assert!(
        value.get("bundle").is_some(),
        "URL credentials should use the server bundle: {value}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("could not use the server's bundle"),
        "bundle auth from URL credentials must not fall back: {stderr}"
    );
}

#[test]
fn clones_start_from_the_server_bundle_and_resume_its_download() {
    let f = fixture();
    let (_server, url) = serve(&f.home, &f.repos);
    ok(&f.home, &f.home, &["login", &url, "--token", &f.token]);
    let manifest = wait_for_bundle(&f.demo, None);
    let file = manifest["file"].as_str().expect("bundle file");
    let size = manifest["size"].as_u64().expect("bundle size");
    assert!(size > 500_000, "{manifest}");

    // A commit the bundle does not have: the clone fetches it on top.
    std::fs::write(f.demo.join("later.txt"), "after the bundle\n").expect("write");
    ok(&f.home, &f.demo, &["add", "later.txt"]);
    ok(&f.home, &f.demo, &["commit", "-m", "After the bundle"]);

    // An earlier clone whose connection dropped partway through the bundle.
    let uri = format!("{url}/demo/bundles/{file}");
    let cache = f.home.join(".cache/grit/bundles");
    std::fs::create_dir_all(&cache).expect("mkdir cache");
    let dropped = Cut {
        inner: UreqHttpClient::new(),
        token: f.token.clone(),
        budget: Arc::new(AtomicU64::new(200_000)),
    };
    let dest = bundle_cache_path(&cache, &uri);
    let first = download_resumable(&dropped, &uri, &dest, &mut NoProgress, &NeverCancel);
    assert!(
        matches!(first, Err(ResumableError::Exhausted { .. })),
        "{first:?}"
    );
    let part = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().and_then(|n| n.to_str()).expect("name")
    ));
    assert_eq!(
        std::fs::metadata(&part).map(|m| m.len()).ok(),
        Some(200_000)
    );

    let clone = json(
        &f.home,
        &f.home,
        &["--json", "clone", &format!("{url}/demo.git"), "copy"],
    );
    assert_eq!(
        clone["bundle"]["uri"],
        Value::String(uri.clone()),
        "{clone}"
    );
    assert_eq!(clone["bundle"]["bytes"], size, "{clone}");
    assert_eq!(clone["bundle"]["resumed_from"], 200_000, "{clone}");
    let copy = f.home.join("copy");
    assert_eq!(head_of(&copy), head_of(&f.demo));
    assert_eq!(
        std::fs::read_to_string(copy.join("later.txt"))
            .ok()
            .as_deref(),
        Some("after the bundle\n")
    );
    assert!(!copy.join(".git/refs/bundles").exists());
    assert!(
        !part.exists() && !dest.exists(),
        "the download is cleaned up"
    );

    // Without a partial download the whole bundle comes down.
    let fresh = json(
        &f.home,
        &f.home,
        &["--json", "clone", &format!("{url}/demo.git"), "fresh"],
    );
    assert_eq!(fresh["bundle"]["resumed_from"], 0, "{fresh}");

    // Shallow clones never use the bundle.
    let shallow = json(
        &f.home,
        &f.home,
        &[
            "--json",
            "clone",
            "--depth",
            "1",
            &format!("{url}/demo.git"),
            "shallow",
        ],
    );
    assert!(shallow.get("bundle").is_none(), "{shallow}");

    // `transfer.bundleURI=false` turns it off.
    ok(
        &f.home,
        &f.home,
        &["config", "--global", "transfer.bundleURI", "false"],
    );
    let off = json(
        &f.home,
        &f.home,
        &["--json", "clone", &format!("{url}/demo.git"), "off"],
    );
    assert!(off.get("bundle").is_none(), "{off}");
}

#[test]
fn large_pushes_upload_a_resumable_bundle_and_refresh_the_server_bundle() {
    let f = fixture();
    let (_server, url) = serve(&f.home, &f.repos);
    ok(&f.home, &f.home, &["login", &url, "--token", &f.token]);
    let before = wait_for_bundle(&f.demo, None);

    ok(
        &f.home,
        &f.home,
        &["clone", &format!("{url}/demo.git"), "work"],
    );
    let work = f.home.join("work");
    std::fs::write(work.join("more.bin"), noise(400_000, 2)).expect("write");
    ok(&f.home, &work, &["add", "more.bin"]);
    ok(&f.home, &work, &["commit", "-m", "Add more"]);
    // Anything over 64 KiB goes through the resumable endpoint.
    ok(&f.home, &work, &["config", "push.bundleThreshold", "64k"]);

    let pushed = json(&f.home, &work, &["--json", "push"]);
    assert_eq!(pushed["rejected"], Value::Bool(false), "{pushed}");
    assert_eq!(pushed["results"][0]["status"], "ok", "{pushed}");
    assert_eq!(head_of(&f.demo), head_of(&work));
    let uploads = f.demo.join(".git/grit-serve/uploads");
    let left: Vec<_> = std::fs::read_dir(&uploads)
        .expect("uploads dir")
        .flatten()
        .collect();
    assert!(
        left.is_empty(),
        "uploads are removed once applied: {left:?}"
    );
    assert!(
        !work.join(".git/grit/push-bundles").exists()
            || std::fs::read_dir(work.join(".git/grit/push-bundles"))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "the push session is removed once applied"
    );

    // The push changed the refs, so the server bundles them again.
    let after = wait_for_bundle(&f.demo, Some(&before));
    assert_ne!(after["refs_digest"], before["refs_digest"]);

    // A small push still uses receive-pack.
    std::fs::write(work.join("small.txt"), "small\n").expect("write");
    ok(&f.home, &work, &["add", "small.txt"]);
    ok(&f.home, &work, &["commit", "-m", "Small"]);
    ok(&f.home, &work, &["config", "push.bundleThreshold", "16m"]);
    ok(&f.home, &work, &["push"]);
    assert_eq!(head_of(&f.demo), head_of(&work));
    let sessions = work.join(".git/grit/push-bundles");
    assert!(
        std::fs::read_dir(&sessions)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "a pack under the threshold is spooled and removed, not kept as a session"
    );
    let fsck = Command::new("git")
        .args(["fsck", "--strict", "--no-dangling"])
        .current_dir(&f.demo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
