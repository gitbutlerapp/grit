//! Comprehensive PUSH matrix across the three wire transports
//! (`git://` daemon, `ssh` via fake-ssh, smart-HTTP via `grit-http-server`).
//!
//! Each transport is driven through the *same* matrix of push scenarios so the
//! shared decide/pack/report helpers behind `push::push_remote` (streaming,
//! git/ssh) and `push::push_http` (stateless RPC) are exercised identically
//! regardless of how the bytes reach `git-receive-pack`:
//!
//!   * new ref (create)                       -> `Ok` `[new branch]`
//!   * fast-forward update                    -> `Ok`
//!   * non-fast-forward without force         -> client `RejectNonFastForward`
//!   * forced non-fast-forward                -> `Ok` (forced), ref moves
//!   * deletion                               -> `Ok`, no pack streamed, ref gone
//!   * up-to-date (re-push same tip)          -> `UpToDate` no-op (no wire round)
//!   * force-with-lease (expected_old) ok     -> `Ok`, ref moves
//!   * force-with-lease (expected_old) stale  -> client `RejectStale`, ref intact
//!   * expect-absent lease on existing ref    -> client `RejectStale`, ref intact
//!   * atomic all-or-nothing (one bad ref)    -> whole push aborts, nothing lands
//!   * multi-ref push (two new refs at once)  -> both `Ok`, both land
//!
//! After every mutation the remote ref + objects are cross-checked against
//! system `git` and the bare repo is `git fsck`-ed clean.
//!
//! Two further tests target the report-status layer specifically:
//!
//!   * `remote_rejection_surfaces_*`: a declining `pre-receive` hook makes
//!     `git-receive-pack` send an `ng <ref> <reason>` line, which must surface
//!     as [`PushRefStatus::RemoteRejected`] (the server, not the client gate,
//!     declines) — over both `git://` and `ssh`.
//!   * `report_status_v1_and_v2_both_parse_over_ssh`: a real fake-`receive-pack`
//!     shell script (driven over the SSH transport) replies with an explicit
//!     `report-status` (v1) report in one run and an explicit `report-status-v2`
//!     report (with the v2-only trailing `option` line) in another, proving the
//!     shared report parser folds both wire shapes — including the `ng`
//!     rejection — into the right per-ref status.
//!
//! Real fixtures only: system `git` / `git daemon`, a fake-ssh `GIT_SSH_COMMAND`
//! script, and the `grit-http-server` binary. Every test makes a real assertion;
//! a test SKIPs (returns early) only when its fixture is genuinely unavailable
//! (`git daemon` cannot bind, no POSIX `sh`, the server binary is missing, …),
//! and the happy path is otherwise real end-to-end wire I/O.
//!
//! Gated on `http-ureq` (the default `UreqHttpClient` lives there):
//!   cargo test -p grit-lib --features http-ureq --test matrix_push

#![cfg(feature = "http-ureq")]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::fetch::NoProgress;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::push::{push_http, push_remote};
use grit_lib::push_report::{PushRefResult, PushRefStatus};
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{PushOptions, PushOutcome, PushRefSpec};
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::{HttpClient, SmartHttpTransport};
#[cfg(unix)]
use grit_lib::transport::SshTransport;
use grit_lib::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};

// ---------------------------------------------------------------------------
// Shared fixture helpers (copied from the sibling transport_*.rs tests so the
// daemon/ssh/http bring-up is identical and not reinvented).
// ---------------------------------------------------------------------------

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

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// Build a source repo with a small commit graph used by the matrix:
///   * `main`:  c1 -> c2          (the branch we push and fast-forward)
///   * `feature`: c1 -> c2 -> c3  (a fast-forward of main, for the FF case)
///   * `side`:  c1 -> s1          (a divergent sibling of c2, for non-ff/force)
///   * `extra`: c1 -> e1          (a second new ref, for the multi-ref case)
///
/// Returns the resolved oids the tests need.
struct Graph {
    c1: ObjectId,
    c2: ObjectId,
    ff: ObjectId,
    side: ObjectId,
    extra: ObjectId,
}

