//! Integration test for the `ssh` transport: `SshTransport` +
//! `fetch::fetch_remote`.
//!
//! There is no real ssh server here. Instead we use the standard trick from
//! Git's own test suite: point the ssh command at a tiny shell script (a
//! "fake ssh") that ignores the host argument and runs the remote command —
//! `git-upload-pack '<path>'` — *locally*. So the wire bytes are real
//! upload-pack output piped through the fake-ssh subprocess, exactly as the
//! `SshTransport` would drive a real `ssh`.
//!
//! We exercise both pluggability seams:
//!   * `SshTransport::with_shell_command(...)` (the `$GIT_SSH_COMMAND` shape,
//!     run via `sh -c`), and
//!   * `SshTransport::with_program(...)` (the `$GIT_SSH` shape, a bare program
//!     invoked with argv `<host> <remote-cmd>`),
//! and assert the fetched refs/objects match `git -C <source> rev-parse`.
//!
//! The test skips gracefully (returns early) on platforms without `sh`/`git`,
//! or where the script cannot be made executable — the happy path is otherwise
//! real end-to-end pkt-line I/O over a subprocess.

#![cfg(unix)]

use grit_test_support::{git, rev_parse};
use std::path::Path;
use std::process::Command;

use grit_lib::objects::odb::Odb;
use grit_lib::progress::NeverCancel;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{FetchOptions, TagMode, UpdateMode};
use grit_lib::remote::transport::{ConnectOptions, Service, SshTransport, Transport};

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// Build a source repo: two commits on `main`, a `topic` branch, an annotated tag.
fn build_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    git(dir, &["tag", "-a", "v1", "-m", "release one"]);
    git(dir, &["branch", "topic"]);
}

/// Write an executable fake-ssh script and return its path, or `None` if it
/// cannot be created/made executable on this platform.
///
/// The script is invoked as `<script> [<-p port>] <host> <remote-cmd>`, where
/// `<remote-cmd>` is `git-upload-pack '<path>'`. It ignores everything except
/// the last argument (the remote command), rewrites `git-upload-pack` to
/// `git upload-pack` (a subcommand, always available), and `eval`s it so the
/// shell-quoted path is parsed by the shell.
fn write_fake_ssh(dir: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-ssh.sh");
    // `$# - 1` is not portable in POSIX sh arithmetic for indexing, so we shift
    // off everything but the final argument (the remote command), which is what
    // every ssh invocation places last.
    let body = r#"#!/bin/sh
# Fake ssh: ignore host/options, run the remote command locally.
# The remote command is always the last argument.
cmd=
for cmd in "$@"; do :; done
# Rewrite the dashed transport name to the `git upload-pack` subcommand form.
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
  "git-receive-pack "*) cmd="git receive-pack ${cmd#git-receive-pack }" ;;
esac
eval "exec $cmd"
"#;
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

/// Run one fetch over the SSH transport against `source`, asserting refs and
/// objects. `transport` is constructed by the caller so both the
/// `with_shell_command` and `with_program` seams can be exercised.
fn run_fetch_and_assert(transport: &SshTransport, url: &str, source: &Path, local_root: &Path) {
    let main_oid = rev_parse(source, "refs/heads/main");
    let topic_oid = rev_parse(source, "refs/heads/topic");
    let tag_oid = rev_parse(source, "refs/tags/v1");

    std::fs::create_dir_all(local_root).unwrap();
    git(local_root, &["init", "-q", "-b", "main", "."]);
    let local_git = local_root.join(".git");

    let mut conn = match transport.connect(url, Service::UploadPack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => panic!("SshTransport::connect failed for {url}: {e}"),
    };

    // The advertisement should carry the source's heads (real upload-pack output).
    assert!(
        conn.advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == main_oid),
        "advertisement missing refs/heads/main = {}",
        main_oid.to_hex()
    );

    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_remote(
        &local_git,
        &mut *conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("fetch_remote over ssh");

    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch vs source");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch vs source");

    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1 written");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch vs source");

    // Objects landed and are readable in the local odb.
    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local odb after fetch",
            oid.to_hex()
        );
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // New head, default branch from HEAD symref.
    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check against git's view of the source.
    assert_eq!(
        got_main.to_hex(),
        git(source, &["rev-parse", "refs/heads/main"]).trim()
    );

    // The fetched pack re-indexes / fsck's clean.
    let fsck = Command::new("git")
        .current_dir(local_root)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after ssh fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn fetch_over_ssh_shell_command_lands_refs_and_objects() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    build_source(&source);
    // Ensure HEAD is a symref to main so the symref check is exercised.
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };

    // `with_shell_command` is the $GIT_SSH_COMMAND shape (run via `sh -c`).
    let transport = SshTransport::with_shell_command(fake_ssh.as_os_str());

    // scp-style URL `host:path`; the fake-ssh ignores the host and runs the
    // remote command on the absolute path.
    let abs_path = source.to_str().expect("utf8 path");
    let url = format!("fakehost:{abs_path}");

    run_fetch_and_assert(&transport, &url, &source, &tmp.path().join("local-shell"));
}

