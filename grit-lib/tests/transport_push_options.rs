//! Integration test for server-side push options over the wire
//! (`PushOptions::push_options`).
//!
//! When `git push --push-option <value>` is used, the client must negotiate the
//! receive-pack `push-options` capability and, after the ref-update command list,
//! send one `push-option <value>` pkt-line per option (terminated by a flush)
//! before the pack. `git-receive-pack` then exposes them to its hooks via
//! `GIT_PUSH_OPTION_COUNT` / `GIT_PUSH_OPTION_<n>`.
//!
//! We drive `push::push_remote` over a real `git daemon
//! --enable=receive-pack`, against a bare repo whose `hooks/pre-receive`
//! records `$GIT_PUSH_OPTION_COUNT` and each `GIT_PUSH_OPTION_<n>` to a file,
//! and assert the hook saw exactly the options we sent. The same fixture is also
//! exercised over the `ssh` transport (fake-ssh script) when available.
//!
//! A second case targets a server that does NOT advertise the capability
//! (`receive.advertisePushOptions` unset/false): pushing options there must fail
//! with the typed `Error::PushOptionsUnsupported`, before any ref is touched.
//!
//! The test skips gracefully (returns early) when `git`/`git daemon`/`sh` are
//! unavailable, a port cannot be bound, or the daemon refuses receive-pack — the
//! happy path is otherwise real end-to-end wire I/O cross-checked against a real
//! `git-receive-pack` and its hook.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push::push_remote;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
#[cfg(unix)]
use grit_lib::remote::transport::SshTransport;
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

/// Build a source repo with two commits on `main`.
fn build_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
}

/// Create a bare repo whose `pre-receive` hook records the push options it saw
/// to `<git_dir>/push-options-seen`: the first line is `count=<N>`, then one
/// `<n>=<value>` line per option. `advertise` toggles whether the repo
/// advertises the `push-options` capability.
fn init_bare_with_hook(bare: &Path, advertise: bool) {
    std::fs::create_dir_all(bare).unwrap();
    git(bare, &["init", "-q", "--bare", "."]);
    git(bare, &["config", "daemon.receivepack", "true"]);
    git(
        bare,
        &[
            "config",
            "receive.advertisePushOptions",
            if advertise { "true" } else { "false" },
        ],
    );

    let out_file = bare.join("push-options-seen");
    let hook = bare.join("hooks").join("pre-receive");
    // The hook drains stdin (the command list) so receive-pack proceeds, then
    // records the push-option environment for the test to inspect.
    let body = format!(
        r#"#!/bin/sh
cat >/dev/null
{{
  echo "count=${{GIT_PUSH_OPTION_COUNT:-0}}"
  i=0
  while [ "$i" -lt "${{GIT_PUSH_OPTION_COUNT:-0}}" ]; do
    eval "v=\${{GIT_PUSH_OPTION_$i}}"
    echo "$i=$v"
    i=$((i + 1))
  done
}} >'{out}'
exit 0
"#,
        out = out_file.display()
    );
    std::fs::write(&hook, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }
}

/// Read and parse the hook's recorded push options into `(count, vec![value..])`.
fn read_recorded_options(bare: &Path) -> Option<(usize, Vec<String>)> {
    let raw = std::fs::read_to_string(bare.join("push-options-seen")).ok()?;
    let mut count = 0usize;
    let mut values: Vec<(usize, String)> = Vec::new();
    for line in raw.lines() {
        if let Some(c) = line.strip_prefix("count=") {
            count = c.trim().parse().unwrap_or(0);
        } else if let Some((idx, val)) = line.split_once('=') {
            if let Ok(i) = idx.parse::<usize>() {
                values.push((i, val.to_owned()));
            }
        }
    }
    values.sort_by_key(|(i, _)| *i);
    Some((count, values.into_iter().map(|(_, v)| v).collect()))
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

fn spec(src: ObjectId, dst: &str) -> PushRefSpec {
    PushRefSpec {
        src: Some(src),
        dst: dst.to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
        ..Default::default()
    }
}

#[test]
fn push_options_are_delivered_to_receive_pack_hook_over_git_daemon() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Local source repo with two commits on main.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");

    // Bare remote under the daemon base path, advertising push-options, with a
    // pre-receive hook that records the options it sees.
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    init_bare_with_hook(&bare, true);

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

    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon receive-pack: {e}");
            return;
        }
    };
    // The advertisement must include `push-options` for the push to proceed; if a
    // local git is too old to advertise it even when configured, skip rather than
    // fail.
    if !conn.capabilities().iter().any(|c| c == "push-options") {
        eprintln!("SKIP: server did not advertise push-options (git too old?)");
        return;
    }

    let opts = PushOptions {
        push_options: vec!["ci.skip".to_owned(), "reviewer=alice".to_owned()],
        ..PushOptions::default()
    };
    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec(main_oid, "refs/heads/main")],
        &opts,
        &mut NoProgress,
    )
    .expect("push_remote with push-options over git daemon");
    drop(conn);

    assert_eq!(outcome.results.len(), 1);
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::Ok,
        "push should be accepted, got {:?} ({:?})",
        outcome.results[0].status,
        outcome.results[0].message
    );
    // The ref landed.
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("remote main written"),
        main_oid
    );

    // The hook recorded both options, in order.
    let (count, values) =
        read_recorded_options(&bare).expect("pre-receive hook recorded push options");
    assert_eq!(count, 2, "hook should see GIT_PUSH_OPTION_COUNT=2");
    assert_eq!(
        values,
        vec!["ci.skip".to_owned(), "reviewer=alice".to_owned()],
        "hook should see both push options in order"
    );
}

