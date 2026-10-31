//! Regression for issue #1657: clone/fetch/push against an empty bare repository
//! over the SSH transport when the server is system `git upload-pack` /
//! `git-receive-pack` (zero-oid `capabilities^{}` advertisement or v2 capability
//! block). Uses fake-ssh like `transport_ssh.rs` — no real sshd required.

#![cfg(unix)]

use grit_test_support::{git, rev_parse};
use std::path::Path;
use std::process::Command;

use grit_lib::prelude::Environment;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::push::push_remote;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::remote::transport::{ConnectOptions, Service, SshTransport, Transport};

fn init_empty_bare(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "--bare", "-b", "main", "."]);
}

/// Fake ssh that execs the remote Git service locally.
///
/// When `GRIT_TEST_SERVER_STDERR` is set in the environment, `git-receive-pack`
/// stderr is appended to that path so tests can assert the server did not log a
/// spurious "hung up unexpectedly" (issues #998 / #1657).
fn write_fake_ssh(dir: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-ssh.sh");
    let body = r#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
  "git-receive-pack "*)
    rp="git receive-pack ${cmd#git-receive-pack }"
    if [ -n "$GRIT_TEST_SERVER_STDERR" ]; then
      eval "$rp" 2>>"$GRIT_TEST_SERVER_STDERR"
      exit $?
    fi
    cmd="$rp"
    ;;
esac
eval "exec $cmd"
"#;
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

fn ssh_url(bare: &Path) -> String {
    format!("ssh://git@fakehost{}", bare.display())
}

fn connect_opts_v2() -> ConnectOptions {
    // Matches `Remote::fetch`, which negotiates protocol v2 for SSH remotes.
    ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    }
}

#[test]
fn ssh_connect_empty_bare_completes_handshake() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    init_empty_bare(&bare);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let conn = transport
        .connect(&ssh_url(&bare), Service::UploadPack, &connect_opts_v2())
        .expect("connect to empty bare over ssh");
    assert!(
        conn.advertised_refs().is_empty(),
        "unexpected refs: {:?}",
        conn.advertised_refs()
    );
    assert!(
        !conn.capabilities().is_empty(),
        "empty repository must still advertise server capabilities (handshake complete)"
    );
}

#[test]
fn fetch_empty_bare_over_ssh_is_noop() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    init_empty_bare(&bare);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let mut conn = transport
        .connect(&ssh_url(&bare), Service::UploadPack, &connect_opts_v2())
        .expect("connect");

    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::None;
        o
    };
    fetch_remote(
        &local.join(".git"),
        &mut *conn,
        &mut opts,
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("fetch against empty remote");
}

#[test]
fn clone_empty_bare_over_ssh_succeeds() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    init_empty_bare(&bare);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let url = ssh_url(&bare);
    let dest = tmp.path().join("clone");
    let mut opts = CloneOptions::new(url, dest.clone());
    opts.environment = Environment::from_vars(
        [(
            std::ffi::OsString::from("GIT_SSH"),
            fake_ssh.as_os_str().to_os_string(),
        )],
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
    );

    let outcome =
        clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone empty bare over ssh");

    assert!(outcome.branches_with_commits.is_empty());
    assert!(dest.join(".git").is_dir());
}

#[test]
fn push_to_empty_bare_over_ssh_without_hung_up_stderr() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    init_empty_bare(&bare);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let tip = rev_parse(&local, "HEAD");

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let server_stderr = tmp.path().join("receive-pack.stderr");
    std::env::set_var(
        "GRIT_TEST_SERVER_STDERR",
        server_stderr.to_str().expect("utf8 path"),
    );

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let mut conn = transport
        .connect(
            &ssh_url(&bare),
            Service::ReceivePack,
            &ConnectOptions::default(),
        )
        .expect("receive-pack connect");

    let spec = PushRefSpec {
        src: Some(tip),
        dst: "refs/heads/x".to_owned(),
        ..Default::default()
    };
    let outcome = push_remote(
        &local.join(".git"),
        &mut *conn,
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("push to empty bare");
    drop(conn);

    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);

    let server_log = std::fs::read_to_string(&server_stderr).unwrap_or_default();
    assert!(
        !server_log.contains("hung up unexpectedly"),
        "receive-pack stderr must not report hung up on a successful push: {server_log}"
    );
}
