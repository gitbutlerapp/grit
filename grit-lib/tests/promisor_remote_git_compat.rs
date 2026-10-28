//! The `promisor-remote` exchange between a grit client and `git upload-pack`, set up the way
//! Git's t5710 does: the server keeps only small blobs and names a large-object promisor
//! (LOP) that holds the big one. Both are served by `git daemon`.
//!
//! Needs Git 2.49 or newer (the release that added the capability); older `git` prints SKIP.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::config::ConfigSet;
use grit_lib::objects::promisor::DefaultPromisorFetcher;
use grit_lib::objects::promisor_remote::PromisorRejection;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone, CloneOptions, CloneOutcome};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::transport::http::HttpClient;
use grit_lib::remote::{HttpClientFactory, RemoteError, RemoteResult};
use grit_test_support::{git_at_least, git_cmd};

fn git(dir: &Path, args: &[&str]) -> String {
    git_cmd(args)
        .in_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .suc()
        .stdout
}

fn skip_old_git() -> bool {
    if git_at_least(2, 49) {
        return false;
    }
    eprintln!("SKIP: git older than 2.49 has no promisor-remote capability");
    true
}

/// Every remote here is `git://`, so no HTTP client is ever made.
struct NoHttp;

impl HttpClientFactory for NoHttp {
    fn create(&self, _: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Err(RemoteError::UnsupportedScheme)
    }

    fn create_for_probe(&self, _: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Err(RemoteError::UnsupportedScheme)
    }
}

fn fetcher() -> Option<Arc<dyn grit_lib::objects::promisor::PromisorFetcher>> {
    Some(Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp))))
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Serve every repository under `base` with `git daemon`, letting its upload-pack lazily
/// fetch from its own promisor remotes.
fn daemon(base: &Path, port: u16) -> Daemon {
    let child = Command::new("git")
        .args([
            "daemon",
            "--listen=127.0.0.1",
            "--reuseaddr",
            "--export-all",
        ])
        .arg(format!("--port={port}"))
        .arg(format!("--base-path={}", base.display()))
        .arg(base)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_NO_LAZY_FETCH", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        assert!(Instant::now() < deadline, "git daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

struct Lop {
    root: tempfile::TempDir,
    server: PathBuf,
    big: String,
    lop_url: String,
    server_url: String,
    _daemon: Daemon,
}

/// t5710's layout: `server` keeps blobs under 5k (its other objects in a promisor pack) and
/// names `lop`, which holds the 20 KiB blob, as a promisor remote it advertises.
fn setup() -> Lop {
    let root = tempfile::tempdir().unwrap();
    let base = root.path();
    let template = base.join("template");
    std::fs::create_dir(&template).unwrap();
    git(&template, &["init", "-q", "-b", "main"]);
    std::fs::write(template.join("small"), "small\n").unwrap();
    let big_body: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(template.join("big"), big_body).unwrap();
    git(&template, &["add", "."]);
    git(
        &template,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "one",
        ],
    );
    git(
        base,
        &["clone", "-q", "--bare", "--no-local", "template", "server"],
    );
    git(base, &["init", "-q", "--bare", "lop"]);
    let server = base.join("server");
    let lop = base.join("lop");
    let big = git(&server, &["rev-parse", "HEAD:big"]).trim().to_owned();
    let stored = git(
        &lop,
        &["hash-object", "-w", template.join("big").to_str().unwrap()],
    );
    assert_eq!(stored.trim(), big);
    let filtered = base.join("filtered");
    std::fs::create_dir(&filtered).unwrap();
    git(
        &server,
        &[
            "-c",
            "repack.writebitmaps=false",
            "repack",
            "-q",
            "-a",
            "-d",
            "--filter=blob:limit=5k",
            &format!("--filter-to={}/pack", filtered.display()),
        ],
    );
    for entry in std::fs::read_dir(server.join("objects/pack")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "pack") {
            std::fs::write(path.with_extension("promisor"), "").unwrap();
        }
    }
    let port = free_port();
    let lop_url = format!("git://127.0.0.1:{port}/lop");
    let server_url = format!("git://127.0.0.1:{port}/server");
    git(&server, &["config", "remote.lop.url", &lop_url]);
    git(&server, &["config", "remote.lop.promisor", "true"]);
    git(&server, &["config", "promisor.advertise", "true"]);
    for repo in [&server, &lop] {
        git(repo, &["config", "uploadpack.allowFilter", "true"]);
        git(repo, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    }
    let daemon = daemon(base, port);
    Lop {
        root,
        server,
        big,
        lop_url,
        server_url,
        _daemon: daemon,
    }
}

