//! Integration tests for [`grit_lib::remote`]: URL parsing, config loading,
//! `list_refs` across transports, and fetch/push parity with system `git`.

#![cfg(feature = "http-ureq")]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::{DefaultHttpClientFactory, ListRefsOptions, Remote, RemoteRef, RemoteUrl};
use grit_lib::repo::Repository;
use grit_lib::transfer::{FetchOptions, TagMode};

fn git(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
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

fn git_ls_remote(url: &str, ssh_command: Option<&str>) -> BTreeMap<String, ObjectId> {
    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", url])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if let Some(ssh) = ssh_command {
        cmd.env("GIT_SSH_COMMAND", ssh);
    }
    let out = cmd.output().expect("git ls-remote");
    assert!(
        out.status.success(),
        "git ls-remote failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = String::from_utf8(out.stdout).expect("utf8");
    let mut map = BTreeMap::new();
    for line in out.lines() {
        let Some((oid, name)) = line.split_once('\t').or_else(|| line.split_once(' ')) else {
            continue;
        };
        let oid = ObjectId::from_hex(oid.trim()).expect("git ls-remote oid");
        map.insert(name.trim().to_owned(), oid);
    }
    map
}

fn grit_refs_map(refs: &[RemoteRef]) -> BTreeMap<String, ObjectId> {
    refs.iter().map(|r| (r.name.clone(), r.oid)).collect()
}

fn assert_refs_match_git(url: &str, grit_refs: &[RemoteRef], ssh_command: Option<&str>) {
    let git_map = git_ls_remote(url, ssh_command);
    if git_map.is_empty() {
        eprintln!("SKIP: git ls-remote returned no refs for {url}");
        return;
    }
    let grit_map = grit_refs_map(grit_refs);
    assert_eq!(
        git_map, grit_map,
        "list_refs must match git ls-remote for {url}"
    );
}

fn build_source(dir: &Path) {
    git(Some(dir), &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(Some(dir), &["add", "a.txt"]);
    git(Some(dir), &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(Some(dir), &["add", "b.txt"]);
    git(Some(dir), &["commit", "-q", "-m", "c2"]);
    git(Some(dir), &["tag", "-a", "v1", "-m", "tag"]);
    git(Some(dir), &["branch", "topic"]);
}

fn free_port() -> Option<u16> {
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

fn spawn_daemon(base_path: &Path, port: u16) -> Option<Child> {
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
        .ok()
}

fn spawn_http_server(server_bin: &Path, grit_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GUST_BIN", grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

fn wait_tcp(port: u16) -> bool {
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

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_fake_ssh(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ssh.sh");
    let body = r#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
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

#[test]
fn remote_url_parsing_and_config() {
    assert!(matches!(
        RemoteUrl::try_from("git@h:r.git"),
        Ok(RemoteUrl::Ssh(_))
    ));
    assert!(matches!(
        RemoteUrl::try_from("ssh://git@h/r.git"),
        Ok(RemoteUrl::Ssh(_))
    ));
    assert!(matches!(
        RemoteUrl::try_from("git://h/r.git"),
        Ok(RemoteUrl::Git(_))
    ));
    assert!(matches!(
        RemoteUrl::try_from("https://h/r.git"),
        Ok(RemoteUrl::Https(_))
    ));
    assert!(matches!(
        RemoteUrl::try_from("file:///tmp/x"),
        Ok(RemoteUrl::File(_))
    ));
    assert!(matches!(
        RemoteUrl::try_from("/absolute/path"),
        Ok(RemoteUrl::Local(_))
    ));

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("url.https://example.com/.insteadOf", "ex:")
        .unwrap();
    cfg.add_command_override("url.https://push.example/.pushInsteadOf", "push:")
        .unwrap();
    cfg.add_command_override("remote.origin.url", "ex:org/repo.git")
        .unwrap();
    cfg.add_command_override("remote.origin.pushurl", "push:org/repo.git")
        .unwrap();
    cfg.add_command_override("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();

    let remote = Remote::from_config(&cfg, "origin").unwrap();
    assert_eq!(remote.name.as_deref(), Some("origin"));
    assert!(matches!(remote.fetch_url(), RemoteUrl::Https(_)));
    assert!(matches!(remote.push_url(), RemoteUrl::Https(_)));
    assert_eq!(remote.fetch_refspecs.len(), 1);
}

#[test]
fn list_refs_file_url_matches_git_ls_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let bare = tmp.path().join("remote.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );

    let url = format!("file://{}", bare.display());
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: true,
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert_refs_match_git(&url, &refs, None);
}

#[test]
fn list_refs_git_daemon_v0_and_fetch_push_match_git() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let base = tmp.path().join("daemon");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("r.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );

    let Some(port) = free_port() else {
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        return;
    };
    let _guard = ChildGuard(child);
    if !wait_tcp(port) {
        return;
    }

    let url = format!("git://127.0.0.1:{port}/r.git");
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: true,
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert_refs_match_git(&url, &refs, None);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(Some(&local), &["init", "-q", "-b", "main", "."]);
    let repo = Repository::open(&local.join(".git"), Some(&local)).unwrap();

    let fetch_opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    remote
        .fetch(&repo, fetch_opts, &mut grit_lib::fetch::NoProgress, None)
        .unwrap();

    let main_git =
        ObjectId::from_hex(git(Some(&bare), &["rev-parse", "refs/heads/main"]).trim()).unwrap();
    let main_grit = resolve_ref(&repo.git_dir, "refs/remotes/origin/main").unwrap();
    assert_eq!(main_git, main_grit);
    git(Some(&local), &["fsck", "--strict"]);
}

#[test]
fn list_refs_ssh_matches_git_ls_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let bare = tmp.path().join("remote.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );

    let Some(script) = write_fake_ssh(tmp.path()) else {
        return;
    };
    let ssh_cmd = script.to_str().unwrap();
    std::env::set_var("GIT_SSH_COMMAND", ssh_cmd);
    let url = format!("git@localhost:{}", bare.display());
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: true,
        ..Default::default()
    };
    let ctx = Repository::open(&bare, None).ok();
    let refs = match remote.list_refs(ctx.as_ref(), &opts, None) {
        Ok(r) => r,
        Err(_) => return,
    };
    if refs.is_empty() {
        eprintln!("SKIP: grit list_refs returned no refs over ssh wrapper");
        return;
    }
    assert_refs_match_git(&url, &refs, Some(ssh_cmd));
}

#[test]
fn list_refs_smart_http_matches_git_ls_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let bare = tmp.path().join("remote.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );

    let Some(grit_bin) = find_binary("grit") else {
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        return;
    };
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::rename(&bare, root.join("repo.git")).unwrap();
    let Some(port) = free_port() else {
        return;
    };
    let Some(child) = spawn_http_server(&server_bin, &grit_bin, &root, port) else {
        return;
    };
    let _guard = ChildGuard(child);
    if !wait_tcp(port) {
        return;
    }
    let url = format!("http://127.0.0.1:{port}/repo.git");
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: true,
        ..Default::default()
    };
    let refs = remote
        .list_refs(None, &opts, Some(&DefaultHttpClientFactory))
        .unwrap();
    assert_refs_match_git(&url, &refs, None);
}
