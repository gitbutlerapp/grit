//! Protocol v2 fetch negotiation runs in rounds against a real `git daemon`.
//!
//! The client walks local history lazily: it sends a first batch of `have`s,
//! and only walks further when the server is not yet `ready`. These tests pin
//! that down from the server's side of the wire (`GIT_TRACE_PACKET`) and from
//! the pack the server sends:
//!
//! * a fetch the server can satisfy after one round never reads deep local
//!   history (proved by deleting a deep commit object, which a full walk would
//!   trip over);
//! * when the first round finds nothing in common, later rounds carry the
//!   commits the server acknowledged plus the next batch, and the pack stays
//!   minimal;
//! * remote-tracking refs are offered as negotiation tips, so history the
//!   client already holds under `refs/remotes/` is not downloaded again.
//!
//! Every test skips (returns early) when `git daemon` is unavailable.

use grit_test_support::{free_port, git};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::NeverCancel;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};
use grit_lib::repo::Repository;

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `git daemon` exporting `<tmp>/srv/repo.git`, tracing its pkt-lines.
struct Server {
    tmp: tempfile::TempDir,
    bare: PathBuf,
    trace: PathBuf,
    url: String,
    _daemon: DaemonGuard,
}

impl Server {
    fn start() -> Option<Self> {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("srv");
        std::fs::create_dir_all(&base).expect("srv dir");
        let bare = base.join("repo.git");
        git(
            &base,
            &["init", "-q", "--bare", "-b", "main", bare.to_str()?],
        );
        let port = free_port()?;
        let trace = tmp.path().join("trace_packet.log");
        let child = Command::new("git")
            .arg("daemon")
            .arg("--listen=127.0.0.1")
            .arg(format!("--port={port}"))
            .arg("--reuseaddr")
            .arg("--export-all")
            .arg(format!("--base-path={}", base.display()))
            .arg(&base)
            .env("GIT_TRACE_PACKET", &trace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let daemon = DaemonGuard(child);
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(Self {
            tmp,
            bare,
            trace,
            url: format!("git://127.0.0.1:{port}/repo.git"),
            _daemon: daemon,
        })
    }

    /// Read the trace once the upload-pack child has finished writing it.
    fn settled_trace(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut last = String::new();
        loop {
            let now = std::fs::read_to_string(&self.trace).unwrap_or_default();
            if (!now.is_empty() && now == last) || Instant::now() >= deadline {
                return now;
            }
            last = now;
            std::thread::sleep(Duration::from_millis(80));
        }
    }
}

/// A local (non-bare) repository created by system git, written to by grit.
fn local_repo(root: &Path) -> (PathBuf, Repository) {
    std::fs::create_dir_all(root).expect("local dir");
    git(root, &["init", "-q", "-b", "main", "."]);
    let git_dir = root.join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open local");
    (git_dir, repo)
}

/// Write `count` empty-tree commits on top of `parent` as loose objects, with
/// strictly increasing committer dates starting at `start`. Returns them
/// oldest first.
fn write_chain(
    repo: &Repository,
    parent: Option<ObjectId>,
    count: usize,
    label: &str,
    start: i64,
) -> Vec<ObjectId> {
    let tree = repo.odb().write(ObjectKind::Tree, &[]).expect("empty tree");
    let mut parent = parent;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let when = start + i as i64;
        let parent_line = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
        let body = format!(
            "tree {tree}\n{parent_line}\
             author T <t@example.com> {when} +0000\n\
             committer T <t@example.com> {when} +0000\n\
             \n\
             {label} {i}\n"
        );
        let oid = repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("write commit");
        out.push(oid);
        parent = Some(oid);
    }
    out
}

fn set_ref(git_dir: &Path, name: &str, oid: ObjectId) {
    git(
        git_dir,
        &["--git-dir", ".", "update-ref", name, &oid.to_hex()],
    );
}

/// Push `oid` from the local repository to the server's `refs/heads/main`.
fn push_to_server(local_root: &Path, srv: &Server, oid: ObjectId) {
    git(
        local_root,
        &[
            "push",
            "-q",
            srv.bare.to_str().expect("utf8 path"),
            &format!("{}:refs/heads/main", oid.to_hex()),
        ],
    );
}

/// Add one empty commit on top of the server's `main` and return it.
fn server_commit(srv: &Server) -> ObjectId {
    let tree = git(&srv.bare, &["rev-parse", "main^{tree}"]);
    let out = git(
        &srv.bare,
        &["commit-tree", "-p", "main", "-m", "server tip", tree.trim()],
    );
    let oid = ObjectId::from_hex(out.trim()).expect("oid");
    git(&srv.bare, &["update-ref", "refs/heads/main", &oid.to_hex()]);
    oid
}

fn fetch_main(git_dir: &Path, srv: &Server) -> Result<(), grit_lib::error::Error> {
    let transport = GitDaemonTransport::new();
    let connect = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = transport.connect(&srv.url, Service::UploadPack, &connect)?;
    assert_eq!(conn.protocol_version(), 2, "expected protocol v2");
    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()];
        o.tags = TagMode::None;
        o
    };
    fetch_remote(
        git_dir,
        &mut *conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )?;
    Ok(())
}

/// Client `have` lines the server read, one entry per line, in wire order.
fn traced_haves(trace: &str) -> Vec<String> {
    trace
        .lines()
        .filter_map(|l| l.split_once("upload-pack< have ").map(|(_, h)| h.trim()))
        .map(str::to_owned)
        .collect()
}

fn traced_fetch_commands(trace: &str) -> usize {
    trace
        .lines()
        .filter(|l| l.contains("upload-pack< command=fetch"))
        .count()
}

