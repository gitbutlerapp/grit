//! Shallow fetch `--depth`, `--deepen`, and `--unshallow` vs system `git` (file:// and HTTP).

use grit_test_support::git;
use std::collections::HashSet;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::error::Error;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{fetch_local, FetchDepth, FetchOptions, FetchOutcome, TagMode};
use grit_lib::remote::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn build_linear_bare(tmp: &Path, n: usize) -> PathBuf {
    let bare = tmp.join("remote.git");
    let wt = tmp.join("seed");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 0..n {
        std::fs::write(wt.join("f.txt"), format!("{i}\n")).unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            wt.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn shallow_clone(tmp: &Path, bare: &Path, name: &str, depth: u32) -> PathBuf {
    let dest = tmp.join(name);
    std::fs::create_dir_all(dest.parent().unwrap_or(tmp)).unwrap();
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--depth",
            &depth.to_string(),
            &format!("file://{}", bare.display()),
            dest.to_str().unwrap(),
        ],
    );
    dest
}

fn read_shallow_set(git_dir: &Path) -> HashSet<String> {
    let p = git_dir.join("shallow");
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return HashSet::new();
    };
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn rev_list_count(work: &Path) -> usize {
    git_out(work, &["rev-list", "--count", "HEAD"])
        .parse()
        .expect("count")
}

fn fsck(work: &Path) {
    let out = Command::new("git")
        .current_dir(work)
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit_fetch_local(
    work: &Path,
    bare: &Path,
    fetch_depth: Option<FetchDepth>,
) -> grit_lib::error::Result<()> {
    let git_dir = work.join(".git");
    fetch_local(
        &git_dir,
        bare,
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::Following,
            fetch_depth,
            ..Default::default()
        },
    )
    .map(|_| ())
}

fn git_fetch(work: &Path, args: &[&str]) {
    git(work, args);
}

fn assert_twin_matches(git_work: &Path, grit_work: &Path) {
    assert_eq!(
        read_shallow_set(&git_work.join(".git")),
        read_shallow_set(&grit_work.join(".git")),
        "shallow boundaries differ"
    );
    assert_eq!(
        rev_list_count(git_work),
        rev_list_count(grit_work),
        "rev-list --count HEAD differs"
    );
    fsck(git_work);
    fsck(grit_work);
}

/// Issue #1934: repeated `fetch --depth N` must set absolute depth, not deepen by N each time.
#[test]
fn fetch_depth_absolute_on_shallow_clone_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_linear_bare(tmp.path(), 6);
    let git_client = shallow_clone(tmp.path(), &bare, "git-abs", 1);
    let grit_client = shallow_clone(tmp.path(), &bare, "grit-abs", 1);
    assert_twin_matches(&git_client, &grit_client);
    assert_eq!(rev_list_count(&git_client), 1);

    for depth in [1u32, 2, 3] {
        git_fetch(
            &git_client,
            &["fetch", "--depth", &depth.to_string(), "origin"],
        );
        grit_fetch_local(&grit_client, &bare, FetchDepth::depth(depth)).expect("depth fetch");
        assert_twin_matches(&git_client, &grit_client);
        assert_eq!(
            rev_list_count(&git_client),
            depth as usize,
            "git depth {depth}"
        );
    }
}

#[test]
fn deepen_and_unshallow_file_remote_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_linear_bare(tmp.path(), 12);
    let git_client = shallow_clone(tmp.path(), &bare, "git-client", 1);
    let grit_client = shallow_clone(tmp.path(), &bare, "grit-client", 1);
    assert_twin_matches(&git_client, &grit_client);

    git_fetch(&git_client, &["fetch", "--deepen", "3", "origin"]);
    grit_fetch_local(&grit_client, &bare, FetchDepth::deepen(3)).expect("deepen fetch");
    assert_twin_matches(&git_client, &grit_client);

    git_fetch(&git_client, &["fetch", "--unshallow", "origin"]);
    grit_fetch_local(&grit_client, &bare, Some(FetchDepth::Unshallow)).expect("unshallow");
    assert_twin_matches(&git_client, &grit_client);
    assert!(read_shallow_set(&git_client.join(".git")).is_empty());
}