impl Lop {
    fn server_has_big(&self) -> bool {
        Command::new("git")
            .args(["cat-file", "-e", &self.big])
            .current_dir(&self.server)
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap()
            .success()
    }

    fn clone(&self, dest: &str, config: &[(&str, &str)]) -> CloneOutcome {
        let dest = self.root.path().join(dest);
        let mut opts = CloneOptions::new(self.server_url.clone(), dest);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(5 * 1024));
        opts.config = config
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        opts.promisor_fetcher = fetcher();
        clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone")
    }
}

#[test]
fn accepted_lop_serves_the_big_blob_and_the_server_omits_it() {
    if skip_old_git() {
        return;
    }
    let lop = setup();
    let outcome = lop.clone(
        "client",
        &[
            ("remote.lop.url", &lop.lop_url),
            ("remote.lop.promisor", "true"),
            ("promisor.acceptFromServer", "knownUrl"),
        ],
    );
    let handshake = outcome.promisor.as_ref().expect("server advertised lop");
    assert_eq!(handshake.decision.accepted_names(), ["lop"]);
    let big = std::fs::read(lop.root.path().join("client/big")).unwrap();
    assert_eq!(big.len(), 20_000);
    assert!(
        !lop.server_has_big(),
        "the server omitted the blob and the client fetched it from the LOP"
    );
}

#[test]
fn without_acceptance_the_server_fetches_the_blob_itself() {
    if skip_old_git() {
        return;
    }
    let lop = setup();
    let outcome = lop.clone(
        "client",
        &[
            ("remote.lop.url", &lop.lop_url),
            ("remote.lop.promisor", "true"),
        ],
    );
    assert_eq!(
        outcome.promisor.unwrap().decision.candidates[0].rejection,
        Some(PromisorRejection::PolicyNone)
    );
    assert!(lop.root.path().join("client/big").is_file());
    assert!(
        lop.server_has_big(),
        "git upload-pack backfilled from the LOP"
    );
}

#[test]
fn known_url_rejects_a_different_url() {
    if skip_old_git() {
        return;
    }
    let lop = setup();
    let elsewhere = format!("file://{}", lop.root.path().join("lop").display());
    let outcome = lop.clone(
        "client",
        &[
            ("remote.lop.url", &elsewhere),
            ("remote.lop.promisor", "true"),
            ("promisor.acceptFromServer", "knownUrl"),
        ],
    );
    assert_eq!(
        outcome.promisor.unwrap().decision.candidates[0].rejection,
        Some(PromisorRejection::UrlMismatch {
            configured: elsewhere.clone()
        })
    );
    assert!(lop.server_has_big());
}

#[test]
fn auto_filter_uses_the_filter_git_advertises() {
    // `promisor.sendFields` arrived in Git 2.52.
    if !git_at_least(2, 52) {
        eprintln!("SKIP: git older than 2.52 does not send promisor fields");
        return;
    }
    let lop = setup();
    git(
        &lop.server,
        &["config", "promisor.sendFields", "partialCloneFilter"],
    );
    git(
        &lop.server,
        &["config", "remote.lop.partialCloneFilter", "blob:limit=5k"],
    );
    let dest = lop.root.path().join("client");
    let mut opts = CloneOptions::new(lop.server_url.clone(), dest.clone());
    opts.auto_filter = true;
    opts.config = vec![
        ("remote.lop.url".into(), lop.lop_url.clone()),
        ("remote.lop.promisor".into(), "true".into()),
        ("promisor.acceptFromServer".into(), "knownUrl".into()),
    ];
    opts.promisor_fetcher = fetcher();
    let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    assert_eq!(outcome.filter, Some(ObjectFilterSpec::BlobLimit(5 * 1024)));
    assert_eq!(
        git(&dest, &["config", "remote.origin.partialclonefilter"]).trim(),
        "auto"
    );
    assert!(dest.join("big").is_file());
    assert!(!lop.server_has_big());
}
