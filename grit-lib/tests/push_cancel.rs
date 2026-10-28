//! Cancelling a push: a local (`file://`) push and a smart-HTTP push stop on the
//! caller's [`Cancel`] while the pack is built, streamed or waited on, leave the
//! remote without new refs or stray files, and a retry still lands the push.
//!
//! Repositories are built with the library only; the smart-HTTP server is an
//! in-process [`HttpClient`] over [`receive_pack`].

use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::{Error, Result};
use grit_lib::objects::{parse_commit, serialize_tree, ObjectId, ObjectKind, TreeEntry};
use grit_lib::progress::{Cancel, FlagCancel, NeverCancel};
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::pkt_line;
use grit_lib::remote::push::push_http;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::serve::{receive_pack, ReceivePolicy, ServeOptions};
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_lib::remote::transport::http::HttpClient;
use grit_lib::remote::Remote;
use grit_lib::repo::{InitOptions, Repository};

/// Deterministic incompressible bytes, so blobs keep the pack large.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn init_bare(dir: &Path) -> Repository {
    Repository::init(dir, &InitOptions::new().with_bare(true)).expect("init");
    Repository::open(dir, &RepositoryOptions::empty()).expect("open")
}

/// A chain of `commits` commits over `files` files of `blob_len` bytes; each
/// commit rewrites one file, half of it shared with the previous version so the
/// delta search has pairs to try. Returns the tip.
fn build_history(repo: &Repository, commits: usize, files: usize, blob_len: usize) -> ObjectId {
    let odb = repo.odb();
    let mut contents: Vec<Vec<u8>> = (0..files).map(|f| noise(f as u64, blob_len)).collect();
    let mut parent: Option<ObjectId> = None;
    for c in 0..commits {
        let f = c % files;
        let half = blob_len / 2;
        contents[f].splice(half.., noise((c + files) as u64, blob_len - half));
        let entries: Vec<TreeEntry> = contents
            .iter()
            .enumerate()
            .map(|(i, data)| TreeEntry {
                mode: 0o100644,
                name: format!("f{i:04}").into_bytes(),
                oid: odb.write(ObjectKind::Blob, data).expect("blob"),
            })
            .collect();
        let tree = odb
            .write(ObjectKind::Tree, &serialize_tree(&entries))
            .expect("tree");
        let mut body = format!("tree {tree}\n");
        if let Some(p) = parent {
            body.push_str(&format!("parent {p}\n"));
        }
        let when = 1_700_000_000 + c;
        body.push_str(&format!(
            "author A <a@example.com> {when} +0000\ncommitter A <a@example.com> {when} +0000\n\ncommit {c}\n"
        ));
        parent = Some(
            odb.write(ObjectKind::Commit, body.as_bytes())
                .expect("commit"),
        );
    }
    parent.expect("at least one commit")
}

/// Every file under `objects/`, relative to it.
fn object_files(git_dir: &Path) -> BTreeSet<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.insert(rel.to_path_buf());
            }
        }
    }
    let root = git_dir.join("objects");
    let mut out = BTreeSet::new();
    walk(&root, &root, &mut out);
    out
}

fn main_spec(tip: ObjectId) -> PushRefSpec {
    PushRefSpec {
        src: Some(tip),
        dst: "refs/heads/main".to_owned(),
        ..Default::default()
    }
}

fn remote_main(git_dir: &Path) -> Option<ObjectId> {
    resolve_ref(git_dir, "refs/heads/main").ok()
}

/// Fires on its `limit`-th check (counting from zero) and stays fired. Not backed
/// by a shared flag, so worker threads only see it through the calling thread.
struct CancelAfter {
    checks: AtomicUsize,
    limit: usize,
}

impl CancelAfter {
    fn new(limit: usize) -> Self {
        Self {
            checks: AtomicUsize::new(0),
            limit,
        }
    }

    fn checks(&self) -> usize {
        self.checks.load(Ordering::Relaxed)
    }
}

impl Cancel for CancelAfter {
    fn is_cancelled(&self) -> bool {
        self.checks.fetch_add(1, Ordering::Relaxed) >= self.limit
    }
}