#[test]
fn unshallow_on_complete_repo_errors_typed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_linear_bare(tmp.path(), 3);
    let wt = tmp.path().join("full");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            &format!("file://{}", bare.display()),
            wt.to_str().unwrap(),
        ],
    );
    let err = fetch_local(
        &wt.join(".git"),
        &bare,
        &FetchOptions {
            refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
            fetch_depth: Some(FetchDepth::Unshallow),
            ..Default::default()
        },
    )
    .expect_err("unshallow on full repo");
    assert!(matches!(err, Error::FetchUnshallowNotShallow));
}

fn daemon_free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn spawn_git_daemon(base_path: &Path, port: u16) -> Child {
    Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base_path.display()))
        .arg(base_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git daemon")
}

fn wait_daemon_ready(port: u16) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("git daemon not ready on port {port}");
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn grit_fetch_daemon(
    work: &Path,
    url: &str,
    fetch_depth: Option<FetchDepth>,
    protocol_version: u8,
) -> FetchOutcome {
    grit_fetch_daemon_refspec(
        work,
        url,
        "+refs/heads/*:refs/remotes/origin/*",
        fetch_depth,
        protocol_version,
    )
}

fn grit_fetch_daemon_refspec(
    work: &Path,
    url: &str,
    refspec: &str,
    fetch_depth: Option<FetchDepth>,
    protocol_version: u8,
) -> FetchOutcome {
    let git_dir = work.join(".git");
    let transport = GitDaemonTransport::new();
    let connect_opts = ConnectOptions {
        protocol_version,
        ..Default::default()
    };
    let mut conn = transport
        .connect(url, Service::UploadPack, &connect_opts)
        .expect("connect git daemon");
    let mut opts = FetchOptions {
        refspecs: vec![refspec.to_owned()],
        tags: TagMode::Following,
        fetch_depth,
        ..Default::default()
    };
    fetch_remote(
        &git_dir,
        &mut *conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("fetch_remote")
}

/// Loose objects in `work` (git unpacks small fetched packs into loose objects).
fn loose_object_count(work: &Path) -> u64 {
    git_out(work, &["count-objects", "-v"])
        .lines()
        .find_map(|l| l.strip_prefix("count: "))
        .and_then(|n| n.parse().ok())
        .expect("count-objects")
}

struct ServedRepo {
    _guard: DaemonGuard,
    url: String,
    served: PathBuf,
}

/// `bare` cloned under a fresh `git daemon` root, served over `git://`.
fn serve_with_daemon(tmp: &Path, bare: &Path) -> ServedRepo {
    let base = tmp.join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let served = base.join("repo.git");
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            bare.to_str().unwrap(),
            served.to_str().unwrap(),
        ],
    );
    let port = daemon_free_port();
    let guard = DaemonGuard(spawn_git_daemon(&base, port));
    wait_daemon_ready(port);
    ServedRepo {
        _guard: guard,
        url: format!("git://127.0.0.1:{port}/repo.git"),
        served,
    }
}

/// A depth-1 clone deepened by 3 receives only the new commits' objects, the same
/// ones git receives.
#[test]
fn deepen_over_system_git_daemon_v2_sends_only_new_history() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_linear_bare(tmp.path(), 12);
    let srv = serve_with_daemon(tmp.path(), &bare);

    let git_client = init_shallow_client_from_daemon(tmp.path(), &srv.url, "git-deepen");
    let grit_client = init_shallow_client_from_daemon(tmp.path(), &srv.url, "grit-deepen");
    let git_before = loose_object_count(&git_client);
    git_fetch(&git_client, &["fetch", "--deepen", "3", "origin"]);
    let git_received = loose_object_count(&git_client) - git_before;

    let outcome = grit_fetch_daemon(&grit_client, &srv.url, FetchDepth::deepen(3), 2);
    assert_twin_matches(&git_client, &grit_client);
    assert_eq!(rev_list_count(&grit_client), 4);
    // Three new commits, each with its own tree and blob; the tip is not resent.
    assert_eq!(git_received, 9, "git baseline");
    assert_eq!(outcome.transfer.objects, git_received);
}

