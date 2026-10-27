//! Integration test for the `ssh` push path: `SshTransport` +
//! `push::push_remote` driving a `git-receive-pack` over an ssh subprocess.
//!
//! As in `transport_ssh.rs` (the fetch side), there is no real ssh server.
//! We use Git's own test-suite trick: point the ssh command at a tiny "fake
//! ssh" shell script that ignores the host argument and execs the remote
//! command — here `git-receive-pack '<path>'`, rewritten to the `git
//! receive-pack` subcommand form — *locally*. So the wire bytes are a real
//! receive-pack exchange piped through the fake-ssh subprocess, exactly as the
//! `SshTransport` would drive a real `ssh`.
//!
//! This is the streaming-transport counterpart to the `git://` push test in
//! `transport_push.rs`: it proves `push_remote`'s `finish_send` teardown lets
//! the persistent receive-pack serve loop terminate cleanly over an ssh
//! subprocess (no hang in `read_to_end` / `child.wait()` on `Drop`), and that
//! the report-status is read back correctly.
//!
//! Cases:
//!   * create `refs/heads/main` (new ref): ref + objects land, `git fsck` clean;
//!   * a non-fast-forward push without force is rejected per-ref and does not
//!     move the remote ref.
//!
//! Pushes use `ConnectOptions::default()` (protocol_version = 0): receive-pack
//! has no protocol v2, so the SSH transport must not set `GIT_PROTOCOL` and the
//! exchange stays v0/v1.
//!
//! The test skips gracefully (returns early) on platforms without `sh`/`git`,
//! or where the script cannot be made executable.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push::push_remote;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_lib::remote::transport::{ConnectOptions, Service, SshTransport, Transport};

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

/// Build a source repo: two commits on `main`, plus a divergent `diverge`
/// commit (used for the non-fast-forward case).
fn build_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
}

/// Write an executable fake-ssh script and return its path, or `None` if it
/// cannot be created/made executable on this platform.
///
/// The script is invoked as `<script> [<-p port>] <host> <remote-cmd>`, where
/// `<remote-cmd>` is `git-receive-pack '<path>'`. It ignores everything except
/// the last argument (the remote command), rewrites `git-receive-pack` to
/// `git receive-pack` (a subcommand, always available), and `eval`s it so the
/// shell-quoted path is parsed by the shell.
fn write_fake_ssh(dir: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-ssh.sh");
    // The remote command's stderr is discarded: when `push_remote` rejects an
    // update client-side (e.g. the non-ff case) it sends no command list, so the
    // far-side `git receive-pack` sees EOF after its advertisement and prints a
    // benign "the remote end hung up unexpectedly" to stderr before exiting
    // cleanly. That is expected fixture noise (real `git push` triggers the same
    // notice), so we keep it out of the test log.
    let body = r#"#!/bin/sh
# Fake ssh: ignore host/options, run the remote command locally.
# The remote command is always the last argument.
cmd=
for cmd in "$@"; do :; done
# Rewrite the dashed transport name to the `git <service>` subcommand form.
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

fn spec_update(src: ObjectId, dst: &str, force: bool) -> PushRefSpec {
    PushRefSpec {
        src: Some(src),
        src_ref: None,
        dst: dst.to_owned(),
        force,
        delete: false,
        expected_old: None,
        expect_absent: false,
        ..Default::default()
    }
}

#[test]
fn push_over_ssh_lands_ref_and_objects_and_reports_nonff_rejection() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");

    // Local source repo with two commits on main.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let c1_oid = rev_parse(&local, "HEAD~1");

    // Empty bare remote: the push creates `refs/heads/main` from scratch.
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };

    // `with_program` is the $GIT_SSH shape: argv `<host> <remote-cmd>`, no shell
    // wrapping by the transport. The fake-ssh ignores the host and execs the
    // remote `git receive-pack '<bare>'` locally.
    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let abs_path = bare.to_str().expect("utf8 path");
    let url = format!("ssh://git@fakehost{abs_path}");

    // --- 1. Push refs/heads/main (create) -------------------------------------
    // protocol_version = 0: receive-pack has no v2, so the transport must not set
    // GIT_PROTOCOL and the exchange stays v0/v1.
    let mut conn = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("SshTransport::connect receive-pack");
    assert!(
        conn.protocol_version() < 2,
        "receive-pack push must be v0/v1, got v{}",
        conn.protocol_version()
    );
    // An empty bare repo advertises only the all-zero capabilities carrier, so
    // there should be no real remote refs yet.
    assert!(
        conn.advertised_refs().is_empty(),
        "empty remote should advertise no refs, got {:?}",
        conn.advertised_refs()
    );

    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec_update(main_oid, "refs/heads/main", false)],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("push_remote over ssh");
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

    // The remote ref now points at our main, and the objects are present.
    let remote_main = resolve_ref(&bare, "refs/heads/main").expect("remote main written");
    assert_eq!(remote_main, main_oid, "remote main oid mismatch");

    let remote_odb = open_odb(&bare);
    for oid in [main_oid, c1_oid] {
        assert!(
            remote_odb.exists(&oid),
            "object {} missing from remote odb after ssh push",
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
        "git fsck failed after ssh push: {}\n{}",
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
        .expect("reconnect for non-ff ssh push");
    // The reconnected advertisement should show main at the previously-pushed oid.
    assert!(
        conn2
            .advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == main_oid),
        "advertisement should report remote main at {}",
        main_oid.to_hex()
    );

    let outcome2 = push_remote(
        &local_git,
        &mut *conn2,
        &[spec_update(diverged, "refs/heads/main", false)],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("non-ff push_remote over ssh completes");
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
        "rejected non-ff ssh push must not move the remote ref"
    );

    // --- 3. Force-update over ssh moves the ref to the divergent tip ----------
    // The same divergent commit, pushed with force, is accepted server-side and
    // the remote ref advances (proving the report-status `ok` round-trips over
    // the ssh streaming channel after `finish_send`).
    let mut conn3 = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect for forced ssh push");
    let outcome3 = push_remote(
        &local_git,
        &mut *conn3,
        &[spec_update(diverged, "refs/heads/main", true)],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("forced push_remote over ssh completes");
    drop(conn3);

    assert_eq!(outcome3.results.len(), 1);
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
        "forced ssh push must advance the remote ref to the divergent tip"
    );

    let fsck2 = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck2.status.success(),
        "git fsck failed after forced ssh push: {}",
        String::from_utf8_lossy(&fsck2.stderr)
    );
}