fn build_source(dir: &Path) -> Graph {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(dir, "HEAD");

    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    let c2 = rev_parse(dir, "HEAD");

    // A fast-forward of c2 (used to advance main).
    std::fs::write(dir.join("c.txt"), "three\n").unwrap();
    git(dir, &["add", "c.txt"]);
    git(dir, &["commit", "-q", "-m", "c3 (ff of c2)"]);
    let ff = rev_parse(dir, "HEAD");

    // A divergent sibling of c2 (branch off c1): never a descendant of c2/ff.
    git(dir, &["checkout", "-q", "-b", "side", c1.to_hex().as_str()]);
    std::fs::write(dir.join("s.txt"), "side\n").unwrap();
    git(dir, &["add", "s.txt"]);
    git(dir, &["commit", "-q", "-m", "s1 (divergent)"]);
    let side = rev_parse(dir, "HEAD");

    // A second independent new tip for the multi-ref push.
    git(
        dir,
        &["checkout", "-q", "-b", "extra", c1.to_hex().as_str()],
    );
    std::fs::write(dir.join("x.txt"), "extra\n").unwrap();
    git(dir, &["add", "x.txt"]);
    git(dir, &["commit", "-q", "-m", "e1"]);
    let extra = rev_parse(dir, "HEAD");

    // Leave the repo checked out on main at c2 for cleanliness.
    git(dir, &["checkout", "-q", "main"]);

    Graph {
        c1,
        c2,
        ff,
        side,
        extra,
    }
}

fn spec(src: Option<ObjectId>, dst: &str) -> PushRefSpec {
    PushRefSpec {
        src,
        dst: dst.to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    }
}

fn forced(src: ObjectId, dst: &str) -> PushRefSpec {
    PushRefSpec {
        force: true,
        ..spec(Some(src), dst)
    }
}

fn deletion(dst: &str) -> PushRefSpec {
    PushRefSpec {
        delete: true,
        ..spec(None, dst)
    }
}

fn lease(src: ObjectId, dst: &str, expected_old: Option<ObjectId>) -> PushRefSpec {
    PushRefSpec {
        expected_old,
        force: true, // a lease is a guarded force; the lease, not FF, gates it
        ..spec(Some(src), dst)
    }
}

fn result_for<'a>(outcome: &'a PushOutcome, dst: &str) -> &'a PushRefResult {
    outcome
        .results
        .iter()
        .find(|r| r.remote_ref == dst)
        .unwrap_or_else(|| panic!("no result for {dst} in {:?}", outcome.results))
}