/// The benchmark shape: a full clone made shallow by `fetch --depth 1`, then
/// deepened. Another branch's complete history covers most of what the deepen
/// reaches, so only the commit above it is transferred.
#[test]
fn deepen_of_refetched_shallow_tip_uses_other_branches_as_haves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_linear_bare(tmp.path(), 12);
    git(&bare, &["branch", "old", "main~2"]);
    let srv = serve_with_daemon(tmp.path(), &bare);

    let mut clients = Vec::new();
    for name in ["git-refetch", "grit-refetch"] {
        let dest = tmp.path().join(name);
        git(
            tmp.path(),
            &["clone", "-q", &srv.url, dest.to_str().unwrap()],
        );
        git(&dest, &["fetch", "-q", "--depth", "1", "origin", "main"]);
        assert_eq!(read_shallow_set(&dest.join(".git")).len(), 1);
        clients.push(dest);
    }
    let (git_client, grit_client) = (&clients[0], &clients[1]);
    let tip = git_out(&srv.served, &["rev-parse", "main"]);

    git_fetch(git_client, &["fetch", "--deepen", "3", "origin", "main"]);
    let outcome = grit_fetch_daemon_refspec(
        grit_client,
        &srv.url,
        "+refs/heads/main:refs/remotes/origin/main",
        FetchDepth::deepen(3),
        2,
    );
    assert_twin_matches(git_client, grit_client);
    assert!(!read_shallow_set(&grit_client.join(".git")).contains(&tip));
    // main~1's commit, tree and blob: main~2 and below are held through `old`.
    assert_eq!(outcome.transfer.objects, 3);
}

fn init_shallow_client_from_daemon(tmp: &Path, url: &str, name: &str) -> PathBuf {
    let dest = tmp.join(name);
    std::fs::create_dir_all(dest.parent().unwrap_or(tmp)).unwrap();
    git(
        tmp,
        &["clone", "-q", "--depth", "1", url, dest.to_str().unwrap()],
    );
    dest
}

/// Full depth-1 → deepen-3 → unshallow against system `git upload-pack` (protocol v2).
#[test]
fn deepen_unshallow_over_system_git_daemon_v2_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_linear_bare(tmp.path(), 12);
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let served = base.join("repo.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            bare.to_str().unwrap(),
            served.to_str().unwrap(),
        ],
    );
    let port = daemon_free_port();
    let child = spawn_git_daemon(&base, port);
    let _guard = DaemonGuard(child);
    wait_daemon_ready(port);
    let url = format!("git://127.0.0.1:{port}/repo.git");

    let git_client = init_shallow_client_from_daemon(tmp.path(), &url, "git-wire");
    let grit_client = init_shallow_client_from_daemon(tmp.path(), &url, "grit-wire");
    assert_twin_matches(&git_client, &grit_client);

    git_fetch(&git_client, &["fetch", "--deepen", "3", "origin"]);
    grit_fetch_daemon(&grit_client, &url, FetchDepth::deepen(3), 2);
    assert_twin_matches(&git_client, &grit_client);

    git_fetch(&git_client, &["fetch", "--unshallow", "origin"]);
    grit_fetch_daemon(&grit_client, &url, Some(FetchDepth::Unshallow), 2);
    assert_twin_matches(&git_client, &grit_client);
    assert!(read_shallow_set(&git_client.join(".git")).is_empty());
}

#[cfg(feature = "http-ureq")]
mod http {
    use super::*;
    use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
    use grit_lib::remote::transport::http::{http_client_arc, http_fetch};