#[test]
fn incremental_ssh_push_with_advertised_blob_and_tree_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let parent = rev_parse(&local, "HEAD~1");
    let tip = rev_parse(&local, "HEAD");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(
        &bare,
        &[
            "fetch",
            "-q",
            local.to_str().unwrap(),
            &format!("{}:refs/heads/main", parent.to_hex()),
        ],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    for (name, target) in [("blob-tag", "HEAD:a.txt"), ("tree-tag", "HEAD^{tree}")] {
        git(&bare, &["tag", "-a", name, "-m", name, target]);
    }
    // Keep the advertised tags locally too: send-pack must peel them and skip
    // non-commit targets rather than failing during have-boundary selection.
    git(
        &local,
        &[
            "fetch",
            "-q",
            bare.to_str().unwrap(),
            "refs/tags/*:refs/tags/*",
        ],
    );
    assert!(!open_odb(&bare).exists(&tip));
    let script = write_fake_ssh(tmp.path()).expect("executable fake ssh");
    let transport = SshTransport::with_program(script.as_os_str());
    let url = format!("ssh://git@fakehost{}", bare.display());
    let mut conn = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("receive-pack connection");
    assert_eq!(conn.advertised_refs().len(), 3);
    let outcome = push_remote(
        &local.join(".git"),
        &mut *conn,
        &[spec_update(tip, "refs/heads/main", false)],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("incremental push with non-commit tags");
    drop(conn);
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(rev_parse(&bare, "refs/heads/main"), tip);
    for name in ["blob-tag", "tree-tag"] {
        assert_eq!(rev_parse(&bare, name), rev_parse(&local, name));
    }
    git(&bare, &["fsck", "--strict", "--no-dangling"]);
}

/// One new commit pushed to a remote that advertises many unrelated branches and tags
/// travels as a thin pack of exactly that commit, its tree and the changed blob, deltified
/// against the version the remote already has.
#[test]
fn one_commit_ssh_push_over_many_advertised_tips_sends_a_thin_pack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let big: String = (0..400).map(|i| format!("line {i}\n")).collect();
    std::fs::write(local.join("big.txt"), &big).unwrap();
    git(&local, &["add", "big.txt"]);
    git(&local, &["commit", "-q", "-m", "base"]);
    for i in 0..30 {
        git(
            &local,
            &["checkout", "-q", "-b", &format!("side{i}"), "main"],
        );
        std::fs::write(local.join(format!("side{i}.txt")), format!("{i}\n{big}")).unwrap();
        git(&local, &["add", "."]);
        git(&local, &["commit", "-q", "-m", &format!("side {i}")]);
        git(&local, &["tag", "-a", &format!("v{i}"), "-m", "release"]);
    }
    git(&local, &["checkout", "-q", "main"]);
    let bare = tmp.path().join("remote.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            local.to_str().unwrap(),
            "remote.git",
        ],
    );
    git(&bare, &["config", "transfer.unpackLimit", "1"]);
    std::fs::write(local.join("big.txt"), format!("{big}one more line\n")).unwrap();
    git(&local, &["commit", "-q", "-am", "edit"]);
    let tip = rev_parse(&local, "HEAD");

    let packs = |dir: &Path| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir.join("objects/pack"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "idx"))
            .collect()
    };
    let before = packs(&bare);
    let script = write_fake_ssh(tmp.path()).expect("executable fake ssh");
    let transport = SshTransport::with_program(script.as_os_str());
    let url = format!("ssh://git@fakehost{}", bare.display());
    let mut conn = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("receive-pack connection");
    assert_eq!(conn.advertised_refs().len(), 61);
    let outcome = push_remote(
        &local.join(".git"),
        &mut *conn,
        &[spec_update(tip, "refs/heads/main", false)],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("push one commit");
    drop(conn);
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(rev_parse(&bare, "refs/heads/main"), tip);
    git(&bare, &["fsck", "--strict", "--no-dangling"]);

    let received: Vec<_> = packs(&bare)
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect();
    assert_eq!(received.len(), 1, "the push kept one pack");
    let verify = git(&bare, &["verify-pack", "-v", received[0].to_str().unwrap()]);
    let objects = verify
        .lines()
        .filter(|l| l.len() > 40 && l.as_bytes()[40] == b' ')
        .count();
    // `index-pack --fix-thin` appends the delta base the remote already had.
    assert_eq!(
        objects, 4,
        "commit, tree and a blob delta plus the appended base:\n{verify}"
    );
}