fn fsck_clean(bare: &Path, what: &str) {
    let out = Command::new("git")
        .current_dir(bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        out.status.success(),
        "git fsck failed after {what}: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Assert the remote ref equals `oid` both via grit's resolver and system git.
fn assert_remote_ref(bare: &Path, name: &str, oid: ObjectId) {
    assert_eq!(
        resolve_ref(bare, name).unwrap_or_else(|e| panic!("resolve {name}: {e}")),
        oid,
        "remote {name} oid mismatch (grit resolver)"
    );
    assert_eq!(
        git(bare, &["rev-parse", name]).trim(),
        oid.to_hex(),
        "remote {name} oid mismatch (system git)"
    );
}

fn assert_objects_present(bare: &Path, oids: &[ObjectId]) {
    let odb = open_odb(bare);
    for oid in oids {
        assert!(
            odb.exists(oid),
            "object {} missing from remote odb",
            oid.to_hex()
        );
        odb.read(oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }
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

fn wait_ready(port: u16, secs: u64) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// git:// daemon bring-up
// ---------------------------------------------------------------------------

fn spawn_daemon(base_path: &Path, port: u16) -> Option<Child> {
    Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg("--enable=receive-pack")
        .arg(format!("--base-path={}", base_path.display()))
        .arg(base_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

// ---------------------------------------------------------------------------
// ssh fake-ssh bring-up (mirrors transport_ssh_push.rs)
// ---------------------------------------------------------------------------

/// Write an executable fake-ssh script that ignores host/options, rewrites the
/// dashed transport name to the `git <service>` subcommand, and execs it
/// locally. Returns `None` if it cannot be created/made executable.
#[cfg(unix)]
fn write_fake_ssh(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ssh.sh");
    let body = r#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
  "git-receive-pack "*) cmd="git receive-pack ${cmd#git-receive-pack }" ;;
esac
eval "exec $cmd" 2>/dev/null
"#;
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

// ---------------------------------------------------------------------------
// grit-http-server bring-up (mirrors transport_http.rs)
// ---------------------------------------------------------------------------

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

fn spawn_server(server_bin: &Path, grit_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GUST_BIN", grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

fn make_bare_target(root: &Path, name: &str) -> PathBuf {
    let bare = root.join(name);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    bare
}

// ===========================================================================
// The transport-agnostic push matrix.
// ===========================================================================

/// A `push` closure abstracts the three transports behind one call shape. It
/// must reconnect fresh each invocation (the streaming transports are one-shot
/// per receive-pack session) and return the structured outcome.
type Pusher<'a> = dyn Fn(&[PushRefSpec], &PushOptions) -> PushOutcome + 'a;

/// Run the full push matrix against `bare` using `push` (which performs a fresh
/// connect per call) and the source repo at `local`/`graph`. `label` names the
/// transport for assertion messages.
fn run_push_matrix(label: &str, bare: &Path, graph: &Graph, push: &Pusher<'_>) {
    let Graph {
        c1,
        c2,
        ff,
        side,
        extra,
    } = *graph;

    // --- 1. New ref (create) --------------------------------------------------
    let out = push(
        &[spec(Some(c2), "refs/heads/main")],
        &PushOptions::default(),
    );
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "[{label}] create main: {:?} ({:?})",
        r.status,
        r.message
    );
    assert_eq!(r.new_oid, Some(c2));
    assert!(r.old_oid.is_none(), "[{label}] new ref has no old oid");
    assert_remote_ref(bare, "refs/heads/main", c2);
    assert_objects_present(bare, &[c1, c2]);
    fsck_clean(bare, &format!("{label} create"));

    // --- 2. Fast-forward update ----------------------------------------------
    let out = push(
        &[spec(Some(ff), "refs/heads/main")],
        &PushOptions::default(),
    );
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "[{label}] fast-forward main: {:?} ({:?})",
        r.status,
        r.message
    );
    assert!(!r.forced, "[{label}] a fast-forward is not a forced update");
    assert_eq!(r.old_oid, Some(c2), "[{label}] ff old oid should be c2");
    assert_eq!(r.new_oid, Some(ff));
    assert_remote_ref(bare, "refs/heads/main", ff);
    fsck_clean(bare, &format!("{label} fast-forward"));

    // --- 3. Up-to-date no-op (re-push the same tip) ---------------------------
    let out = push(
        &[spec(Some(ff), "refs/heads/main")],
        &PushOptions::default(),
    );
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::UpToDate,
        "[{label}] re-pushing the same tip must be UpToDate, got {:?}",
        r.status
    );
    assert_remote_ref(bare, "refs/heads/main", ff);

    // --- 4. Non-fast-forward without force is rejected client-side ------------
    // `side` (sibling of c2) is not a descendant of `ff`; pushing it without
    // force must be rejected without moving the remote ref.
    let out = push(
        &[spec(Some(side), "refs/heads/main")],
        &PushOptions::default(),
    );
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::RejectNonFastForward,
        "[{label}] non-ff push must be client-rejected, got {:?}",
        r.status
    );
    assert!(r.status.is_error());
    assert_remote_ref(bare, "refs/heads/main", ff); // unchanged

    // --- 5. Forced non-fast-forward moves the ref -----------------------------
    let out = push(&[forced(side, "refs/heads/main")], &PushOptions::default());
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "[{label}] forced non-ff push should be accepted: {:?}",
        r.message
    );
    assert!(r.forced, "[{label}] forced update should be flagged forced");
    assert_eq!(r.new_oid, Some(side));
    assert_remote_ref(bare, "refs/heads/main", side);
    assert_objects_present(bare, &[side]);
    fsck_clean(bare, &format!("{label} forced"));

    // --- 6. Deletion (no pack streamed) ---------------------------------------
    // Create a scratch ref, then delete it. A pure deletion must stream no pack;
    // if it did, the streaming transports would reset on the unread bytes — so a
    // successful deletion here also proves the no-pack behaviour end-to-end.
    let out = push(
        &[spec(Some(extra), "refs/heads/scratch")],
        &PushOptions::default(),
    );
    assert_eq!(
        result_for(&out, "refs/heads/scratch").status,
        PushRefStatus::Ok,
        "[{label}] scratch create for deletion"
    );
    assert_remote_ref(bare, "refs/heads/scratch", extra);

    let out = push(&[deletion("refs/heads/scratch")], &PushOptions::default());
    let r = result_for(&out, "refs/heads/scratch");
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "[{label}] deletion should be accepted: {:?}",
        r.message
    );
    assert!(r.deletion, "[{label}] result should be flagged a deletion");
    assert!(r.new_oid.is_none(), "[{label}] a deletion has no new oid");
    assert!(
        resolve_ref(bare, "refs/heads/scratch").is_err(),
        "[{label}] scratch ref must be gone after deletion"
    );
    fsck_clean(bare, &format!("{label} deletion"));

    // --- 7. Deleting an absent ref is an UpToDate no-op ------------------------
    let out = push(
        &[deletion("refs/heads/never-existed")],
        &PushOptions::default(),
    );
    assert_eq!(
        result_for(&out, "refs/heads/never-existed").status,
        PushRefStatus::UpToDate,
        "[{label}] deleting a non-existent ref is a no-op success"
    );

    // --- 8. Force-with-lease: stale expectation is client-rejected ------------
    // Remote main is at `side`; lease it against the wrong (`c2`) value. The
    // compare-and-swap fails -> RejectStale, ref unchanged, no wire round.
    let out = push(
        &[lease(ff, "refs/heads/main", Some(c2))],
        &PushOptions::default(),
    );
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::RejectStale,
        "[{label}] stale force-with-lease must be RejectStale, got {:?}",
        r.status
    );
    assert_remote_ref(bare, "refs/heads/main", side); // unchanged

    // --- 9. Force-with-lease: correct expectation succeeds --------------------
    // Lease against the real current value (`side`) and move to `ff`.
    let out = push(
        &[lease(ff, "refs/heads/main", Some(side))],
        &PushOptions::default(),
    );
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "[{label}] correct-lease force-with-lease should succeed: {:?}",
        r.message
    );
    assert_eq!(r.new_oid, Some(ff));
    assert_remote_ref(bare, "refs/heads/main", ff);
    fsck_clean(bare, &format!("{label} lease-ok"));

    // --- 10. Expect-absent lease on an existing ref is client-rejected --------
    // main exists, so an "only if absent" lease must fail as stale and not move
    // the ref.
    let mut absent = spec(Some(extra), "refs/heads/main");
    absent.expect_absent = true;
    let out = push(&[absent], &PushOptions::default());
    let r = result_for(&out, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::RejectStale,
        "[{label}] expect-absent lease on an existing ref must be RejectStale, got {:?}",
        r.status
    );
    assert_remote_ref(bare, "refs/heads/main", ff); // unchanged

    // --- 11. Atomic all-or-nothing: one bad ref aborts the whole push ---------
    // Pair a valid new ref (`refs/heads/atomic-ok`) with a guaranteed client
    // rejection (a non-ff on main). With `atomic`, neither must land: the good
    // ref is reported AtomicPushFailed and never created on the remote.
    let opts_atomic = PushOptions {
        atomic: true,
        ..PushOptions::default()
    };
    let out = push(
        &[
            spec(Some(extra), "refs/heads/atomic-ok"),
            spec(Some(side), "refs/heads/main"), // non-ff (side is sibling of ff)
        ],
        &opts_atomic,
    );
    let good = result_for(&out, "refs/heads/atomic-ok");
    let bad = result_for(&out, "refs/heads/main");
    assert_eq!(
        bad.status,
        PushRefStatus::RejectNonFastForward,
        "[{label}] atomic: the bad ref keeps its rejection, got {:?}",
        bad.status
    );
    assert_eq!(
        good.status,
        PushRefStatus::AtomicPushFailed,
        "[{label}] atomic: the otherwise-ok ref must become AtomicPushFailed, got {:?}",
        good.status
    );
    assert!(
        resolve_ref(bare, "refs/heads/atomic-ok").is_err(),
        "[{label}] atomic abort must not create the good ref"
    );
    assert_remote_ref(bare, "refs/heads/main", ff); // still at ff

    // --- 12. Multi-ref push: two new refs in one push both land ---------------
    let out = push(
        &[
            spec(Some(side), "refs/heads/multi-a"),
            spec(Some(extra), "refs/heads/multi-b"),
        ],
        &PushOptions::default(),
    );
    assert_eq!(
        result_for(&out, "refs/heads/multi-a").status,
        PushRefStatus::Ok,
        "[{label}] multi-ref a"
    );
    assert_eq!(
        result_for(&out, "refs/heads/multi-b").status,
        PushRefStatus::Ok,
        "[{label}] multi-ref b"
    );
    assert_remote_ref(bare, "refs/heads/multi-a", side);
    assert_remote_ref(bare, "refs/heads/multi-b", extra);
    assert_objects_present(bare, &[side, extra]);
    fsck_clean(bare, &format!("{label} multi-ref"));
}