fn push_file(
    local: &Repository,
    remote_dir: &Path,
    tip: ObjectId,
    cancel: &dyn Cancel,
) -> std::result::Result<Vec<PushRefStatus>, grit_lib::remote::RemoteError> {
    let remote = Remote::from_url(&format!("file://{}", remote_dir.display())).expect("url");
    remote
        .push(
            local,
            &[main_spec(tip)],
            PushOptions::default(),
            &mut NoProgress,
            cancel,
            None,
        )
        .map(|outcome| outcome.results.into_iter().map(|r| r.status).collect())
}

#[test]
fn local_push_with_a_fired_cancel_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = init_bare(&tmp.path().join("local.git"));
    let tip = build_history(&local, 3, 2, 64);
    let remote_dir = tmp.path().join("remote.git");
    init_bare(&remote_dir);
    let before = object_files(&remote_dir);

    let cancel = FlagCancel::new();
    cancel.cancel();
    let err = push_file(&local, &remote_dir, tip, &cancel).expect_err("cancelled");
    assert!(err.is_cancelled(), "{err:?}");
    assert_eq!(remote_main(&remote_dir), None);
    assert_eq!(object_files(&remote_dir), before);
}

#[cfg(unix)]
#[test]
fn local_push_publishes_a_pack_as_readable_as_its_index() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let local = init_bare(&tmp.path().join("local.git"));
    let tip = build_history(&local, 5, 3, 256);
    let remote_dir = tmp.path().join("remote.git");
    init_bare(&remote_dir);
    assert_eq!(
        push_file(&local, &remote_dir, tip, &NeverCancel).expect("push"),
        vec![PushRefStatus::Ok]
    );

    let files = object_files(&remote_dir);
    let mode = |ext: &str| {
        let rel = files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == ext))
            .unwrap_or_else(|| panic!("no .{ext} in {files:?}"));
        let meta = std::fs::metadata(remote_dir.join("objects").join(rel)).expect("metadata");
        meta.permissions().mode() & 0o777
    };
    assert_eq!(mode("pack"), mode("idx"), "{files:?}");
    if files
        .iter()
        .any(|p| p.extension().is_some_and(|e| e == "rev"))
    {
        assert_eq!(mode("pack"), mode("rev"), "{files:?}");
    }
    assert!(
        files.len() >= 2 && files.len() <= 3,
        "pack, index, and optional rev sidecar: {files:?}"
    );
}

#[test]
fn local_push_stops_wherever_the_cancel_lands_and_a_retry_succeeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = init_bare(&tmp.path().join("local.git"));
    let tip = build_history(&local, 120, 12, 2048);

    // A full push into a scratch remote counts the checks it makes: the walk,
    // the pack build and the indexing each check many times.
    let probe_dir = tmp.path().join("probe.git");
    init_bare(&probe_dir);
    let counting = CancelAfter::new(usize::MAX);
    assert_eq!(
        push_file(&local, &probe_dir, tip, &counting).expect("uncancelled push"),
        vec![PushRefStatus::Ok]
    );
    let total = counting.checks();
    let (commits, trees, blobs) = (120, 120, 12 + 119);
    assert!(
        total >= commits + trees + (commits + trees + blobs),
        "the walk checks once per commit and tree it reads, the pack writer once per \
         object, got {total} checks"
    );

    let remote_dir = tmp.path().join("remote.git");
    init_bare(&remote_dir);
    let before = object_files(&remote_dir);
    for limit in [1, total / 8, total / 4, total / 2] {
        let cancel = CancelAfter::new(limit);
        let err = push_file(&local, &remote_dir, tip, &cancel).expect_err("cancelled");
        assert!(err.is_cancelled(), "cancel at check {limit}: {err:?}");
        assert_eq!(remote_main(&remote_dir), None, "cancel at check {limit}");
        assert_eq!(
            object_files(&remote_dir),
            before,
            "cancel at check {limit} left files behind"
        );
    }

    assert_eq!(
        push_file(&local, &remote_dir, tip, &NeverCancel).expect("retry"),
        vec![PushRefStatus::Ok]
    );
    assert_eq!(remote_main(&remote_dir), Some(tip));
    assert_eq!(history_len(&remote_dir, tip), 120);
}

