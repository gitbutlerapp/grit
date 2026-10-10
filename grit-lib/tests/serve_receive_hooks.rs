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
use grit_lib::objects::{HashAlgo, ObjectId};
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
    body.extend_from_slice(&pkt_line("ci.skip\n"));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let (outcome, _out) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);
    assert_eq!(outcome.push_options, vec!["ci.skip".to_owned()]);

    let specs = runner.specs();
    assert_eq!(specs.len(), 1, "expected only pre-receive configured hook");
    let env = &specs[0].env.set;
    assert!(env
        .iter()
        .any(|(k, v)| k == "GIT_PUSH_OPTION_COUNT" && v == "1"));
    assert!(env
        .iter()
        .any(|(k, v)| k == "GIT_PUSH_OPTION_0" && v == "ci.skip"));
    assert!(env.iter().any(|(k, _)| k == "GIT_QUARANTINE_PATH"));
    let stdins = runner.stdin_payloads();
    assert_eq!(stdins.len(), 1);
    let stdin = String::from_utf8(stdins[0].clone()).expect("utf8 stdin");
    assert!(stdin.contains(&oid.to_hex()));
    assert!(stdin.contains("refs/heads/main"));
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
    assert!(
        !repo.odb.exists(&oid),
        "rejected push must not install objects in the main ODB"
    );
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
fn all_update_rejects_skip_post_receive_and_post_update() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    write_executable_hook(
        &bare.join("hooks").join("pre-receive"),
        "#!/bin/sh\ncat >/dev/null\nexit 0\n",
    );
    write_executable_hook(&bare.join("hooks").join("update"), "#!/bin/sh\nexit 1\n");
    let post_log = bare.join("post-receive-log");
    write_executable_hook(
        &bare.join("hooks").join("post-receive"),
        &format!("#!/bin/sh\ncat >'{log}'\n", log = post_log.display()),
    );
    let post_update_log = bare.join("post-update-log");
    write_executable_hook(
        &bare.join("hooks").join("post-update"),
        &format!(
            "#!/bin/sh\necho ran >'{log}'\n",
            log = post_update_log.display()
        ),
    );
    let repo = open_bare_with_runner(&bare, grit_lib::command_runner::system_command_runner());

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "x\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);
    let body = push_body(None, oid, "refs/heads/main", &pack);

    let (outcome, _) = serve_push(&repo, &body);
    assert_eq!(outcome.updates[0].error.as_deref(), Some("hook declined"));
    assert!(
        outcome
            .hooks
            .iter()
            .all(|h| h.name != "post-receive" && h.name != "post-update"),
        "post hooks must not run when no ref applied: {:?}",
        outcome.hooks
    );
    assert!(!post_log.exists());
    assert!(!post_update_log.exists());
}

#[test]
fn recording_runner_update_and_post_update_omit_push_options_and_quarantine() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    let runner = RecordingRunner::always_success();
    for _ in 0..4 {
        runner.push_response(RecordedResponse::Output {
            stdout: Vec::new(),
            code: 0,
        });
    }

    let repo = open_bare_with_runner(&bare, runner.clone());
    std::fs::write(
        bare.join("config"),
        "[receive]\n\tadvertisePushOptions = true\n\
         [hook \"pr\"]\n\tcommand = cat\n\tevent = pre-receive\n\
         [hook \"up\"]\n\tcommand = cat\n\tevent = update\n\
         [hook \"post\"]\n\tcommand = cat\n\tevent = post-receive\n\
         [hook \"pu\"]\n\tcommand = true\n\tevent = post-update\n",
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
    body.extend_from_slice(&pkt_line("ci.skip\n"));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let (outcome, _) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);

    let specs = runner.specs();
    assert_eq!(specs.len(), 4, "pre, update, post-receive, post-update");
    let pre = &specs[0].env.set;
    assert!(pre.iter().any(|(k, _)| k == "GIT_PUSH_OPTION_COUNT"));
    assert!(pre.iter().any(|(k, _)| k == "GIT_QUARANTINE_PATH"));
    let update = &specs[1].env.set;
    assert!(
        !update.iter().any(|(k, _)| k == "GIT_PUSH_OPTION_COUNT"),
        "update hook must not receive push options"
    );
    assert!(
        !update.iter().any(|(k, _)| k == "GIT_QUARANTINE_PATH"),
        "update hook runs after migrate"
    );
    let post = &specs[2].env.set;
    assert!(post.iter().any(|(k, _)| k == "GIT_PUSH_OPTION_COUNT"));
    assert!(
        !post.iter().any(|(k, _)| k == "GIT_QUARANTINE_PATH"),
        "post-receive runs after migrate"
    );
    let post_update = &specs[3].env.set;
    assert!(
        !post_update
            .iter()
            .any(|(k, _)| k == "GIT_PUSH_OPTION_COUNT"),
        "post-update must not receive push options"
    );
    assert!(
        !post_update.iter().any(|(k, _)| k == "GIT_QUARANTINE_PATH"),
        "post-update must not receive quarantine env"
    );
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