/// Total objects in the packs under `git_dir/objects/pack`, from their headers.
fn packed_object_count(git_dir: &Path) -> u32 {
    let mut total = 0;
    for entry in std::fs::read_dir(git_dir.join("objects/pack")).expect("pack dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("pack") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read pack");
        assert_eq!(&bytes[..4], b"PACK", "pack magic in {}", path.display());
        total += u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    }
    total
}

fn fsck_clean(root: &Path) {
    let out = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ready_after_first_round_leaves_deep_history_unread() {
    let Some(srv) = Server::start() else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let root = srv.tmp.path().join("local");
    let (git_dir, repo) = local_repo(&root);
    let chain = write_chain(&repo, None, 3000, "c", 1_600_000_000);
    let tip = *chain.last().expect("tip");
    set_ref(&git_dir, "refs/heads/main", tip);
    push_to_server(&root, &srv, tip);
    let new_tip = server_commit(&srv);

    // Remove a commit near the root. Answering the server needs only the
    // newest commits, so a lazy walk never gets there.
    let deep = chain[5].to_hex();
    std::fs::remove_file(git_dir.join("objects").join(&deep[..2]).join(&deep[2..]))
        .expect("remove deep commit");
    drop(repo);

    fetch_main(&git_dir, &srv).expect("fetch must not walk past what the server needs");
    assert_eq!(
        resolve_ref(&git_dir, "refs/remotes/origin/main").expect("tracking ref"),
        new_tip
    );

    let trace = srv.settled_trace();
    assert_eq!(
        traced_fetch_commands(&trace),
        1,
        "server should be ready after the first round:\n{trace}"
    );
    let haves = traced_haves(&trace);
    assert!(
        !haves.is_empty() && haves.len() <= 16,
        "first round sends at most 16 haves, got {}",
        haves.len()
    );
    assert_eq!(haves[0], tip.to_hex(), "newest local commit goes first");
    assert_eq!(packed_object_count(&git_dir), 1, "only the new commit");
}

#[test]
fn later_rounds_resend_acknowledged_commits_and_pack_stays_minimal() {
    let Some(srv) = Server::start() else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let root = srv.tmp.path().join("local");
    let (git_dir, repo) = local_repo(&root);
    let base = write_chain(&repo, None, 20, "base", 1_600_000_000);
    let base_tip = *base.last().expect("base tip");
    set_ref(&git_dir, "refs/heads/main", base_tip);
    push_to_server(&root, &srv, base_tip);
    let new_tip = server_commit(&srv);

    // Thousands of local-only commits on top of the shared base: the first
    // round (16 skipping haves) cannot reach the base, so more rounds follow.
    let local_only = write_chain(&repo, Some(base_tip), 3000, "local", 1_700_000_000);
    let local_tip = *local_only.last().expect("local tip");
    set_ref(&git_dir, "refs/heads/main", local_tip);
    drop(repo);

    fetch_main(&git_dir, &srv).expect("multi-round fetch");
    assert_eq!(
        resolve_ref(&git_dir, "refs/remotes/origin/main").expect("tracking ref"),
        new_tip
    );
    // The skipping walk may land anywhere in the shared base, so the server can
    // resend part of it, but never the full history.
    let packed = packed_object_count(&git_dir) as usize;
    assert!(
        (1..=base.len() + 1).contains(&packed),
        "negotiation found the shared base, yet the server packed {packed} objects"
    );
    fsck_clean(&root);

    let trace = srv.settled_trace();
    let rounds = traced_fetch_commands(&trace);
    assert!(
        rounds >= 2,
        "expected several rounds, got {rounds}:\n{trace}"
    );
    let haves = traced_haves(&trace);
    let distinct: std::collections::HashSet<&String> = haves.iter().collect();
    assert!(
        distinct.len() < local_only.len() + base.len(),
        "negotiation should stop early, offered {} distinct haves",
        distinct.len()
    );
    // Haves the server acknowledged are repeated in every later request
    // (v2 servers keep no state between rounds); unacknowledged ones are not.
    let base_hex: std::collections::HashSet<String> = base.iter().map(ObjectId::to_hex).collect();
    for have in &haves {
        let repeats = haves.iter().filter(|h| *h == have).count();
        if !base_hex.contains(have) {
            assert_eq!(repeats, 1, "unacknowledged have {have} was resent");
        }
    }
}

#[test]
fn remote_tracking_refs_are_negotiation_tips() {
    let Some(srv) = Server::start() else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let root = srv.tmp.path().join("local");
    let (git_dir, repo) = local_repo(&root);
    let base = write_chain(&repo, None, 20, "base", 1_600_000_000);
    let base_tip = *base.last().expect("base tip");
    let ahead = write_chain(&repo, Some(base_tip), 50, "ahead", 1_700_000_000);
    let ahead_tip = *ahead.last().expect("ahead tip");
    // The local branch is behind its remote-tracking ref, as after a plain
    // `fetch` that was never merged.
    set_ref(&git_dir, "refs/heads/main", base_tip);
    set_ref(&git_dir, "refs/remotes/origin/main", ahead_tip);
    push_to_server(&root, &srv, ahead_tip);
    let new_tip = server_commit(&srv);
    drop(repo);

    fetch_main(&git_dir, &srv).expect("fetch");
    assert_eq!(
        resolve_ref(&git_dir, "refs/remotes/origin/main").expect("tracking ref"),
        new_tip
    );
    assert_eq!(
        packed_object_count(&git_dir),
        1,
        "commits already held under refs/remotes/ must not be sent again"
    );
    fsck_clean(&root);
}