/// Commits on the first-parent chain from `tip`, every tree and blob of each read
/// from the repository at `git_dir`.
fn history_len(git_dir: &Path, tip: ObjectId) -> usize {
    let repo = Repository::open(git_dir, &RepositoryOptions::empty()).expect("open");
    let odb = repo.odb();
    let mut next = Some(tip);
    let mut len = 0;
    while let Some(oid) = next {
        let commit = parse_commit(&odb.read(&oid).expect("commit").data).expect("parse");
        let tree = odb.read(&commit.tree).expect("tree");
        for entry in grit_lib::objects::parse_tree(&tree.data).expect("parse tree") {
            odb.read(&entry.oid).expect("blob");
        }
        next = commit.parents.first().copied();
        len += 1;
    }
    len
}

#[test]
fn local_push_returns_promptly_when_cancelled_from_another_thread() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = init_bare(&tmp.path().join("local.git"));
    let tip = build_history(&local, 300, 20, 4096);
    let remote_dir = tmp.path().join("remote.git");
    init_bare(&remote_dir);

    let cancel = FlagCancel::new();
    let flip = cancel.clone();
    let flipper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        flip.cancel();
        Instant::now()
    });
    let err = push_file(&local, &remote_dir, tip, &cancel).expect_err("cancelled mid-push");
    let returned = Instant::now();
    let flipped = flipper.join().expect("flipper");
    assert!(err.is_cancelled(), "{err:?}");
    assert!(
        returned.saturating_duration_since(flipped) < Duration::from_secs(2),
        "push kept running {:?} after the cancel",
        returned.saturating_duration_since(flipped)
    );
    assert_eq!(remote_main(&remote_dir), None);
}

/// A smart-HTTP receive-pack endpoint served in-process.
struct InProcessServer {
    repo_dir: PathBuf,
    /// Pause between reads of a streamed body, like a server indexing as it reads.
    read_pause: Duration,
    /// Streamed body bytes read so far.
    body_read: Arc<AtomicUsize>,
    /// When set, a complete request waits here before it is served.
    hold: Mutex<Option<mpsc::Receiver<()>>>,
    /// Set once a request is waiting at `hold`.
    held: Arc<AtomicBool>,
    /// How each streamed body ended: its length, or the read error.
    body_end: Mutex<Option<mpsc::Sender<std::io::Result<usize>>>>,
}

impl InProcessServer {
    fn new(repo_dir: &Path) -> Self {
        Self {
            repo_dir: repo_dir.to_path_buf(),
            read_pause: Duration::ZERO,
            body_read: Arc::new(AtomicUsize::new(0)),
            hold: Mutex::new(None),
            held: Arc::new(AtomicBool::new(false)),
            body_end: Mutex::new(None),
        }
    }

    fn repo(&self) -> Result<Repository> {
        Repository::open(&self.repo_dir, &RepositoryOptions::empty())
    }

    fn serve(&self, body: &[u8]) -> Result<Vec<u8>> {
        let probe = body == b"0000";
        let hold = (!probe)
            .then(|| self.hold.lock().unwrap_or_else(|e| e.into_inner()).take())
            .flatten();
        if let Some(hold) = hold {
            self.held.store(true, Ordering::SeqCst);
            let _ = hold.recv();
        }
        let mut out = Vec::new();
        let opts = ServeOptions {
            stateless_rpc: true,
            ..Default::default()
        };
        receive_pack(
            &self.repo()?,
            &mut Cursor::new(body),
            &mut out,
            &opts,
            &ReceivePolicy::from_config(&ConfigSet::new()),
        )
        .map_err(|e| Error::Message(e.to_string()))?;
        Ok(out)
    }
}

impl HttpClient for InProcessServer {
    fn get(&self, _url: &str, _git_protocol: Option<&str>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        pkt_line::write_line_to_vec(&mut out, "# service=git-receive-pack")?;
        out.extend_from_slice(b"0000");
        let opts = ServeOptions {
            stateless_rpc: true,
            advertise_refs: true,
            ..Default::default()
        };
        receive_pack(
            &self.repo()?,
            &mut std::io::empty(),
            &mut out,
            &opts,
            &ReceivePolicy::from_config(&ConfigSet::new()),
        )
        .map_err(|e| Error::Message(e.to_string()))?;
        Ok(out)
    }

    fn post(
        &self,
        _url: &str,
        _content_type: &str,
        _accept: &str,
        body: &[u8],
        _git_protocol: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.serve(body)
    }