#[test]
fn update_hook_runs_without_quarantine_env_after_migrate() {
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
        "#!/bin/sh\nif [ -n \"${GIT_QUARANTINE_PATH:-}\" ]; then exit 1; fi\necho blob | git hash-object -w --stdin >/dev/null\nexit 0\n",
    );
    let repo = open_bare_with_runner(&bare, grit_lib::command_runner::system_command_runner());

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "hook-write\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);
    let body = push_body(None, oid, "refs/heads/main", &pack);

    let (outcome, _) = serve_push(&repo, &body);
    assert!(
        outcome.updates[0].error.is_none(),
        "update hook should succeed after migrate: {:?}",
        outcome.updates
    );
}

fn git_sha256_bare_init_supported() -> bool {
    let Ok(probe) = tempfile::tempdir() else {
        return false;
    };
    Command::new("git")
        .current_dir(probe.path())
        .args(["init", "-q", "--object-format=sha256", "--bare", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn sha256_pre_receive_stdin_uses_full_width_null_oid() {
    if !git_sha256_bare_init_supported() {
        eprintln!("SKIP: git cannot create sha256 repos");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(
        &bare,
        &["init", "-q", "--bare", "--object-format=sha256", "."],
    );
    let log = bare.join("pre-receive-log");
    write_executable_hook(
        &bare.join("hooks").join("pre-receive"),
        &format!("#!/bin/sh\ncat >'{log}'\n", log = log.display()),
    );
    let repo = open_bare_with_runner(&bare, grit_lib::command_runner::system_command_runner());

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(
        &source,
        &["init", "-q", "-b", "main", "--object-format=sha256", "."],
    );
    std::fs::write(source.join("f"), "s\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    assert_eq!(oid.algo(), HashAlgo::Sha256);
    let pack = pack_for_commit(&source, oid);
    let null = ObjectId::null(HashAlgo::Sha256).to_hex();
    let mut body = pkt_line(&format!(
        "{null} {} refs/heads/main\0report-status\n",
        oid.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let (outcome, _) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);
    let stdin = std::fs::read_to_string(&log).expect("pre-receive stdin");
    assert!(stdin.contains(&null));
    assert!(stdin.contains(&oid.to_hex()));
}

#[test]
fn report_status_matches_git_receive_pack_for_same_push() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "rs\n").unwrap();
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);
    let zero = ObjectId::null(HashAlgo::Sha1).to_hex();
    let mut body = pkt_line(&format!(
        "{zero} {} refs/heads/main\0report-status\n",
        oid.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let git_out = Command::new("git")
        .current_dir(&bare)
        .args(["receive-pack", "--stateless-rpc", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(&body)
                .map_err(std::io::Error::other)?;
            child.wait_with_output()
        })
        .expect("git receive-pack");
    assert!(
        git_out.status.success(),
        "git receive-pack failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    let bare2 = tmp.path().join("bare2");
    std::fs::create_dir_all(&bare2).unwrap();
    git(&bare2, &["init", "-q", "--bare", "."]);
    let repo = open_bare_with_runner(&bare2, grit_lib::command_runner::system_command_runner());
    let (outcome, grit_out) = serve_push(&repo, &body);
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);

    fn normalize_report(raw: &[u8]) -> String {
        String::from_utf8_lossy(raw)
            .lines()
            .filter(|l| !l.is_empty() && *l != "0000")
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
    }
    assert_eq!(
        normalize_report(&git_out.stdout),
        normalize_report(&grit_out),
        "report-status pkt-lines should match git receive-pack"
    );
}

#[cfg(unix)]
#[test]
fn git_push_push_option_value_over_grit_http_server() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: grit-http-server not built");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("opts.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["config", "receive.advertisePushOptions", "true"]);
    let seen = bare.join("push-options-seen");
    write_executable_hook(
        &bare.join("hooks").join("pre-receive"),
        &format!(
            "#!/bin/sh\ncat >/dev/null\necho \"count=${{GIT_PUSH_OPTION_COUNT:-0}}\" >'{seen}'\necho \"0=${{GIT_PUSH_OPTION_0:-}}\" >>'{seen}'\n",
            seen = seen.display()
        ),
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
        .expect("spawn");
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
            &format!("http://127.0.0.1:{port}/opts.git"),
        ],
    );
    let push = Command::new("git")
        .current_dir(&source)
        .args(["push", "-q", "--push-option=ci.skip", "grit", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git push");
    assert!(
        push.status.success(),
        "git push with push-option failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    let recorded = std::fs::read_to_string(&seen).expect("hook log");
    assert!(recorded.contains("count=1"));
    assert!(recorded.contains("0=ci.skip"));
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