// ===========================================================================
// git:// daemon matrix
// ===========================================================================

#[test]
fn push_matrix_over_git_daemon() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    let graph = build_source(&local);
    let local_git = local.join(".git");

    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["config", "daemon.receivepack", "true"]);

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = ChildGuard(child);
    if !wait_ready(port, 5) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");
    let transport = GitDaemonTransport::new();

    // Probe: confirm receive-pack is actually reachable before asserting.
    match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("SKIP: git daemon refuses receive-pack: {e}");
            return;
        }
    }

    let push = |specs: &[PushRefSpec], opts: &PushOptions| -> PushOutcome {
        let mut conn = transport
            .connect(&url, Service::ReceivePack, &ConnectOptions::default())
            .expect("connect git daemon receive-pack");
        push_remote(&local_git, &mut *conn, specs, opts, &mut NoProgress)
            .expect("push_remote over git daemon")
    };

    run_push_matrix("git", &bare, &graph, &push);
}

// ===========================================================================
// ssh (fake-ssh) matrix
// ===========================================================================

#[cfg(unix)]
#[test]
fn push_matrix_over_ssh() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    let graph = build_source(&local);
    let local_git = local.join(".git");

    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let abs_path = bare.to_str().expect("utf8 path");
    let url = format!("ssh://git@fakehost{abs_path}");

    // Probe connectivity once; skip cleanly if the fake-ssh path cannot run.
    match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => assert!(
            c.protocol_version() < 2,
            "receive-pack must be v0/v1, got v{}",
            c.protocol_version()
        ),
        Err(e) => {
            eprintln!("SKIP: fake-ssh receive-pack not runnable: {e}");
            return;
        }
    }

    let push = |specs: &[PushRefSpec], opts: &PushOptions| -> PushOutcome {
        let mut conn = transport
            .connect(&url, Service::ReceivePack, &ConnectOptions::default())
            .expect("connect ssh receive-pack");
        push_remote(&local_git, &mut *conn, specs, opts, &mut NoProgress)
            .expect("push_remote over ssh")
    };

    run_push_matrix("ssh", &bare, &graph, &push);
}

