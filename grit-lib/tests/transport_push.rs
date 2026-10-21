//! Integration test for the `git://` push path: `GitDaemonTransport` +
//! `push::push_remote` against a `git receive-pack` daemon.
//!
//! A bare remote repo is served by a real `git daemon --enable=receive-pack`.
//! A local repo with commits pushes `refs/heads/main` via the trait
//! (`GitDaemonTransport::connect(.., Service::ReceivePack, ..)` -> `push_remote`).
//! We assert the remote ref and objects landed, `git -C <bare> fsck` is clean,
//! and a non-fast-forward push (no force) reports a per-ref rejection without
//! moving the remote ref.
//!
//! The test skips gracefully (returns early) when `git daemon` is unavailable,
//! cannot bind, or refuses the receive-pack service — the happy path is
//! otherwise real end-to-end wire I/O.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push::push_remote;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_lib::remote::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};

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

/// Build a source repo: two commits on `main`, plus a divergent `other` commit
/// reachable from a separate ref (used for the non-fast-forward case).
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

/// Spawn `git daemon` over `base_path` on `port` with receive-pack enabled.
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

fn wait_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn push_over_git_daemon_lands_ref_and_objects_and_reports_rejection() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Local source repo with two commits on main.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let c1_oid = rev_parse(&local, "HEAD~1");

    // Bare remote under the daemon base path, served at `/repo.git`. Start it
    // empty so the push creates `refs/heads/main` from scratch.
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    // The daemon refuses an unconfigured receive-pack via the daemon protocol
    // unless the repo opts in; `--enable=receive-pack` on the daemon covers it,
    // but also set the per-repo flag so older daemons honor it.
    git(&bare, &["config", "daemon.receivepack", "true"]);

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");
    let transport = GitDaemonTransport::new();

    // --- 1. Push refs/heads/main (create) -------------------------------------
    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            // Some environments forbid the daemon receive-pack service entirely;
            // treat the connect failure as a skip rather than a hard failure.
            eprintln!("SKIP: could not connect to git daemon receive-pack: {e}");
            return;
        }
    };

    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec],
        &PushOptions {
            tracking_remote: Some("origin".to_owned()),
            ..PushOptions::default()
        },
        &mut NoProgress,
    )
    .expect("push_remote over git daemon");
    drop(conn);

    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "push of new ref should be accepted, got {:?} ({:?})",
        r.status,
        r.message
    );
    assert_eq!(r.new_oid, Some(main_oid));
    assert!(r.old_oid.is_none(), "new ref has no old value");

    let tracking = resolve_ref(&local_git, "refs/remotes/origin/main").expect("tracking ref");
    assert_eq!(
        tracking, main_oid,
        "PushOptions.tracking_remote must update refs/remotes/origin/main"
    );

    // The remote ref now points at our main, and the objects are present.
    let remote_main = resolve_ref(&bare, "refs/heads/main").expect("remote main written");
    assert_eq!(remote_main, main_oid, "remote main oid mismatch");

    let remote_odb = open_odb(&bare);
    for oid in [main_oid, c1_oid] {
        assert!(
            remote_odb.exists(&oid),
            "object {} missing from remote odb after push",
            oid.to_hex()
        );
        remote_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // The bare repo fscks clean after receiving the pushed pack.
    let fsck = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after push: {}\n{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );

    // --- 2. Non-fast-forward push (no force) is rejected per-ref --------------
    // Rewrite local main onto a divergent history so it is no longer a
    // descendant of the just-pushed tip; pushing it without force must be
    // rejected and must NOT move the remote ref.
    git(&local, &["checkout", "-q", "-b", "diverge", "HEAD~1"]);
    std::fs::write(local.join("c.txt"), "three\n").unwrap();
    git(&local, &["add", "c.txt"]);
    git(&local, &["commit", "-q", "-m", "divergent"]);
    let diverged = rev_parse(&local, "HEAD");
    assert_ne!(diverged, main_oid);

    let mut conn2 = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect for non-ff push");
    // The reconnected advertisement should show main at the previously-pushed oid.
    assert!(
        conn2
            .advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == main_oid),
        "advertisement should report remote main at {}",
        main_oid.to_hex()
    );

    let nonff = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome2 = push_remote(
        &local_git,
        &mut *conn2,
        &[nonff],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("non-ff push_remote completes");
    drop(conn2);

    assert_eq!(outcome2.results.len(), 1);
    let r2 = &outcome2.results[0];
    assert!(
        r2.status.is_error(),
        "non-fast-forward push must be rejected, got {:?}",
        r2.status
    );
    assert_eq!(
        r2.status,
        PushRefStatus::RejectNonFastForward,
        "client-side non-ff detection should reject before sending"
    );

    // The remote ref must be unchanged by the rejected push.
    let remote_main_after = resolve_ref(&bare, "refs/heads/main").expect("remote main still set");
    assert_eq!(
        remote_main_after, main_oid,
        "rejected non-ff push must not move the remote ref"
    );

    // --- 3. Force-update: the divergent tip is accepted with `force` ----------
    // Same non-ff update as case 2, but with force: the server accepts it and
    // the remote ref advances to the divergent commit.
    let mut conn3 = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect for forced push");
    let forced = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: true,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome3 = push_remote(
        &local_git,
        &mut *conn3,
        &[forced],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("forced push_remote completes");
    drop(conn3);

    let r3 = &outcome3.results[0];
    assert_eq!(
        r3.status,
        PushRefStatus::Ok,
        "forced push should be accepted, got {:?} ({:?})",
        r3.status,
        r3.message
    );
    assert!(r3.forced, "forced update should be flagged forced");
    assert_eq!(r3.new_oid, Some(diverged));
    let remote_main_forced =
        resolve_ref(&bare, "refs/heads/main").expect("remote main after force");
    assert_eq!(
        remote_main_forced, diverged,
        "forced push must advance the remote ref to the divergent tip"
    );

    // The divergent commit's objects landed and the bare repo still fscks clean.
    let remote_odb2 = open_odb(&bare);
    assert!(
        remote_odb2.exists(&diverged),
        "divergent object {} missing after forced push",
        diverged.to_hex()
    );
    let fsck2 = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck2.status.success(),
        "git fsck failed after forced push: {}",
        String::from_utf8_lossy(&fsck2.stderr)
    );

    // --- 4. Deletion: push a null update to remove the remote ref -------------
    // First create a second ref to delete (so we never delete the repo's only
    // branch, which the daemon may protect), then delete it with a null source.
    let mut conn_mk = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect to create deletable ref");
    let mk = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/scratch".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    push_remote(
        &local_git,
        &mut *conn_mk,
        &[mk],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("create scratch ref");
    drop(conn_mk);
    assert_eq!(
        resolve_ref(&bare, "refs/heads/scratch").expect("scratch created"),
        diverged,
        "scratch ref should be created before deletion"
    );

    let mut conn_del = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect for deletion");
    let del = PushRefSpec {
        src: None,
        dst: "refs/heads/scratch".to_owned(),
        force: false,
        delete: true,
        expected_old: None,
        expect_absent: false,
    };
    let outcome_del = push_remote(
        &local_git,
        &mut *conn_del,
        &[del],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("deletion push_remote completes");
    drop(conn_del);

    let rdel = &outcome_del.results[0];
    assert_eq!(
        rdel.status,
        PushRefStatus::Ok,
        "deletion should be accepted, got {:?} ({:?})",
        rdel.status,
        rdel.message
    );
    assert!(rdel.deletion, "result should be flagged as a deletion");
    assert!(
        rdel.new_oid.is_none(),
        "a deletion has no new oid, got {:?}",
        rdel.new_oid
    );
    assert!(
        resolve_ref(&bare, "refs/heads/scratch").is_err(),
        "scratch ref must be gone after a deletion push"
    );
}

/// Two clones of the same daemon repo: when clone A advances `main`, clone B
/// must still push a brand-new branch without fetching (issue #1019).
#[test]
fn push_new_branch_without_local_copy_of_advanced_remote_main() {
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::porcelain::add::stage_for_commit;
    use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    use grit_lib::progress::NullProgress;
    use grit_lib::repo::Repository;

    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["config", "daemon.receivepack", "true"]);

    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main", "."]);
    git(&seed, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "-q", "origin", "HEAD:main"]);
    let seed_main = rev_parse(&bare, "refs/heads/main");
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }
    let url = format!("git://127.0.0.1:{port}/repo.git");

    fn grit_commit(repo: &Repository, filename: &str, message: &str) -> ObjectId {
        let wt = repo.work_tree().expect("worktree");
        std::fs::write(wt.join(filename), format!("{message}\n")).unwrap();
        stage_for_commit(repo, &mut NullProgress).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        let out = create_commit(
            repo,
            &CommitRequest {
                message: message.to_owned(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
<<<<<<< New base: Merge branch 'factory/commit-dry-run-3068'
                ..Default::default()
||||||| Common ancestor
=======
                hooks: Default::default(),
>>>>>>> Current commit: commit: message file, author override, and no-verify hooks
            },
            &mut NullProgress,
        )
        .expect("commit");
        out.oid
    }

    let clone_a = tmp.path().join("cA");
    let clone_b = tmp.path().join("cB");
    git(&clone_a.parent().unwrap(), &["clone", "-q", &url, "cA"]);
    git(&clone_b.parent().unwrap(), &["clone", "-q", &url, "cB"]);
    assert_eq!(
        rev_parse(&clone_a, "HEAD"),
        seed_main,
        "clone A should start at seeded main"
    );
    assert_eq!(
        rev_parse(&clone_b, "HEAD"),
        seed_main,
        "clone B should start at seeded main"
    );

    let repo_a = Repository::discover(Some(&clone_a), &RepositoryOptions::empty()).expect("open a");
    let advanced = grit_commit(&repo_a, "a.txt", "A");
    assert_ne!(advanced, seed_main);
    let transport = GitDaemonTransport::new();
    let mut conn_a = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("connect a");
    let outcome_a = push_remote(
        &clone_a.join(".git"),
        &mut *conn_a,
        &[PushRefSpec {
            src: Some(advanced),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("clone A advances main");
    drop(conn_a);
    assert_eq!(outcome_a.results.len(), 1);
    assert_eq!(
        outcome_a.results[0].status,
        PushRefStatus::Ok,
        "clone A ff push to main should succeed, got {:?}",
        outcome_a.results[0].message
    );
    let remote_main_after_a = resolve_ref(&bare, "refs/heads/main").expect("remote main");
    assert_eq!(
        remote_main_after_a, advanced,
        "remote main must advance after clone A push"
    );

    git(&clone_b, &["checkout", "-q", "-b", "newbr"]);
    let repo_b = Repository::discover(Some(&clone_b), &RepositoryOptions::empty()).expect("open b");
    let clone_b_odb = open_odb(&clone_b.join(".git"));
    assert!(
        !clone_b_odb.exists(&advanced),
        "clone B must not have clone A's advanced main commit locally"
    );
    let new_tip = grit_commit(&repo_b, "b.txt", "B");

    let mut conn_b = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("connect b");
    let outcome = push_remote(
        &clone_b.join(".git"),
        &mut *conn_b,
        &[PushRefSpec {
            src: Some(new_tip),
            dst: "refs/heads/newbr".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("clone B pushes new branch without fetch");
    drop(conn_b);

    assert_eq!(outcome.results.len(), 1);
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::Ok,
        "new branch push should succeed, got {:?} ({:?})",
        outcome.results[0].status,
        outcome.results[0].message
    );
    let remote_newbr = resolve_ref(&bare, "refs/heads/newbr").expect("remote newbr");
    assert_eq!(remote_newbr, new_tip);
    assert!(
        open_odb(&bare).exists(&new_tip),
        "pushed commit missing on remote"
    );

    // Force-updating main without the advertised remote tip in the local ODB must work.
    let mut conn_force = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("connect for force main");
    assert!(
        !clone_b_odb.exists(&remote_main_after_a),
        "clone B still lacks the advertised main tip"
    );
    let outcome_force = push_remote(
        &clone_b.join(".git"),
        &mut *conn_force,
        &[PushRefSpec {
            src: Some(new_tip),
            dst: "refs/heads/main".to_owned(),
            force: true,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("forced main push without local copy of old tip");
    drop(conn_force);
    assert_eq!(outcome_force.results.len(), 1);
    assert_eq!(
        outcome_force.results[0].status,
        PushRefStatus::Ok,
        "forced main update should succeed, got {:?}",
        outcome_force.results[0].message
    );
    assert!(outcome_force.results[0].forced);
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("main after force"),
        new_tip
    );

    let fsck = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "git fsck after new-branch push: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