    fn post_streaming(
        &self,
        _url: &str,
        _content_type: &str,
        _accept: &str,
        body: &mut dyn Read,
        _git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        let mut got = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        let end = loop {
            match body.read(&mut buf) {
                Ok(0) => break Ok(got.len()),
                Ok(n) => {
                    got.extend_from_slice(&buf[..n]);
                    self.body_read.fetch_add(n, Ordering::SeqCst);
                    std::thread::sleep(self.read_pause);
                }
                Err(e) => break Err(e),
            }
        };
        let failed = end.is_err();
        if let Some(tx) = self
            .body_end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(end);
        }
        if failed {
            return Err(Error::Message("request body did not finish".to_owned()));
        }
        Ok(Box::new(Cursor::new(self.serve(&got)?)))
    }
}

/// A local repo whose push to an empty remote streams a body of several MiB.
fn http_fixture(tmp: &Path) -> (PathBuf, ObjectId, PathBuf) {
    let local_dir = tmp.join("local.git");
    let local = init_bare(&local_dir);
    let tip = build_history(&local, 40, 8, 256 * 1024);
    let remote_dir = tmp.join("remote.git");
    init_bare(&remote_dir);
    (local_dir, tip, remote_dir)
}

fn push_over_http(
    server: Arc<InProcessServer>,
    local_dir: &Path,
    tip: ObjectId,
    cancel: &dyn Cancel,
) -> Result<Vec<PushRefStatus>> {
    push_http(
        server,
        local_dir,
        "http://in-process.invalid/remote.git",
        &[main_spec(tip)],
        &PushOptions::default(),
        &mut NoProgress,
        cancel,
    )
    .map(|outcome| outcome.results.into_iter().map(|r| r.status).collect())
}

#[test]
fn http_push_cancelled_mid_body_abandons_the_request_and_a_retry_lands() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (local_dir, tip, remote_dir) = http_fixture(tmp.path());
    let (end_tx, end_rx) = mpsc::channel();
    let mut server = InProcessServer::new(&remote_dir);
    server.read_pause = Duration::from_millis(20);
    server.body_end = Mutex::new(Some(end_tx));
    let body_read = Arc::clone(&server.body_read);
    let server = Arc::new(server);

    let cancel = FlagCancel::new();
    let flip = cancel.clone();
    let flipper = std::thread::spawn(move || {
        while body_read.load(Ordering::SeqCst) < 512 * 1024 {
            std::thread::sleep(Duration::from_millis(2));
        }
        flip.cancel();
        Instant::now()
    });
    let err = push_over_http(Arc::clone(&server), &local_dir, tip, &cancel).expect_err("cancelled");
    let returned = Instant::now();
    let flipped = flipper.join().expect("flipper");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(
        returned.saturating_duration_since(flipped) < Duration::from_secs(2),
        "push kept running {:?} after the cancel",
        returned.saturating_duration_since(flipped)
    );
    let end = end_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the server saw the body end");
    assert!(
        end.is_err(),
        "a cancelled body must not look complete: {end:?}"
    );
    assert_eq!(remote_main(&remote_dir), None);

    let retry = Arc::new(InProcessServer::new(&remote_dir));
    assert_eq!(
        push_over_http(retry, &local_dir, tip, &NeverCancel).expect("retry"),
        vec![PushRefStatus::Ok]
    );
    assert_eq!(remote_main(&remote_dir), Some(tip));
}

#[test]
fn http_push_cancelled_while_the_server_is_silent_returns_at_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (local_dir, tip, remote_dir) = http_fixture(tmp.path());
    let (release, hold) = mpsc::channel();
    let server = InProcessServer::new(&remote_dir);
    *server.hold.lock().unwrap() = Some(hold);
    let held = Arc::clone(&server.held);
    let server = Arc::new(server);

    let cancel = FlagCancel::new();
    let flip = cancel.clone();
    let flipper = std::thread::spawn(move || {
        while !held.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        flip.cancel();
        Instant::now()
    });
    let err = push_over_http(Arc::clone(&server), &local_dir, tip, &cancel).expect_err("cancelled");
    let returned = Instant::now();
    let flipped = flipper.join().expect("flipper");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(
        returned.saturating_duration_since(flipped) < Duration::from_secs(1),
        "push waited {:?} for a silent server after the cancel",
        returned.saturating_duration_since(flipped)
    );
    // The whole request was sent, so the server may still apply it once it
    // answers; the cancelled push just stopped waiting for that answer.
    drop(release);
}