// ===========================================================================
// smart-HTTP matrix
// ===========================================================================

#[test]
fn push_matrix_over_smart_http() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    let graph = build_source(&local);
    let local_git = local.join(".git");

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = make_bare_target(&root, "matrix.git");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &grit_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ChildGuard(child);
    if !wait_ready(port, 10) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/matrix.git");

    // Probe: require a smart receive-pack advertisement; skip otherwise.
    let probe = UreqHttpClient::new();
    let probe_url = format!("{url}/info/refs?service=git-receive-pack");
    match probe.get(&probe_url, None) {
        Ok(body) if body.windows(20).any(|w| w == b"# service=git-receiv") => {}
        Ok(_) => {
            eprintln!("SKIP: server returned a non-smart receive-pack advertisement");
            return;
        }
        Err(e) => {
            eprintln!("SKIP: server does not offer receive-pack: {e}");
            return;
        }
    }

    let push = |specs: &[PushRefSpec], opts: &PushOptions| -> PushOutcome {
        let client = UreqHttpClient::new();
        push_http(&client, &local_git, &url, specs, opts, &mut NoProgress)
            .expect("push_http over grit-http-server")
    };

    run_push_matrix("http", &bare, &graph, &push);

    // Bonus HTTP-only round-trip: fetch the pushed history back via the smart
    // transport's advertisement to prove the pushed pack is servable.
    let conn = SmartHttpTransport::new(UreqHttpClient::new())
        .connect(&url, Service::UploadPack, &ConnectOptions::default())
        .expect("connect upload-pack after matrix");
    assert!(
        conn.advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == graph.ff),
        "after the matrix, upload-pack must advertise main at ff"
    );
}

