//! Upload-pack shallow/deepen, partial-clone filters, and v2 `want-ref` against
//! grit's server (`grit-http-server` and fake-ssh `grit upload-pack`), compared
//! to system `git upload-pack`.
//!
//!   cargo test -p grit-lib --features http-ureq --test serve_shallow_filter

#![cfg(all(feature = "http-ureq", unix))]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    String::from_utf8(out.stdout).expect("utf8")
}

fn git_ok(dir: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
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

fn spawn_http_server(server_bin: &Path, root: &Path, port: u16) -> Option<Child> {
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
    let deadline = Instant::now() + Duration::from_secs(15);
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

fn build_linear_repo(dir: &Path, n: usize) {
    git(Some(dir), &["init", "-q", "-b", "main", "."]);
    for i in 0..n {
        std::fs::write(dir.join("f.txt"), format!("v{i}\n")).unwrap();
        git(Some(dir), &["add", "f.txt"]);
        git(Some(dir), &["commit", "-q", "-m", &format!("c{i}")]);
    }
}

fn bare_clone(source: &Path, dest: &Path) {
    git(
        None,
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
}

fn read_shallow(git_dir: &Path) -> Vec<String> {
    let p = git_dir.join("shallow");
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn object_snapshot(work: &Path) -> String {
    let out = git(
        Some(work),
        &["rev-list", "--all", "--objects", "--missing=print"],
    );
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort();
    lines.join("\n")
}

fn fsck_clean(work: &Path) {
    let out = git_ok(Some(work), &["fsck", "--no-dangling"]);
    assert!(
        out.status.success(),
        "fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn compare_to_git_reference(grit_work: &Path, git_work: &Path) {
    assert_eq!(
        read_shallow(&grit_work.join(".git")),
        read_shallow(&git_work.join(".git")),
        "shallow boundaries differ"
    );
    assert_eq!(
        object_snapshot(grit_work),
        object_snapshot(git_work),
        "rev-list --objects snapshot differs"
    );
    fsck_clean(grit_work);
    fsck_clean(git_work);
}

fn setup_bare_pair(tmp: &Path) -> (PathBuf, PathBuf) {
    let upstream = tmp.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    build_linear_repo(&upstream, 5);
    let grit_bare = tmp.join("grit.git");
    let git_bare = tmp.join("git.git");
    bare_clone(&upstream, &grit_bare);
    bare_clone(&upstream, &git_bare);
    for bare in [&grit_bare, &git_bare] {
        git(Some(bare), &["config", "uploadpack.allowFilter", "true"]);
        git(
            Some(bare),
            &["config", "uploadpack.allowReachableSha1InWant", "true"],
        );
    }
    (grit_bare, git_bare)
}

fn git_file_url(bare: &Path) -> String {
    format!("file://{}", bare.canonicalize().unwrap().to_string_lossy())
}

fn append_pkt_line(buf: &mut Vec<u8>, line: &str) {
    let payload = format!("{line}\n");
    let len = 4 + payload.len();
    buf.extend(format!("{len:04x}").as_bytes());
    buf.extend(payload.as_bytes());
}

fn append_pkt_flush(buf: &mut Vec<u8>) {
    buf.extend(b"0000");
}

/// Stateless v2 `command=fetch` POST with a `filter` argument (exercises ERR when disallowed).
fn post_v2_fetch_raw(post_url: &str, want: &str, sideband_all: bool) -> Vec<u8> {
    let mut body = Vec::new();
    append_pkt_line(&mut body, "command=fetch");
    append_pkt_line(&mut body, "agent=git/2.43.0");
    append_pkt_line(&mut body, "object-format=sha1");
    body.extend(b"0001");
    append_pkt_line(&mut body, "thin-pack");
    append_pkt_line(&mut body, "no-progress");
    append_pkt_line(&mut body, "include-tag");
    append_pkt_line(&mut body, "ofs-delta");
    if sideband_all {
        append_pkt_line(&mut body, "sideband-all");
    }
    append_pkt_line(&mut body, &format!("want {want}"));
    append_pkt_line(&mut body, "done");
    append_pkt_flush(&mut body);

    let out = ureq::post(post_url)
        .header("Content-Type", "application/x-git-upload-pack-request")
        .header("Accept", "application/x-git-upload-pack-result")
        .header("Git-Protocol", "version=2")
        .send(&body)
        .expect("POST upload-pack");
    assert_eq!(out.status(), 200, "HTTP status for v2 fetch");
    out.into_body().read_to_vec().expect("response body")
}

/// After the plain `packfile` section header, v2 requires band-1 side-band-64k
/// framing even when `sideband-all` was not negotiated.
fn v2_packfile_body_is_sideband_framed(resp: &[u8]) -> bool {
    let mut i = 0usize;
    let mut seen_packfile_header = false;
    while i + 4 <= resp.len() {
        let Ok(len_str) = std::str::from_utf8(&resp[i..i + 4]) else {
            break;
        };
        let Ok(len) = usize::from_str_radix(len_str, 16) else {
            break;
        };
        if len < 4 {
            i += 4;
            continue;
        }
        if i + len > resp.len() {
            break;
        }
        let payload = &resp[i + 4..i + len];
        i += len;
        let line = String::from_utf8_lossy(payload).trim_end().to_owned();
        if line == "packfile" {
            seen_packfile_header = true;
            continue;
        }
        if seen_packfile_header {
            return payload.first() == Some(&1) && payload.len() >= 5 && &payload[1..5] == b"PACK";
        }
    }
    false
}

fn post_v2_filter_fetch(post_url: &str, want: &str) -> String {
    let mut body = Vec::new();
    append_pkt_line(&mut body, "command=fetch");
    append_pkt_line(&mut body, "agent=git/2.43.0");
    append_pkt_line(&mut body, "object-format=sha1");
    body.extend(b"0001");
    append_pkt_line(&mut body, "thin-pack");
    append_pkt_line(&mut body, "no-progress");
    append_pkt_line(&mut body, "include-tag");
    append_pkt_line(&mut body, "ofs-delta");
    append_pkt_line(&mut body, "sideband-all");
    append_pkt_line(&mut body, "filter blob:none");
    append_pkt_line(&mut body, &format!("want {want}"));
    append_pkt_line(&mut body, "done");
    append_pkt_flush(&mut body);

    let out = ureq::post(post_url)
        .header("Content-Type", "application/x-git-upload-pack-request")
        .header("Accept", "application/x-git-upload-pack-result")
        .header("Git-Protocol", "version=2")
        .send(&body)
        .expect("POST upload-pack");
    assert_eq!(out.status(), 200, "HTTP status for filter fetch");
    out.into_body()
        .read_to_string()
        .expect("response body utf8")
}

fn clone_depth(url: &str, dest: &Path, depth: &str, extra_env: &[(&str, &str)]) {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "-q", "--depth", depth, url, dest.to_str().unwrap()]);
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("clone");
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fetch_in(repo: &Path, remote: &str, args: &[&str]) {
    let mut argv = vec!["fetch", "-q", remote];
    argv.extend(args);
    git(Some(repo), &argv);
}

#[test]
fn shallow_and_filter_http_matches_git_upload_pack() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: grit-http-server not built");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (grit_bare, _git_bare) = setup_bare_pair(tmp.path());

    let Some(port) = free_port() else {
        eprintln!("SKIP: no port");
        return;
    };
    let srv_root = tmp.path().join("http-root");
    std::fs::create_dir_all(&srv_root).unwrap();
    std::fs::create_dir_all(srv_root.join("repo.git")).unwrap();
    std::fs::rename(&grit_bare, srv_root.join("repo.git")).unwrap();
    let Some(child) = spawn_http_server(&server_bin, &srv_root, port) else {
        eprintln!("SKIP: server spawn");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: server not ready");
        return;
    }
    let grit_url = format!("http://127.0.0.1:{port}/repo.git");
    let git_url = git_file_url(&tmp.path().join("git.git"));

    let grit_clone = tmp.path().join("grit-depth1");
    let git_clone = tmp.path().join("git-depth1");
    clone_depth(&grit_url, &grit_clone, "1", &[]);
    clone_depth(&git_url, &git_clone, "1", &[]);
    compare_to_git_reference(&grit_clone, &git_clone);

    fetch_in(&grit_clone, "origin", &["--deepen", "2"]);
    fetch_in(&git_clone, "origin", &["--deepen", "2"]);
    compare_to_git_reference(&grit_clone, &git_clone);

    let grit_f = tmp.path().join("grit-filter-none");
    let git_f = tmp.path().join("git-filter-none");
    for (url, dest) in [(&grit_url, &grit_f), (&git_url, &git_f)] {
        let out = Command::new("git")
            .args([
                "clone",
                "-q",
                "--filter=blob:none",
                "--depth",
                "1",
                url,
                dest.to_str().unwrap(),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("filter clone");
        assert!(
            out.status.success(),
            "filter clone: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    compare_to_git_reference(&grit_f, &git_f);
}

#[test]
fn disallowed_filter_returns_err_over_http() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (grit_bare, _) = setup_bare_pair(tmp.path());
    git(
        Some(&grit_bare),
        &["config", "uploadpack.allowFilter", "false"],
    );
    let Some(port) = free_port() else {
        return;
    };
    let srv_root = tmp.path().join("http-root");
    std::fs::create_dir_all(&srv_root).unwrap();
    let repo_path = srv_root.join("repo.git");
    std::fs::rename(&grit_bare, &repo_path).unwrap();

    let Some(child) = spawn_http_server(&server_bin, &srv_root, port) else {
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        return;
    }
    let post_url = format!("http://127.0.0.1:{port}/repo.git/git-upload-pack");
    let want = git(Some(&repo_path), &["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned();
    let body = post_v2_filter_fetch(&post_url, &want);
    assert!(
        body.contains("ERR") && body.to_lowercase().contains("filter"),
        "expected filter ERR in response, got: {body}"
    );
}

fn write_fake_ssh(grit_bin: &Path, dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ssh.sh");
    let body = format!(
        r#"#!/bin/sh
# Git probes ssh configuration with `-G` before the real upload-pack invocation.
if [ "$1" = "-G" ]; then
  exec ssh "$@"
fi
while [ $# -gt 0 ]; do
  case "$1" in
    git-upload-pack)
      shift
      exec "{grit}" upload-pack "$1"
      ;;
    git-upload-pack\ *)
      path="${{1#git-upload-pack }}"
      path="${{path#\'}}"; path="${{path%\'}}"
      path="${{path#\"}}"; path="${{path%\"}}"
      exec "{grit}" upload-pack "$path"
      ;;
  esac
  shift
done
exec sh -c "$*"
"#,
        grit = grit_bin.display()
    );
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

#[test]
fn shallow_clone_over_ssh_wrapper_matches_git() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: grit binary not built");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (grit_bare, git_bare) = setup_bare_pair(tmp.path());
    let script = write_fake_ssh(&grit_bin, tmp.path()).expect("fake ssh");
    let path = grit_bare.canonicalize().unwrap();
    let grit_url = format!("ssh://dummy{}", path.display());
    let git_url = git_file_url(&git_bare);

    let grit_dest = tmp.path().join("grit-ssh");
    let git_dest = tmp.path().join("git-ref");
    for (url, dest) in [(&grit_url, &grit_dest), (&git_url, &git_dest)] {
        let out = Command::new("git")
            .args([
                "-c",
                &format!("core.sshCommand={}", script.display()),
                "clone",
                "-q",
                "--depth",
                "1",
                url,
                dest.to_str().unwrap(),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("clone");
        assert!(
            out.status.success(),
            "clone {url}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    compare_to_git_reference(&grit_dest, &git_dest);
}

#[test]
fn want_ref_v2_fetch_over_http() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (grit_bare, git_bare) = setup_bare_pair(tmp.path());
    git(
        Some(&grit_bare),
        &["config", "uploadpack.allowRefInWant", "true"],
    );
    git(
        Some(&git_bare),
        &["config", "uploadpack.allowRefInWant", "true"],
    );
    let Some(port) = free_port() else {
        return;
    };
    let srv_root = tmp.path().join("http-root");
    std::fs::create_dir_all(&srv_root).unwrap();
    let repo_path = srv_root.join("repo.git");
    std::fs::rename(&grit_bare, &repo_path).unwrap();
    let Some(child) = spawn_http_server(&server_bin, &srv_root, port) else {
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        return;
    }
    let grit_url = format!("http://127.0.0.1:{port}/repo.git");
    let git_url = git_file_url(&git_bare);

    let env_v2 = &[("GIT_PROTOCOL", "version=2")];
    let grit_w = tmp.path().join("grit-wref");
    let git_w = tmp.path().join("git-wref");
    std::fs::create_dir_all(&grit_w).unwrap();
    std::fs::create_dir_all(&git_w).unwrap();
    git(Some(&grit_w), &["init", "-q"]);
    git(Some(&git_w), &["init", "-q"]);
    git(Some(&grit_w), &["remote", "add", "origin", &grit_url]);
    git(Some(&git_w), &["remote", "add", "origin", &git_url]);

    let mut grit_cmd = Command::new("git");
    grit_cmd
        .current_dir(&grit_w)
        .args([
            "fetch",
            "-q",
            "origin",
            "refs/heads/main:refs/remotes/origin/main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    for (k, v) in env_v2 {
        grit_cmd.env(k, v);
    }
    assert!(grit_cmd.output().unwrap().status.success());

    let mut git_cmd = Command::new("git");
    git_cmd
        .current_dir(&git_w)
        .args([
            "fetch",
            "-q",
            "origin",
            "refs/heads/main:refs/remotes/origin/main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    for (k, v) in env_v2 {
        git_cmd.env(k, v);
    }
    assert!(git_cmd.output().unwrap().status.success());

    compare_to_git_reference(&grit_w, &git_w);
}

#[test]
fn v2_packfile_sideband_without_sideband_all() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (grit_bare, _git_bare) = setup_bare_pair(tmp.path());
    let tip = git(Some(&grit_bare), &["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned();
    let Some(port) = free_port() else {
        return;
    };
    let srv_root = tmp.path().join("http-root");
    std::fs::create_dir_all(&srv_root).unwrap();
    std::fs::create_dir_all(srv_root.join("repo.git")).unwrap();
    std::fs::rename(&grit_bare, srv_root.join("repo.git")).unwrap();
    let Some(child) = spawn_http_server(&server_bin, &srv_root, port) else {
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        return;
    }
    let post_url = format!("http://127.0.0.1:{port}/repo.git/git-upload-pack");
    let resp = post_v2_fetch_raw(&post_url, &tip, false);
    assert!(
        v2_packfile_body_is_sideband_framed(&resp),
        "v2 packfile section must be side-band framed without sideband-all"
    );
}
