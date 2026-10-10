//! Integration tests for [`grit_lib::remote`]: URL parsing, config loading,
//! `list_refs`, `fetch`, and `push` compared with system `git`.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::ConfigSet;
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::{list_refs_from_git_dir, ListRefsOptions, Remote, RemoteRef, RemoteUrl};
use grit_lib::repo::Repository;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};

#[cfg(feature = "http-ureq")]
use grit_lib::remote::DefaultHttpClientFactory;

struct EnvRestore {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl EnvRestore {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

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

fn git_ls_remote(
    url: &str,
    git_args: &[&str],
    patterns: &[&str],
    extra: &[(&str, &str)],
) -> BTreeMap<String, ObjectId> {
    let mut cmd = Command::new("git");
    cmd.arg("ls-remote")
        .args(git_args)
        .arg(url)
        .args(patterns)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    for (k, v) in extra {
        cmd.env(k, v);
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
        if line.starts_with("ref:") {
            continue;
        }
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

fn git_args_for_list_opts(opts: &ListRefsOptions) -> Vec<&'static str> {
    let mut args = Vec::new();
    if opts.symrefs {
        args.push("--symref");
    }
    args
}

fn assert_refs_match_git(
    url: &str,
    grit_refs: &[RemoteRef],
    opts: &ListRefsOptions,
    extra: &[(&str, &str)],
) {
    let git_args = git_args_for_list_opts(opts);
    let patterns: Vec<&str> = opts.prefixes.iter().map(String::as_str).collect();
    let mut git_map = git_ls_remote(url, &git_args, &patterns, extra);
    assert!(
        !git_map.is_empty(),
        "git ls-remote must return refs for {url}"
    );
    let mut grit_map = grit_refs_map(grit_refs);
    if !opts.peel {
        let drop_peel = |name: &str| name.ends_with("^{}");
        git_map.retain(|name, _| !drop_peel(name));
        grit_map.retain(|name, _| !drop_peel(name));
    }
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

#[cfg(feature = "http-ureq")]
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

fn bare_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let bare = tmp.path().join("remote.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    (tmp, bare)
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
    let (_tmp, bare) = bare_fixture();
    let url = format!("file://{}", bare.display());
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: true,
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert_refs_match_git(&url, &refs, &opts, &[]);
}

#[test]
fn list_refs_heads_only_matches_git() {
    let (_tmp, bare) = bare_fixture();
    let url = format!("file://{}", bare.display());
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        heads: true,
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert!(!refs.iter().any(|r| r.name == "HEAD"));

    let git_heads = git_ls_remote(&url, &["--heads"], &[], &[]);
    let grit_map = grit_refs_map(&refs);
    assert_eq!(git_heads, grit_map);
}

#[test]
fn remote_push_local_file_url_matches_git() {
    let (fixture, bare) = bare_fixture();
    let local = fixture.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(Some(&local), &["init", "-q", "-b", "main", "."]);
    let repo = Repository::open(&local.join(".git"), Some(&local)).unwrap();

    let fetch_opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    let url = format!("file://{}", bare.display());
    let remote = Remote::from_url(&url).unwrap();
    remote
        .fetch(&repo, fetch_opts, &mut grit_lib::fetch::NoProgress, None)
        .unwrap();

    std::fs::write(local.join("push.txt"), "push\n").unwrap();
    git(Some(&local), &["add", "push.txt"]);
    git(Some(&local), &["commit", "-q", "-m", "push commit"]);
    let tip = resolve_ref(&repo.git_dir, "refs/heads/main").unwrap();

    let push_spec = PushRefSpec {
        src: Some(tip),
        dst: "refs/heads/pushed".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    remote
        .push(
            &repo,
            &[push_spec],
            PushOptions::default(),
            &mut grit_lib::fetch::NoProgress,
            None,
        )
        .unwrap();

    let git_oid =
        ObjectId::from_hex(git(Some(&bare), &["rev-parse", "refs/heads/pushed"]).trim()).unwrap();
    assert_eq!(tip, git_oid);
    git(Some(&bare), &["fsck", "--strict"]);
}

struct DaemonFixture {
    _tmp: tempfile::TempDir,
    _guard: ChildGuard,
    url: String,
    bare: PathBuf,
}

fn daemon_fixture() -> Option<DaemonFixture> {
    let tmp = tempfile::tempdir().ok()?;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).ok()?;
    build_source(&work);
    let base = tmp.path().join("daemon");
    std::fs::create_dir_all(&base).ok()?;
    let bare = base.join("r.git");
    git(Some(&work), &["clone", "-q", "--bare", ".", bare.to_str()?]);
    let port = free_port()?;
    let child = spawn_daemon(&base, port)?;
    let guard = ChildGuard(child);
    if !wait_tcp(port) {
        return None;
    }
    Some(DaemonFixture {
        url: format!("git://127.0.0.1:{port}/r.git"),
        bare,
        _tmp: tmp,
        _guard: guard,
    })
}

#[test]
fn list_refs_git_daemon_v0_matches_git() {
    let Some(fixture) = daemon_fixture() else {
        panic!("git daemon fixture unavailable in this environment");
    };
    let url = &fixture.url;
    let remote = Remote::from_url(url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: false,
        protocol_version: Some(0),
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert!(!refs.is_empty(), "v0 daemon list_refs must return refs");
    assert_refs_match_git(url, &refs, &opts, &[]);
}

#[test]
fn list_refs_git_daemon_v2_pattern_main_matches_git() {
    let Some(fixture) = daemon_fixture() else {
        panic!("git daemon fixture unavailable in this environment");
    };
    let url = &fixture.url;
    let remote = Remote::from_url(url).unwrap();
    let opts = ListRefsOptions {
        prefixes: vec!["main".to_owned()],
        protocol_version: Some(2),
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "refs/heads/main");

    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", url, "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().expect("git ls-remote pattern");
    assert!(out.status.success());
    let git_out = String::from_utf8(out.stdout).expect("utf8");
    assert!(git_out.contains("refs/heads/main"));
}

#[test]
fn list_refs_git_daemon_v2_matches_git() {
    let Some(fixture) = daemon_fixture() else {
        panic!("git daemon fixture unavailable in this environment");
    };
    let url = &fixture.url;
    let remote = Remote::from_url(url).unwrap();
    let opts = ListRefsOptions {
        symrefs: true,
        peel: false,
        protocol_version: Some(2),
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert!(!refs.is_empty(), "v2 daemon list_refs must return refs");
    assert_refs_match_git(url, &refs, &opts, &[]);
}

#[test]
fn remote_fetch_git_daemon_matches_git() {
    let Some(fixture) = daemon_fixture() else {
        panic!("git daemon fixture unavailable in this environment");
    };
    let url = &fixture.url;
    let local = fixture._tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(Some(&local), &["init", "-q", "-b", "main", "."]);
    let repo = Repository::open(&local.join(".git"), Some(&local)).unwrap();
    let remote = Remote::from_url(url).unwrap();
    remote
        .fetch(
            &repo,
            FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::Following,
                ..Default::default()
            },
            &mut grit_lib::fetch::NoProgress,
            None,
        )
        .unwrap();
    let main_git =
        ObjectId::from_hex(git(Some(&fixture.bare), &["rev-parse", "refs/heads/main"]).trim())
            .unwrap();
    let main_grit = resolve_ref(&repo.git_dir, "refs/remotes/origin/main").unwrap();
    assert_eq!(main_git, main_grit);
}

#[test]
fn list_refs_ssh_matches_git_ls_remote() {
    let (_tmp, bare) = bare_fixture();
    let script = write_fake_ssh(_tmp.path()).expect("fake ssh script");
    let ssh_cmd = script.to_str().unwrap();
    let _env = EnvRestore::set("GIT_SSH_COMMAND", ssh_cmd);
    let url = format!("git@localhost:{}", bare.display());
    let remote = Remote::from_url(&url).unwrap();
    let options = RepositoryOptions::with_environment(Environment::capture_process());
    let ctx = Repository::open_with(&options, &bare, None).expect("open bare");
    let opts = ListRefsOptions {
        symrefs: true,
        peel: true,
        ..Default::default()
    };
    let refs = remote
        .list_refs(Some(&ctx), &opts, None)
        .expect("ssh list_refs");
    assert!(!refs.is_empty());
    assert_refs_match_git(&url, &refs, &opts, &[("GIT_SSH_COMMAND", ssh_cmd)]);
}

#[cfg(feature = "http-ureq")]
#[test]
fn list_refs_smart_http_matches_git_ls_remote() {
    let (_tmp, bare) = bare_fixture();
    let grit_bin = find_binary("grit").expect("grit binary (build grit-cli first)");
    let server_bin =
        find_binary("grit-http-server").expect("grit-http-server binary must be built for CI");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::rename(&bare, root.join("repo.git")).unwrap();
    let port = free_port().expect("free port");
    let child = spawn_http_server(&server_bin, &grit_bin, &root, port).expect("spawn server");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port), "smart HTTP server did not become ready");
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
    assert_refs_match_git(&url, &refs, &opts, &[]);
}

fn bare_with_changes_ref() -> (tempfile::TempDir, PathBuf) {
    let (tmp, bare) = bare_fixture();
    let main = git(Some(&bare), &["rev-parse", "refs/heads/main"]);
    git(Some(&bare), &["update-ref", "refs/changes/1", main.trim()]);
    (tmp, bare)
}

#[test]
fn list_refs_file_changes_namespace_matches_git() {
    let (_tmp, bare) = bare_with_changes_ref();
    let url = bare.to_str().unwrap();
    let odb = grit_lib::odb::Odb::new(&bare.join("objects"));
    let opts = ListRefsOptions {
        prefixes: vec!["refs/changes/*".to_owned()],
        ..Default::default()
    };
    let refs = list_refs_from_git_dir(&bare, &odb, &opts).unwrap();
    assert_refs_match_git(url, &refs, &opts, &[]);
    assert!(refs.iter().any(|r| r.name == "refs/changes/1"));
}

#[test]
fn list_refs_git_daemon_v2_changes_namespace_matches_git() {
    let Some(fixture) = daemon_fixture() else {
        panic!("git daemon fixture unavailable in this environment");
    };
    let main = git(Some(&fixture.bare), &["rev-parse", "refs/heads/main"]);
    git(
        Some(&fixture.bare),
        &["update-ref", "refs/changes/1", main.trim()],
    );
    let url = &fixture.url;
    let remote = Remote::from_url(url).unwrap();
    let opts = ListRefsOptions {
        prefixes: vec!["refs/changes/*".to_owned()],
        protocol_version: Some(2),
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert_refs_match_git(url, &refs, &opts, &[]);
    assert!(refs.iter().any(|r| r.name == "refs/changes/1"));
}

fn bare_with_annotated_tag() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(Some(&work), &["init", "-q", "-b", "main", "."]);
    git(Some(&work), &["commit", "-q", "--allow-empty", "-m", "c1"]);
    git(Some(&work), &["tag", "-a", "v1", "-m", "tag"]);
    let bare = tmp.path().join("remote.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    (tmp, bare)
}

#[test]
fn list_refs_file_pattern_suppresses_peel_line() {
    let (_tmp, bare) = bare_with_annotated_tag();
    let url = bare.to_str().unwrap();
    let odb = grit_lib::odb::Odb::new(&bare.join("objects"));
    let opts = ListRefsOptions {
        prefixes: vec!["v1".to_owned()],
        peel: true,
        ..Default::default()
    };
    let refs = list_refs_from_git_dir(&bare, &odb, &opts).unwrap();
    assert_refs_match_git(url, &refs, &opts, &[]);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "refs/tags/v1");
}

#[test]
fn list_refs_git_daemon_v2_pattern_suppresses_peel_line() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(Some(&work), &["init", "-q", "-b", "main", "."]);
    git(Some(&work), &["commit", "-q", "--allow-empty", "-m", "c1"]);
    git(Some(&work), &["tag", "-a", "v1", "-m", "tag"]);
    let base = tmp.path().join("base");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("r.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    let port = free_port().expect("free port");
    let child = spawn_daemon(&base, port).expect("spawn daemon");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port));
    let url = format!("git://127.0.0.1:{port}/r.git");
    let remote = Remote::from_url(&url).unwrap();
    let opts = ListRefsOptions {
        prefixes: vec!["v1".to_owned()],
        peel: true,
        protocol_version: Some(2),
        ..Default::default()
    };
    let refs = remote.list_refs(None, &opts, None).unwrap();
    assert_refs_match_git(&url, &refs, &opts, &[]);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "refs/tags/v1");
}
