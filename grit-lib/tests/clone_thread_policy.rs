//! Clone must propagate global `--threads` policy through fetch pack ingest.

#![cfg(feature = "http-ureq")]

use std::ffi::OsString;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::environment::Environment;
use grit_lib::objects::index_pack::{
    last_resolved_ingest_threads_for_tests, set_ingest_thread_recording_for_tests,
};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::DefaultHttpClientFactory;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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

fn spawn_server(server_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

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

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn http_clone_honors_thread_policy_over_pack_config_for_ingest() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: grit-http-server binary not found");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("a.txt"), "one\n").unwrap();
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "c1"]);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("repo.git");
    git(
        &work,
        &["clone", "-q", "--bare", ".", source.to_str().expect("utf8")],
    );

    let port = {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        l.local_addr().expect("addr").port()
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: server not ready");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");
    let dest = tmp.path().join("clone");
    let policy = ThreadPolicy::with_threads_override(Some(1));
    let environment = Environment::from_vars(
        [
            (OsString::from("GIT_CONFIG_COUNT"), OsString::from("1")),
            (
                OsString::from("GIT_CONFIG_KEY_0"),
                OsString::from("pack.threads"),
            ),
            (OsString::from("GIT_CONFIG_VALUE_0"), OsString::from("2")),
        ],
        dest.clone(),
    );

    set_ingest_thread_recording_for_tests(true);
    let outcome = clone_repo(
        CloneOptions {
            environment,
            thread_policy: policy,
            ..CloneOptions::new(url, dest)
        },
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
        Some(&DefaultHttpClientFactory::new()),
    )
    .expect("http clone");

    assert_eq!(outcome.repo.thread_policy().threads_override, Some(1));

    let recorded = last_resolved_ingest_threads_for_tests()
        .expect("clone must run index-pack ingest while recording");
    set_ingest_thread_recording_for_tests(false);
    assert_eq!(
        recorded, 1,
        "received-pack ingest must honor clone thread policy (1) over pack.threads (2)"
    );

    let pack_dir = outcome.repo.git_dir().join("objects/pack");
    assert!(
        pack_dir
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false),
        "http clone must install a pack (exercises index-pack ingest)"
    );
}