    fn free_port() -> Option<u16> {
        TcpListener::bind(("127.0.0.1", 0))
            .ok()
            .and_then(|l| l.local_addr().ok().map(|a| a.port()))
    }

    fn find_binary(name: &str) -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let deps = exe.parent()?;
        let profile = deps.parent()?;
        profile.join(name).is_file().then(|| profile.join(name))
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

    #[test]
    fn deepen_over_smart_http_matches_git() {
        let Some(server_bin) = find_binary("grit-http-server") else {
            eprintln!("SKIP: grit-http-server not built");
            return;
        };
        let Some(port) = free_port() else {
            eprintln!("SKIP: no free port");
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = build_linear_bare(tmp.path(), 12);
        let root = tmp.path().join("srv");
        std::fs::create_dir_all(&root).unwrap();
        let served = root.join("repo.git");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                bare.to_str().unwrap(),
                served.to_str().unwrap(),
            ],
        );
        let Some(child) = spawn_server(&server_bin, &root, port) else {
            eprintln!("SKIP: could not spawn server");
            return;
        };
        struct Guard(Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _g = Guard(child);
        if !wait_ready(port) {
            eprintln!("SKIP: http server not ready");
            return;
        }

        let url = format!("http://127.0.0.1:{port}/repo.git");
        let git_client = shallow_clone(tmp.path(), &served, "git-http", 1);
        let grit_client = shallow_clone(tmp.path(), &served, "grit-http", 1);
        // Re-point remotes at smart HTTP (shallow_clone used file:// above).
        for client in [&git_client, &grit_client] {
            git(client, &["remote", "set-url", "origin", &url]);
        }

        git_fetch(&git_client, &["fetch", "--deepen", "3", "origin"]);
        let client = http_client_arc(UreqHttpClient::new());
        let mut opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::Following,
            fetch_depth: FetchDepth::deepen(3),
            ..Default::default()
        };
        http_fetch(
            client,
            &grit_client.join(".git"),
            &url,
            &mut opts,
            &mut NoProgress,
        )
        .expect("http deepen");
        assert_twin_matches(&git_client, &grit_client);
    }

    #[test]
    fn absolute_depth_over_smart_http_matches_git() {
        let Some(server_bin) = find_binary("grit-http-server") else {
            eprintln!("SKIP: grit-http-server not built");
            return;
        };
        let Some(port) = free_port() else {
            eprintln!("SKIP: no free port");
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = build_linear_bare(tmp.path(), 6);
        let root = tmp.path().join("srv-abs");
        std::fs::create_dir_all(&root).unwrap();
        let served = root.join("repo.git");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                bare.to_str().unwrap(),
                served.to_str().unwrap(),
            ],
        );
        let Some(child) = spawn_server(&server_bin, &root, port) else {
            eprintln!("SKIP: could not spawn server");
            return;
        };
        struct Guard(Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _g = Guard(child);
        if !wait_ready(port) {
            eprintln!("SKIP: http server not ready");
            return;
        }

        let url = format!("http://127.0.0.1:{port}/repo.git");
        let git_client = shallow_clone(tmp.path(), &served, "git-http-abs", 1);
        let grit_client = shallow_clone(tmp.path(), &served, "grit-http-abs", 1);
        for client in [&git_client, &grit_client] {
            git(client, &["remote", "set-url", "origin", &url]);
        }
        assert_twin_matches(&git_client, &grit_client);
        assert_eq!(rev_list_count(&git_client), 1);

        let client = http_client_arc(UreqHttpClient::new());
        for depth in [1u32, 2, 3] {
            git_fetch(
                &git_client,
                &["fetch", "--depth", &depth.to_string(), "origin"],
            );
            let mut opts = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::Following,
                fetch_depth: FetchDepth::depth(depth),
                ..Default::default()
            };
            http_fetch(
                client.clone(),
                &grit_client.join(".git"),
                &url,
                &mut opts,
                &mut NoProgress,
            )
            .expect("http absolute depth fetch");
            assert_twin_matches(&git_client, &grit_client);
        }
    }
}
