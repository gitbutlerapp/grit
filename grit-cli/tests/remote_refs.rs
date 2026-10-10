//! `grit remote refs` compared with `git ls-remote`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn grit(args: &[&str]) -> (String, String) {
    let out = Command::new(GRIT)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn parse_git_ls_remote(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((a, b)) = line.split_once('\t') {
            if a.starts_with("ref: ") {
                continue;
            }
            map.insert(b.to_owned(), a.to_owned());
        }
    }
    map
}

fn bare_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(Some(&work), &["init", "-q", "-b", "main", "."]);
    git(
        Some(&work),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "commit",
            "--allow-empty",
            "-qm",
            "init",
        ],
    );
    let bare = tmp.path().join("upstream.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    (tmp, bare)
}

#[test]
fn remote_refs_file_url_matches_git_ls_remote_human() {
    let (_tmp, bare) = bare_fixture();
    let url = bare.to_str().unwrap();
    let git_map = parse_git_ls_remote(&git(None, &["ls-remote", url]));
    let (stdout, _) = grit(&["remote", "refs", url]);
    let grit_map = parse_git_ls_remote(&stdout);
    assert_eq!(grit_map, git_map);
}

#[test]
fn remote_refs_json_schema() {
    let (_tmp, bare) = bare_fixture();
    let url = bare.to_str().unwrap();
    let (stdout, _) = grit(&["--json", "remote", "refs", url]);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["action"], "refs");
    let refs = value["refs"].as_array().unwrap();
    assert!(!refs.is_empty());
    for entry in refs {
        assert!(entry.get("name").and_then(|v| v.as_str()).is_some());
        assert!(entry.get("oid").and_then(|v| v.as_str()).is_some());
        assert!(entry.get("peeled").is_some() || entry.get("peeled").is_none());
        assert!(entry.get("symref_target").is_some() || entry.get("symref_target").is_none());
    }
}

#[test]
fn remote_refs_markdown_emits_json_document() {
    let (_tmp, bare) = bare_fixture();
    let url = bare.to_str().unwrap();
    let (stdout, _) = grit(&["--markdown", "remote", "refs", url]);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["action"], "refs");
}

#[test]
fn remote_refs_smart_http_matches_git() {
    use std::net::TcpListener;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    fn find_binary(name: &str) -> Option<std::path::PathBuf> {
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

    fn free_port() -> Option<u16> {
        TcpListener::bind("127.0.0.1:0")
            .ok()
            .and_then(|l| l.local_addr().ok().map(|a| a.port()))
    }

    fn wait_tcp(port: u16) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }

    let (_tmp, bare) = bare_fixture();
    let grit_bin = find_binary("grit").expect("grit");
    let server_bin = find_binary("grit-http-server").expect("grit-http-server");
    let srv = tempfile::tempdir().unwrap();
    let root = srv.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::rename(&bare, root.join("repo.git")).unwrap();
    let port = free_port().expect("port");
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GUST_BIN", &grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port));
    let url = format!("http://127.0.0.1:{port}/repo.git");
    let git_map = parse_git_ls_remote(&git(None, &["ls-remote", &url]));
    let (stdout, _) = grit(&["remote", "refs", &url]);
    assert_eq!(parse_git_ls_remote(&stdout), git_map);
}