// ===========================================================================
// Remote-rejection surfacing: a declining pre-receive hook -> RemoteRejected.
// ===========================================================================

/// Init a bare repo under `base` whose `pre-receive` hook exits non-zero (and
/// prints a reason), so `git-receive-pack` declines every command with an `ng`
/// report-status line. Returns the bare path.
fn init_bare_declining_hook(base: &Path, name: &str) -> PathBuf {
    let bare = base.join(name);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["config", "daemon.receivepack", "true"]);

    let hook = bare.join("hooks").join("pre-receive");
    // Drain the command list so receive-pack proceeds to run the hook, then
    // decline. The non-zero exit makes receive-pack reject all updates.
    let body = "#!/bin/sh\ncat >/dev/null\necho 'policy: pushes are blocked' 1>&2\nexit 1\n";
    std::fs::write(&hook, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }
    bare
}

#[test]
fn remote_rejection_surfaces_over_git_daemon() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    let graph = build_source(&local);
    let local_git = local.join(".git");

    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = init_bare_declining_hook(&base, "blocked.git");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = ChildGuard(child);
    if !wait_ready(port, 5) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/blocked.git");
    let transport = GitDaemonTransport::new();
    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon receive-pack: {e}");
            return;
        }
    };

    // A clean new-ref create that the *server* (hook) declines: the client gate
    // accepts it (new ref), but the hook's non-zero exit yields an `ng` line.
    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec(Some(graph.c2), "refs/heads/main")],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("push against declining hook completes");
    drop(conn);

    let r = result_for(&outcome, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::RemoteRejected,
        "a declining pre-receive hook must surface as RemoteRejected, got {:?} ({:?})",
        r.status,
        r.message
    );
    // Nothing landed: the ref must not exist on the remote.
    assert!(
        resolve_ref(&bare, "refs/heads/main").is_err(),
        "server-rejected push must not create the ref"
    );
}

#[cfg(unix)]
#[test]
fn remote_rejection_surfaces_over_ssh() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    let graph = build_source(&local);
    let local_git = local.join(".git");

    let bare = init_bare_declining_hook(tmp.path(), "blocked.git");

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };
    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let url = format!("ssh://git@fakehost{}", bare.to_str().expect("utf8 path"));

    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect over fake-ssh receive-pack: {e}");
            return;
        }
    };

    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec(Some(graph.c2), "refs/heads/main")],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("ssh push against declining hook completes");
    drop(conn);

    let r = result_for(&outcome, "refs/heads/main");
    assert_eq!(
        r.status,
        PushRefStatus::RemoteRejected,
        "a declining pre-receive hook must surface as RemoteRejected over ssh, got {:?} ({:?})",
        r.status,
        r.message
    );
    assert!(
        resolve_ref(&bare, "refs/heads/main").is_err(),
        "server-rejected ssh push must not create the ref"
    );
}

// ===========================================================================
// report-status (v1) vs report-status-v2: both wire shapes must parse.
//
// Real `git-receive-pack` (2.x) replies with the v2 report whenever the client
// advertises `report-status-v2` (which `push_remote` always does), so the
// happy-path tests above already exercise v2 parsing against a real server. To
// *also* pin the plain-v1 report and the v2-specific trailing `option` line, we
// drive a tiny real fake-`receive-pack` shell script over the SSH transport: it
// emits a valid v0 advertisement, drains the client's command list + pack, then
// writes a chosen report (v1 or v2) byte-for-byte. This is a real subprocess
// wire exchange, not a synthetic in-process call.
// ===========================================================================

