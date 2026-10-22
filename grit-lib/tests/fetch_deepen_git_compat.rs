//! Shallow fetch `--depth`, `--deepen`, and `--unshallow` vs system `git` (file:// and HTTP).

use std::collections::HashSet;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::error::Error;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{fetch_local, FetchDepth, FetchOptions, TagMode};
fn git(dir: &Path, args: &[&str]) {
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
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

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

#[cfg(feature = "http-ureq")]
mod http {
    use super::*;
    use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
    use grit_lib::remote::transport::http::{http_client_arc, http_fetch};

    fn free_port() -> Option<u16> {
        TcpListener::bind(("127.0.0.1", 0))
            .ok()
            .map(|l| l.local_addr().ok()?.port())
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
}