/// Write an executable fake-ssh script (as in `transport_ssh_push.rs`): ignore
/// host/options, rewrite the dashed transport name to the `git <service>`
/// subcommand, and exec it locally. Returns `None` if it cannot be created.
#[cfg(unix)]
fn write_fake_ssh(dir: &Path) -> Option<std::path::PathBuf> {
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

#[cfg(unix)]
#[test]
fn push_options_are_delivered_to_receive_pack_hook_over_ssh() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");

    // Bare remote advertising push-options, with the recording hook.
    let bare = tmp.path().join("remote.git");
    init_bare_with_hook(&bare, true);

    let Some(fake_ssh) = write_fake_ssh(tmp.path()) else {
        eprintln!("SKIP: could not create executable fake-ssh script");
        return;
    };

    let transport = SshTransport::with_program(fake_ssh.as_os_str());
    let abs_path = bare.to_str().expect("utf8 path");
    let url = format!("ssh://git@fakehost{abs_path}");

    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect over fake-ssh receive-pack: {e}");
            return;
        }
    };
    if !conn.capabilities().iter().any(|c| c == "push-options") {
        eprintln!("SKIP: server did not advertise push-options (git too old?)");
        return;
    }

    let opts = PushOptions {
        push_options: vec!["topic=feature".to_owned(), "notify".to_owned()],
        ..PushOptions::default()
    };
    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec(main_oid, "refs/heads/main")],
        &opts,
        &mut NoProgress,
    )
    .expect("push_remote with push-options over ssh");
    drop(conn);

    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::Ok,
        "push should be accepted, got {:?} ({:?})",
        outcome.results[0].status,
        outcome.results[0].message
    );
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("remote main written"),
        main_oid
    );

    let (count, values) =
        read_recorded_options(&bare).expect("pre-receive hook recorded push options");
    assert_eq!(count, 2, "hook should see GIT_PUSH_OPTION_COUNT=2");
    assert_eq!(
        values,
        vec!["topic=feature".to_owned(), "notify".to_owned()],
        "hook should see both push options in order"
    );
}

#[test]
fn pushing_options_to_server_without_capability_errors_typed() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");

    // Bare remote that does NOT advertise push-options.
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    init_bare_with_hook(&bare, false);

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
    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon receive-pack: {e}");
            return;
        }
    };
    assert!(
        !conn.capabilities().iter().any(|c| c == "push-options"),
        "server must not advertise push-options for this case"
    );

    let opts = PushOptions {
        push_options: vec!["ci.skip".to_owned()],
        ..PushOptions::default()
    };
    let err = push_remote(
        &local_git,
        &mut *conn,
        &[spec(main_oid, "refs/heads/main")],
        &opts,
        &mut NoProgress,
    )
    .expect_err("push with options to a server lacking the capability must fail");
    drop(conn);

    assert!(
        matches!(err, Error::PushOptionsUnsupported),
        "expected typed PushOptionsUnsupported, got {err:?}"
    );

    // Nothing was pushed: the ref must not exist on the remote.
    assert!(
        resolve_ref(&bare, "refs/heads/main").is_err(),
        "no ref should be created when the push aborts before sending"
    );
}
