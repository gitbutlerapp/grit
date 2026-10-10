//! Receive-pack server hooks, quarantine, and side-band hook output.

use std::io::Write;
#[cfg(unix)]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};

use grit_lib::command_runner::{CommandRunner, RecordedResponse, RecordingRunner};
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::serve::{receive_pack, ProtocolVersion, ReceiveOutcome, ReceivePolicy, ServeOptions};

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
    String::from_utf8(out.stdout).expect("utf8")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

fn write_executable_hook(path: &Path, body: &str) {
    std::fs::write(path, body).expect("hook body");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }
}

fn pack_for_commit(source: &Path, oid: ObjectId) -> Vec<u8> {
    let hex = oid.to_hex();
    let mut child = Command::new("git")
        .current_dir(source)
        .args(["pack-objects", "--revs", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("pack-objects spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("{hex}\n").as_bytes())
        .expect("write revs");
    let out = child.wait_with_output().expect("pack-objects wait");
    assert!(out.status.success(), "pack-objects failed");
    out.stdout
}

fn pkt_line(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    format!("{:04x}{}", bytes.len() + 4, payload).into_bytes()
}

fn push_body(old: Option<ObjectId>, new: ObjectId, refname: &str, pack: &[u8]) -> Vec<u8> {
    let zero = "0000000000000000000000000000000000000000";
    let old_s = old.map(|o| o.to_hex()).unwrap_or_else(|| zero.to_owned());
    let mut body = pkt_line(&format!(
        "{old_s} {} {refname}\0report-status side-band-64k\n",
        new.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

fn serve_push(repo: &Repository, body: &[u8]) -> (ReceiveOutcome, Vec<u8>) {
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        advertise_refs: false,
        agent: "grit-test".to_owned(),
        hidden_refs: Vec::new(),
    };
    let policy = ReceivePolicy::default();
    let mut output = Vec::new();
    let outcome = receive_pack(
        repo,
        &mut std::io::Cursor::new(body),
        &mut output,
        &opts,
        &policy,
    )
    .expect("receive_pack");
    (outcome, output)
}

fn open_bare_with_runner(path: &Path, runner: Arc<dyn CommandRunner>) -> Repository {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "--bare", "."]);
    let git_dir = path.to_path_buf();
    let mut env = Environment::empty();
    env.cwd = path.to_path_buf();
    let opts = RepositoryOptions::with_environment(env).with_command_runner(runner);
    Repository::open_with(&opts, &git_dir, None).expect("open bare")
}

fn count_pack_files(objects: &Path) -> usize {
    objects
        .join("pack")
        .read_dir()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn recording_runner_pre_receive_sees_push_options_and_ref_stdin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    let runner = RecordingRunner::always_success();
    runner.push_response(RecordedResponse::Output {
        stdout: Vec::new(),
        code: 0,
    });

    let repo = open_bare_with_runner(&bare, runner.clone());
    std::fs::write(
        bare.join("config"),
        "[receive]\n\tadvertisePushOptions = true\n[hook \"pr\"]\n\tcommand = cat\n\tevent = pre-receive\n",
    )
    .expect("config");

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "x\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);

    let mut body = pkt_line(&format!(
        "{} {} refs/heads/main\0report-status push-options\n",
        "0000000000000000000000000000000000000000",
        oid.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("push-option ci.skip\n"));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let (outcome, _out) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);
    assert_eq!(outcome.push_options, vec!["ci.skip".to_owned()]);

    let specs = runner.specs();
    assert!(!specs.is_empty(), "expected hook spawn");
    let env = &specs[0].env.set;
    assert!(env
        .iter()
        .any(|(k, v)| k == "GIT_PUSH_OPTION_COUNT" && v == "1"));
    assert!(env
        .iter()
        .any(|(k, v)| k == "GIT_PUSH_OPTION_0" && v == "ci.skip"));
    assert!(env.iter().any(|(k, _)| k == "GIT_QUARANTINE_PATH"));
}

#[test]
fn pre_receive_reject_leaves_main_object_store_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    let hook = bare.join("hooks").join("pre-receive");
    write_executable_hook(&hook, "#!/bin/sh\ncat >/dev/null\nexit 1\n");
    let repo = open_bare_with_runner(&bare, grit_lib::command_runner::system_command_runner());

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "payload\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);
    let body = push_body(None, oid, "refs/heads/main", &pack);

    let packs_before = count_pack_files(&bare.join("objects"));
    let (outcome, _) = serve_push(&repo, &body);
    assert_eq!(outcome.updates.len(), 1);
    assert_eq!(
        outcome.updates[0].error.as_deref(),
        Some("pre-receive hook declined")
    );
    assert!(resolve_ref(&bare, "refs/heads/main").is_err());
    assert_eq!(count_pack_files(&bare.join("objects")), packs_before);
}

#[test]
fn update_reject_only_blocks_one_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    write_executable_hook(
        &bare.join("hooks").join("pre-receive"),
        "#!/bin/sh\ncat >/dev/null\nexit 0\n",
    );
    write_executable_hook(
        &bare.join("hooks").join("update"),
        "#!/bin/sh\nif [ \"$1\" = \"refs/heads/block\" ]; then exit 1; fi\nexit 0\n",
    );
    let repo = open_bare_with_runner(&bare, grit_lib::command_runner::system_command_runner());

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("a"), "1\n").unwrap();
    git(&source, &["add", "a"]);
    git(&source, &["commit", "-q", "-m", "c1"]);
    std::fs::write(source.join("b"), "2\n").unwrap();
    git(&source, &["add", "b"]);
    git(&source, &["commit", "-q", "-m", "c2"]);
    let main = rev_parse(&source, "refs/heads/main");
    git(&source, &["branch", "block"]);
    git(&source, &["checkout", "-q", "main"]);
    let pack = pack_for_commit(&source, main);

    let zero = "0000000000000000000000000000000000000000";
    let mut body = pkt_line(&format!(
        "{zero} {} refs/heads/main\0report-status\n",
        main.to_hex()
    ));
    body.extend_from_slice(&pkt_line(&format!(
        "{zero} {} refs/heads/block\0report-status\n",
        main.to_hex()
    )));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let (outcome, _) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none());
    assert_eq!(outcome.updates[1].error.as_deref(), Some("hook declined"));
    assert_eq!(resolve_ref(&bare, "refs/heads/main").unwrap(), main);
    assert!(resolve_ref(&bare, "refs/heads/block").is_err());
}

