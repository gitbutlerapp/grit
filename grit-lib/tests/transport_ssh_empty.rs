//! Regression for issues #1657 and #2340: clone/fetch/push against an empty bare
//! repository over the SSH transport when the server is system `git upload-pack` /
//! `git-receive-pack` (zero-oid `capabilities^{}` advertisement or v2 capability
//! block), or when the server sends only a flush pkt-line with no refs (Apple Git
//! 2.39 on empty bare repos). Uses fake-ssh like `transport_ssh.rs` — no real sshd
//! required.

#![cfg(unix)]

use grit_test_support::{git, rev_parse};
use std::path::Path;
use std::process::Command;

use grit_lib::error::{Error, RemoteUnreadableReason};
use grit_lib::prelude::Environment;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::remote::transport::{ConnectOptions, Connection, Service, SshTransport, Transport};
use grit_lib::remote::{ListRefsOptions, Remote};

/// Restore an environment variable on drop so parallel tests do not leak stubs.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

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
    ConnectOptions::with_protocol_version(2)
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

    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::None;
    grit_lib::repo::Repository::open(
        &local.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&mut opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Connection(&mut *conn),
        &mut opts,
        &mut NoProgress,
        &grit_lib::progress::CancelToken::never(),
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

    let outcome = clone(opts, &mut NoProgress, &CancelToken::never(), None)
        .expect("clone empty bare over ssh");

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
    let _server_stderr_env = EnvVarGuard::set(
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
    let outcome = grit_lib::repo::Repository::open(
        &local.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Connection(&mut *conn),
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::CancelToken::never(),
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

/// Fake ssh whose upload-pack speaks Apple Git 2.39's empty-bare advertisement: `0000` only.
fn write_fake_ssh_flush_only_upload_pack(dir: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-ssh-apple-upload-pack.sh");
    let body = r#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*)
    printf '0000'
    if [ -n "$GRIT_TEST_UPLOAD_PACK_STDIN" ]; then
      cat >"$GRIT_TEST_UPLOAD_PACK_STDIN"
    fi
    cat >/dev/null
    exit 0
    ;;
  "git-receive-pack "*)
    rp="git receive-pack ${cmd#git-receive-pack }"
    if [ -n "$GRIT_TEST_SERVER_STDERR" ]; then
      eval "$rp" 2>>"$GRIT_TEST_SERVER_STDERR"
      exit $?
    fi
    eval "exec $rp"
    ;;
esac
exec sh -c "echo unexpected: $cmd >&2; exit 1"
"#;
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

fn connect_opts_v0() -> ConnectOptions {
    ConnectOptions::with_protocol_version(0)
}

#[test]
fn ssh_connect_flush_only_empty_bare_v0_completes_handshake() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(fake_ssh) = write_fake_ssh_flush_only_upload_pack(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let url = format!(
        "ssh://git@fakehost{}",
        tmp.path().join("ignored.git").display()
    );
    let conn = transport
        .connect(&url, Service::UploadPack, &connect_opts_v0())
        .expect("connect to flush-only empty advertisement");
    assert!(conn.advertised_refs().is_empty());
    assert!(conn.capabilities().is_empty());
    assert!(Connection::saw_ref_advertisement_flush(&*conn));
}

#[test]
fn clone_empty_bare_over_ssh_flush_only_v0_succeeds() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(fake_ssh) = write_fake_ssh_flush_only_upload_pack(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let url = format!(
        "ssh://git@fakehost{}",
        tmp.path().join("remote.git").display()
    );
    let dest = tmp.path().join("clone");
    let mut opts = CloneOptions::new(url, dest.clone());
    opts.environment = Environment::from_vars(
        [(
            std::ffi::OsString::from("GIT_SSH"),
            fake_ssh.as_os_str().to_os_string(),
        )],
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
    );

    let outcome = clone(opts, &mut NoProgress, &CancelToken::never(), None)
        .expect("clone flush-only empty over ssh");
    assert!(outcome.branches_with_commits.is_empty());
    assert!(dest.join(".git").is_dir());
}

#[test]
fn list_refs_empty_bare_over_ssh_flush_only_v0_succeeds() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(fake_ssh) = write_fake_ssh_flush_only_upload_pack(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let stdin_capture = tmp.path().join("upload-pack.stdin");
    let _stdin_env = EnvVarGuard::set(
        "GRIT_TEST_UPLOAD_PACK_STDIN",
        stdin_capture.to_str().expect("utf8"),
    );
    let _git_ssh = EnvVarGuard::set("GIT_SSH", fake_ssh.as_os_str());
    let _git_global = EnvVarGuard::set("GIT_CONFIG_GLOBAL", "/dev/null");
    let _git_system = EnvVarGuard::set("GIT_CONFIG_SYSTEM", "/dev/null");

    let url = format!(
        "ssh://git@fakehost{}",
        tmp.path().join("remote.git").display()
    );
    let remote = Remote::from_url(&url).expect("remote url");
    let refs = remote
        .list_refs(
            None,
            &ListRefsOptions {
                protocol_version: Some(0),
                ..Default::default()
            },
            None,
        )
        .expect("list refs");
    assert!(refs.is_empty());
    let client_stdin = std::fs::read(&stdin_capture).unwrap_or_default();
    assert!(
        client_stdin.ends_with(b"0000"),
        "list_refs must flush upload-pack before close, got {:?}",
        String::from_utf8_lossy(&client_stdin)
    );
}

#[test]
fn fetch_empty_bare_over_ssh_flush_only_v0_is_noop() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(fake_ssh) = write_fake_ssh_flush_only_upload_pack(tmp.path()) else {
        eprintln!("SKIP: fake-ssh script");
        return;
    };

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let url = format!(
        "ssh://git@fakehost{}",
        tmp.path().join("remote.git").display()
    );
    let mut conn = transport
        .connect(&url, Service::UploadPack, &connect_opts_v0())
        .expect("connect");

    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::None;
    grit_lib::repo::Repository::open(
        &local.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&mut opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Connection(&mut *conn),
        &mut opts,
        &mut NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("fetch against flush-only empty remote");
}

#[test]
fn ssh_connect_delim_only_advertisement_is_remote_unreadable() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let script = tmp.path().join("fake-ssh-delim.sh");
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(
        &script,
        b"#!/bin/sh\ncmd=\nfor cmd in \"$@\"; do :; done\ncase \"$cmd\" in\n  \"git-upload-pack \"*) printf '0001'; cat >/dev/null; exit 0;;\nesac\nexit 1\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let transport = SshTransport::with_program(&script);
    let url = format!(
        "ssh://git@fakehost{}",
        tmp.path().join("ignored.git").display()
    );
    match transport.connect(&url, Service::UploadPack, &connect_opts_v0()) {
        Err(Error::Remote(grit_lib::remote::RemoteError::Unreadable {
            reason: RemoteUnreadableReason::NoHandshake,
            ..
        })) => {}
        Ok(_) => panic!("delim-only peer must not complete handshake"),
        Err(other) => panic!("delim-only peer must not complete handshake: {other}"),
    }
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git; run with --features full-tests"
)]
#[test]
fn ssh_connect_system_git_empty_bare_v0_then_fsck() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no system git");
        return;
    }
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

    let dest = tmp.path().join("clone");
    let mut opts = CloneOptions::new(ssh_url(&bare), dest.clone());
    opts.environment = Environment::from_vars(
        [(
            std::ffi::OsString::from("GIT_SSH"),
            fake_ssh.as_os_str().to_os_string(),
        )],
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
    );
    clone(opts, &mut NoProgress, &CancelToken::never(), None).expect("clone");
    let fsck = Command::new("git")
        .args(["-C", dest.to_str().expect("utf8"), "fsck"])
        .status()
        .expect("git fsck");
    assert!(fsck.success(), "cloned repo must fsck clean");
}