/// Build a fake-ssh script whose "receive-pack" is `fake_rp`: the script execs
/// the fake receive-pack regardless of the requested command. Returns `None` if
/// it cannot be created.
#[cfg(unix)]
fn write_fake_ssh_to(dir: &Path, name: &str, fake_rp: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join(name);
    // Ignore every argument and run the canned receive-pack; the path argument
    // is irrelevant because the fake server writes a fixed report.
    let body = format!("#!/bin/sh\nexec {} 2>/dev/null\n", fake_rp.display());
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

/// Write an executable fake-`receive-pack` POSIX script that:
///   1. prints a v0 advertisement: one ref line carrying capabilities (NO
///      side-band, so `push_remote` reads the report raw), then a flush;
///   2. drains stdin to EOF (the client's command list + pack);
///   3. prints `$REPORT_BYTES` (the chosen report-status body) and exits.
///
/// `report_bytes` is the exact pkt-line report to emit (built with [`pkt`]).
#[cfg(unix)]
fn write_fake_receive_pack(
    dir: &Path,
    name: &str,
    adv_oid: &str,
    report_bytes: &[u8],
) -> Option<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    // The advertisement: `<oid> refs/heads/main\0<caps>` then flush. Caps include
    // report-status + report-status-v2 (so the client knows the server can do
    // both) but deliberately omit side-band-64k so the report is read raw.
    let adv_line = format!(
        "{adv_oid} refs/heads/main\0report-status report-status-v2 delete-refs ofs-delta object-format=sha1\n"
    );
    let mut adv = Vec::new();
    write_pkt(&mut adv, adv_line.as_bytes());
    adv.extend_from_slice(b"0000");

    // Emit raw pkt-line bytes via `printf` octal escapes so arbitrary bytes
    // survive the shell. The advertisement is written first (the client waits
    // for it before sending its command list), then stdin is drained, then the
    // report is written — so the two payloads are escaped and emitted separately.
    let adv_escaped = octal_escape(&adv);
    let report_escaped = octal_escape(report_bytes);

    // Sequencing the fake `receive-pack` is subtle because `push_remote`
    // deliberately keeps its write half open until it has read the full report
    // (see `push.rs`): it never sends an EOF the server could wait on, and it
    // only stops *reading* (`read_to_end`) at the server's stdout EOF.
    //
    //   * A foreground `cat >/dev/null` (drain to EOF) before the report would
    //     deadlock — the client never closes its write half.
    //   * Writing the report and exiting immediately races the client's writes:
    //     the script (and the read end of its stdin) can vanish before the
    //     client finishes `write_all`, so the client takes EPIPE/BrokenPipe.
    //
    // Real `git-receive-pack` avoids this by reading the whole command list +
    // length-delimited pack *first* (so all client input is consumed), then
    // writing the report and closing. We can't parse the pack here, so instead we
    // drain stdin in the background (keeping the read end open so the client's
    // small command-list + pack writes never block or EPIPE), write the report
    // (which the client reads as it arrives), briefly hold the process open so
    // those writes are fully absorbed, then exit — closing stdout and giving the
    // client's `read_to_end` a clean EOF.
    let script = dir.join(name);
    let body = format!(
        "#!/bin/sh\nprintf '{adv}'\ncat >/dev/null &\nprintf '{report}'\nsleep 1\n",
        adv = adv_escaped,
        report = report_escaped,
    );
    let mut f = std::fs::File::create(&script).ok()?;
    f.write_all(body.as_bytes()).ok()?;
    drop(f);
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

/// Append a single pkt-line (`<4-hex-len><payload>`) to `out`.
fn write_pkt(out: &mut Vec<u8>, payload: &[u8]) {
    let len = payload.len() + 4;
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(payload);
}

/// Build a pkt-line carrying `s` (a trailing `\n` is included as given).
fn pkt(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    write_pkt(&mut v, s.as_bytes());
    v
}

/// Render bytes as a `printf`-safe octal-escaped string (`\NNN` per byte).
fn octal_escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 4);
    for &b in bytes {
        s.push_str(&format!("\\{b:03o}"));
    }
    s
}

