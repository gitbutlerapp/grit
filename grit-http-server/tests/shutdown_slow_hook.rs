//! Graceful shutdown must not wait on a slow receive-pack hook past the grace period.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use grit_http_server::bundles::BundleSettings;
use grit_http_server::server::{ServeConfig, Server, Shutdown};
use grit_http_server::store::{Access, AuthStore, HashedPassword, Token};
use tokio::sync::oneshot;

fn git_output(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git")
}

fn fixture(root: &Path, pid_file: &Path) -> std::path::PathBuf {
    let seed = root.join("seed");
    std::fs::create_dir(&seed).expect("mkdir seed");
    assert!(git_output(&seed, &["init", "-q", "-b", "main"])
        .status
        .success());
    std::fs::write(seed.join("README"), "hello\n").expect("write");
    assert!(git_output(&seed, &["add", "README"]).status.success());
    assert!(git_output(&seed, &["commit", "-q", "-m", "init"])
        .status
        .success());

    let repos = root.join("repos");
    std::fs::create_dir(&repos).expect("mkdir repos");
    let bare = repos.join("project.git");
    assert!(git_output(
        root,
        &["clone", "-q", "--bare", "seed", "repos/project.git"]
    )
    .status
    .success());

    std::fs::create_dir_all(bare.join("hooks")).expect("hooks dir");
    let hook = bare.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >/dev/null\necho $$ >{}\nsleep 120\n",
            pid_file.display()
        ),
    )
    .expect("hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let database = root.join("serve.db");
    let mut store = AuthStore::open(&database).expect("open store");
    let password = HashedPassword::new("admin password").expect("hash");
    let admin = store
        .setup("admin@example.com", &password, false)
        .expect("setup");
    store
        .grant(&admin, "project", Access::Write)
        .expect("grant");
    store
        .add_token(&admin, &Token::from_secret("grit_admin_token"))
        .expect("token");
    database
}

#[test]
fn server_run_returns_after_grace_while_pre_receive_sleeps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pid_file = tmp.path().join("hook.pid");
    let database = fixture(tmp.path(), &pid_file);

    let (trigger_tx, trigger_rx) = oneshot::channel();
    let grace = Duration::from_millis(350);
    let server = Server::bind(
        ServeConfig {
            root: tmp.path().join("repos"),
            database,
            repository_options: grit_protocol::repository_options_from_process(),
            bundles: BundleSettings {
                enabled: false,
                ..BundleSettings::default()
            },
        },
        "127.0.0.1:0",
    )
    .expect("bind");
    let addr = server.local_addr().expect("addr");
    let started = Instant::now();
    let server_handle = std::thread::spawn(move || {
        server
            .run(Shutdown::triggered(grace, trigger_rx))
            .expect("serve");
    });

    let client = tmp.path().join("client");
    assert!(git_output(
        tmp.path(),
        &[
            "clone",
            "-q",
            &format!("http://user:grit_admin_token@{addr}/project"),
            "client"
        ]
    )
    .status
    .success());
    std::fs::write(client.join("f"), "x\n").expect("write");
    assert!(git_output(&client, &["add", "f"]).status.success());
    assert!(git_output(&client, &["commit", "-q", "-m", "c"])
        .status
        .success());

    let push_url = format!("http://user:grit_admin_token@{addr}/project");
    let push_handle = std::thread::spawn({
        let push_url = push_url.clone();
        move || {
            let _ = git_output(&client, &["push", &push_url, "HEAD:refs/heads/topic"]);
        }
    });

    let hook_deadline = Instant::now() + Duration::from_secs(15);
    while !pid_file.is_file() {
        assert!(Instant::now() < hook_deadline, "hook never started");
        std::thread::sleep(Duration::from_millis(30));
    }
    let hook_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("pid")
        .trim()
        .parse()
        .expect("parse pid");

    let _ = trigger_tx.send(());
    server_handle
        .join()
        .expect("server thread join should succeed");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "server.run should finish soon after grace, took {elapsed:?}"
    );

    push_handle.join().expect("push thread");
    let hook_gone = Command::new("kill")
        .args(["-0", &hook_pid.to_string()])
        .status()
        .expect("kill -0")
        .code()
        != Some(0);
    assert!(
        hook_gone,
        "hook {hook_pid} should be terminated after shutdown"
    );
}