#[test]
fn post_receive_sees_applied_updates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    let log = bare.join("post-receive-log");
    write_executable_hook(
        &bare.join("hooks").join("post-receive"),
        &format!("#!/bin/sh\ncat >'{log}'\n", log = log.display()),
    );
    let repo = open_bare_with_runner(&bare, grit_lib::command_runner::system_command_runner());

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "z\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);
    let body = push_body(None, oid, "refs/heads/main", &pack);

    let (outcome, _) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none());
    let recorded = std::fs::read_to_string(&log).expect("post-receive log");
    assert!(recorded.contains(&oid.to_hex()));
    assert!(recorded.contains("refs/heads/main"));
}

#[cfg(unix)]
#[test]
fn git_push_shows_hook_message_over_grit_http_server() {
    let server_bin = find_binary("grit-http-server");
    let Some(server_bin) = server_bin else {
        eprintln!("SKIP: grit-http-server not built");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("hook.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    write_executable_hook(
        &bare.join("hooks").join("pre-receive"),
        "#!/bin/sh\ncat >/dev/null\necho hook-banner-from-server >&2\nexit 0\n",
    );

    let port = free_port().expect("port");
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: server not ready");
        return;
    }

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("x"), "1\n").unwrap();
    git(&source, &["add", "x"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    git(
        &source,
        &[
            "remote",
            "add",
            "grit",
            &format!("http://127.0.0.1:{port}/hook.git"),
        ],
    );
    let push = Command::new("git")
        .current_dir(&source)
        .args(["push", "-q", "grit", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git push");
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(push.status.success(), "push failed: {stderr}");
    assert!(
        stderr.contains("remote:") && stderr.contains("hook-banner-from-server"),
        "expected hook stderr as remote: lines, got:\n{stderr}"
    );
}

#[cfg(unix)]
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

#[cfg(unix)]
fn free_port() -> Option<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    Some(l.local_addr().ok()?.port())
}

#[cfg(unix)]
fn wait_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(unix)]
struct ServerGuard(Child);
#[cfg(unix)]
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