#[cfg(unix)]
#[test]
fn report_status_v1_and_v2_both_parse_over_ssh() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");

    // A real source repo so the client builds a genuine command list + pack.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    let graph = build_source(&local);
    let local_git = local.join(".git");

    // The advertised remote oid for `refs/heads/main`: use `c1` so pushing `c2`
    // is a real fast-forward update the client will accept and send. (The fake
    // server ignores what we send and replies with the canned report.)
    let adv_oid = graph.c1.to_hex();

    // We push main: c1 -> c2 (a fast-forward against the advertised c1).
    let push_specs = [spec(Some(graph.c2), "refs/heads/main")];

    // ---- Case A: explicit v1 report-status, accepting the ref ----------------
    {
        let mut report = Vec::new();
        report.extend_from_slice(&pkt("unpack ok\n"));
        report.extend_from_slice(&pkt("ok refs/heads/main\n"));
        report.extend_from_slice(b"0000");

        let Some(rp) = write_fake_receive_pack(tmp.path(), "rp-v1-ok.sh", &adv_oid, &report) else {
            eprintln!("SKIP: could not create fake receive-pack script");
            return;
        };
        let Some(ssh) = write_fake_ssh_to(tmp.path(), "ssh-v1-ok.sh", &rp) else {
            eprintln!("SKIP: could not create fake-ssh wrapper");
            return;
        };
        let transport = SshTransport::with_program(ssh.as_os_str());
        let url = "ssh://git@fakehost/whatever.git".to_owned();
        let mut conn = transport
            .connect(&url, Service::ReceivePack, &ConnectOptions::default())
            .expect("connect fake receive-pack (v1 ok)");
        // The advertisement was parsed: main at the advertised oid.
        assert!(
            conn.advertised_refs()
                .iter()
                .any(|(n, o)| n == "refs/heads/main" && *o == graph.c1),
            "fake server should advertise main at c1, got {:?}",
            conn.advertised_refs()
        );
        let out = push_remote(
            &local_git,
            &mut *conn,
            &push_specs,
            &PushOptions::default(),
            &mut NoProgress,
        )
        .expect("push reading a v1 report");
        drop(conn);
        let r = result_for(&out, "refs/heads/main");
        assert_eq!(
            r.status,
            PushRefStatus::Ok,
            "v1 `ok` report must keep the accepted (Ok) status, got {:?}",
            r.status
        );
    }

    // ---- Case B: explicit v2 report-status, declining the ref ----------------
    // The v2 report adds the `option` trailing line after the per-ref status,
    // which the parser must tolerate while still folding `ng` -> RemoteRejected.
    {
        let mut report = Vec::new();
        report.extend_from_slice(&pkt("unpack ok\n"));
        report.extend_from_slice(&pkt("ng refs/heads/main pre-receive hook declined\n"));
        // A v2-only trailing option line (e.g. ref-status options). The v0/v1
        // parser must skip it harmlessly.
        report.extend_from_slice(&pkt("option refname refs/heads/main\n"));
        report.extend_from_slice(b"0000");

        let Some(rp) = write_fake_receive_pack(tmp.path(), "rp-v2-ng.sh", &adv_oid, &report) else {
            eprintln!("SKIP: could not create fake receive-pack script");
            return;
        };
        let Some(ssh) = write_fake_ssh_to(tmp.path(), "ssh-v2-ng.sh", &rp) else {
            eprintln!("SKIP: could not create fake-ssh wrapper");
            return;
        };
        let transport = SshTransport::with_program(ssh.as_os_str());
        let url = "ssh://git@fakehost/whatever.git".to_owned();
        let mut conn = transport
            .connect(&url, Service::ReceivePack, &ConnectOptions::default())
            .expect("connect fake receive-pack (v2 ng)");
        let out = push_remote(
            &local_git,
            &mut *conn,
            &push_specs,
            &PushOptions::default(),
            &mut NoProgress,
        )
        .expect("push reading a v2 report");
        drop(conn);
        let r = result_for(&out, "refs/heads/main");
        assert_eq!(
            r.status,
            PushRefStatus::RemoteRejected,
            "v2 `ng` report must surface as RemoteRejected, got {:?}",
            r.status
        );
        assert_eq!(
            r.message.as_deref(),
            Some("pre-receive hook declined"),
            "the server's ng reason must be captured, got {:?}",
            r.message
        );
    }
}
