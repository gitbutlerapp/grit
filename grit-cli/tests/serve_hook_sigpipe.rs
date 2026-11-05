//! Issue #2456: `grit serve` must stay up when a receive hook exits without
//! reading a large stdin (pre-receive / post-receive).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

/// Enough ref updates that hook stdin exceeds a typical 64 KiB pipe buffer.
const REF_COUNT: usize = 900;

fn grit(home: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(GRIT);
    cmd.current_dir(dir)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env_remove("GIT_ASKPASS")
        .stdin(Stdio::null());
    cmd
}

fn with_input(home: &Path, dir: &Path, args: &[&str], input: &str) -> Output {
    let mut child = grit(home, dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grit");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("grit")
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(home: &Path, root: &Path) -> (ServerGuard, String) {
    let mut child = grit(
        home,
        root,
        &[
            "serve",
            "--root",
            ".",
            "--bind",
            "127.0.0.1:0",
            "--no-bundles",
        ],
    )
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn grit serve");
    let stderr = child.stderr.take().expect("server stderr");
    let guard = ServerGuard(child);
    let mut stderr = BufReader::new(stderr);
    let mut first = String::new();
    stderr.read_line(&mut first).expect("read server banner");
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let url = first
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .unwrap_or_else(|| panic!("no URL in server banner: {first}"))
        .trim_end_matches('/')
        .to_owned();
    (guard, url)
}

fn install_hook(repo: &Path, name: &str) {
    let hooks = repo.join("hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let script = hooks.join(name);
    std::fs::write(&script, b"#!/bin/sh\nexit 0\n").expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

fn refspecs() -> Vec<String> {
    (0..REF_COUNT)
        .map(|i| format!("HEAD:refs/heads/b{i}"))
        .collect()
}

fn git_push(work: &Path, url: &str, specs: &[String]) -> Output {
    let mut args = vec!["push".to_owned(), url.to_owned()];
    args.extend(specs.iter().cloned());
    Command::new("git")
        .args(&args)
        .current_dir(work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git push")
}

fn server_responds(url: &str) -> bool {
    let host = url.trim_start_matches("http://");
    TcpStream::connect(host).is_ok()
}

struct ServeFixture {
    _tmp: tempfile::TempDir,
    repos: PathBuf,
    bare: PathBuf,
    url: String,
    token: String,
    _server: ServerGuard,
}

impl ServeFixture {
    fn new(hook: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let repos = tmp.path().join("repos");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&repos).expect("repos");
        Command::new("git")
            .args(["init", "-q", "--bare", "repo.git"])
            .current_dir(&repos)
            .output()
            .expect("git init --bare");
        install_hook(&repos.join("repo.git"), hook);
        let setup = [
            "serve",
            "--root",
            ".",
            "--setup",
            "--email",
            "push@example.com",
        ];
        let setup_out = with_input(&home, &repos, &setup, "serve password\n");
        assert!(
            setup_out.status.success(),
            "serve setup failed: {}",
            String::from_utf8_lossy(&setup_out.stderr)
        );
        let (server, url) = serve(&home, &repos);
        let token_out = grit(
            &home,
            &repos,
            &["serve", "--root", ".", "token", "push@example.com"],
        )
        .output()
        .expect("serve token");
        assert!(
            token_out.status.success(),
            "serve token failed: {}",
            String::from_utf8_lossy(&token_out.stderr)
        );
        let token = String::from_utf8_lossy(&token_out.stdout)
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("Token:")
                    .map(|rest| rest.trim().to_owned())
            })
            .expect("token line");
        let bare = repos.join("repo.git");
        Self {
            _tmp: tmp,
            repos,
            bare,
            url: format!("{url}/repo.git"),
            token,
            _server: server,
        }
    }

    fn push_url(&self) -> String {
        let host = self.url.trim_start_matches("http://");
        format!("http://u:{}@{host}", self.token)
    }

    fn seed_client(&self) -> PathBuf {
        let work = self.repos.join("work");
        std::fs::create_dir_all(&work).expect("work");
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&work)
            .output()
            .expect("git init");
        std::fs::write(work.join("a"), b"a\n").expect("file");
        Command::new("git")
            .args(["add", "a"])
            .current_dir(&work)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-qm", "a"])
            .current_dir(&work)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git commit");
        work
    }
}

fn assert_large_push_survives(hook: &str) {
    let fixture = ServeFixture::new(hook);
    let work = fixture.seed_client();
    let specs = refspecs();
    let push = git_push(&work, &fixture.push_url(), &specs);
    assert!(
        push.status.success(),
        "git push failed\n--- stdout\n{}\n--- stderr\n{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    let listed = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
        .current_dir(&fixture.bare)
        .output()
        .expect("for-each-ref");
    assert!(listed.status.success());
    let count = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(count, REF_COUNT, "expected {REF_COUNT} refs on server");
    assert!(
        server_responds(fixture.url.trim_end_matches("/repo.git")),
        "grit serve died after push with {hook}"
    );
}

#[test]
fn serve_survives_pre_receive_hook_that_ignores_large_stdin() {
    assert_large_push_survives("pre-receive");
}

#[test]
fn serve_survives_post_receive_hook_that_ignores_large_stdin() {
    assert_large_push_survives("post-receive");
}