#[test]
fn fetch_over_ssh_v2_lands_refs_and_objects() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    build_source(&source);
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };

    // The transport sets `GIT_PROTOCOL=version=2` on the (fake) ssh process; the
    // fake-ssh inherits it and the local `git upload-pack` it execs switches to
    // protocol v2 — so the fetch exercises the v2 ls-refs + fetch path through
    // the SSH transport's streaming pkt-line channel.
    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let abs_path = source.to_str().expect("utf8 path");
    let url = format!("ssh://git@fakehost{abs_path}");

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let tag_oid = rev_parse(&source, "refs/tags/v1");

    let local_root = tmp.path().join("local-v2");
    std::fs::create_dir_all(&local_root).unwrap();
    git(&local_root, &["init", "-q", "-b", "main", "."]);
    let local_git = local_root.join(".git");

    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = transport
        .connect(&url, Service::UploadPack, &opts_v2)
        .expect("SshTransport::connect v2");

    // Proof v2 was negotiated: the server answered with a v2 capability block
    // (no refs on connect) and the protocol version is 2.
    if conn.protocol_version() != 2 {
        // The local `git upload-pack` did not honor GIT_PROTOCOL (very old git);
        // skip rather than fail, since the v2 path cannot be exercised here.
        eprintln!(
            "SKIP: server negotiated v{} (GIT_PROTOCOL not honored by upload-pack)",
            conn.protocol_version()
        );
        return;
    }
    assert!(
        conn.advertised_refs().is_empty(),
        "v2 connection must advertise no refs on connect"
    );
    assert!(
        conn.capabilities()
            .iter()
            .any(|c| c == "ls-refs" || c.starts_with("ls-refs=") || c.starts_with("fetch=")),
        "v2 capability block missing ls-refs/fetch: {:?}",
        conn.capabilities()
    );

    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_remote(
        &local_git,
        &mut *conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("v2 fetch over ssh");

    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_main, main_oid);
    assert_eq!(got_topic, topic_oid);
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1");
    assert_eq!(got_tag, tag_oid);
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, tag_oid] {
        assert!(local_odb.exists(&oid), "object {} missing", oid.to_hex());
    }

    let fsck = Command::new("git")
        .current_dir(&local_root)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after v2 ssh fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn fetch_over_ssh_program_lands_refs_and_objects() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    build_source(&source);
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };

    // `with_program` is the $GIT_SSH shape: the script is invoked directly with
    // argv `<host> <remote-cmd>` (no shell wrapping by the transport).
    let transport = SshTransport::with_program(fake_ssh.as_os_str());

    // An `ssh://` URL with a user and an absolute path on the remote.
    let abs_path = source.to_str().expect("utf8 path");
    let url = format!("ssh://git@fakehost{abs_path}");

    run_fetch_and_assert(&transport, &url, &source, &tmp.path().join("local-prog"));
}

#[test]
fn fake_ssh_inherits_git_protocol_env_for_v2_upload_pack() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let probe = tmp.path().join("git-protocol.txt");
    let script = tmp.path().join("probe-ssh.sh");
    let body = format!(
        r#"#!/bin/sh
if [ -n "$GIT_PROTOCOL" ]; then
  printf '%s' "$GIT_PROTOCOL" > {probe}
fi
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${{cmd#git-upload-pack }}" ;;
esac
eval "exec $cmd"
"#,
        probe = shell_escape_path(&probe)
    );
    write_executable(&script, &body);

    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    build_source(&source);

    let transport = SshTransport::with_program(&script);
    let url = format!("fakehost:{}", source.to_str().expect("utf8 path"));
    let opts = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let conn = transport
        .connect(&url, Service::UploadPack, &opts)
        .expect("connect");
    let _ = conn.protocol_version();
    drop(conn);

    let recorded = std::fs::read_to_string(&probe).unwrap_or_default();
    assert_eq!(
        recorded, "version=2",
        "ssh child must inherit GIT_PROTOCOL=version=2 for upload-pack v2"
    );
}

fn shell_escape_path(path: &Path) -> String {
    path.to_str()
        .map(|s| format!("'{s}'"))
        .unwrap_or_else(|| "'?'".to_owned())
}

fn write_executable(script: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(script, body).expect("write script");
    let mut perms = std::fs::metadata(script).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(script, perms).expect("chmod");
}
