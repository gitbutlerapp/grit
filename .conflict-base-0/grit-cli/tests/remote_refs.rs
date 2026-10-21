//! `grit remote refs` compared with `git ls-remote`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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

fn grit_in(cwd: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(GRIT)
        .current_dir(cwd)
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

fn grit(args: &[&str]) -> (String, String) {
    grit_in(Path::new("."), args)
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

fn bare_fixture_with_annotated_tag() -> (tempfile::TempDir, PathBuf) {
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
    git(
        Some(&work),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "tag",
            "-a",
            "v1",
            "-m",
            "release",
        ],
    );
    let bare = tmp.path().join("upstream.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    (tmp, bare)
}

fn grit_http_server_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().expect("workspace root");
        let status = Command::new("cargo")
            .current_dir(workspace)
            .args(["build", "-q", "-p", "grit-http-server"])
            .status()
            .expect("cargo build grit-http-server");
        assert!(
            status.success(),
            "failed to build grit-http-server for tests"
        );
        let debug = workspace.join("target/debug/grit-http-server");
        assert!(debug.is_file(), "missing {}", debug.display());
        debug
    })
    .clone()
}

#[test]
fn remote_refs_file_url_matches_git_ls_remote_human() {
    let (_tmp, bare) = bare_fixture_with_annotated_tag();
    let url = bare.to_str().unwrap();
    let git_map = parse_git_ls_remote(&git(None, &["ls-remote", url]));
    let (stdout, _) = grit(&["remote", "refs", url]);
    let grit_map = parse_git_ls_remote(&stdout);
    assert_eq!(grit_map, git_map);
    assert!(
        grit_map.keys().any(|k| k.ends_with("^{}")),
        "expected annotated tag peel line in output"
    );
}

#[test]
fn remote_refs_configured_name_ending_in_git_matches_git_ls_remote() {
    let (_tmp, bare) = bare_fixture_with_annotated_tag();
    let url = bare.to_str().unwrap();
    let client = tempfile::tempdir().unwrap();
    git(
        Some(client.path()),
        &["init", "-q", "-b", "main", client.path().to_str().unwrap()],
    );
    git(Some(client.path()), &["remote", "add", "upstream.git", url]);
    let git_map = parse_git_ls_remote(&git(Some(client.path()), &["ls-remote", "upstream.git"]));
    let (stdout, _) = grit_in(client.path(), &["remote", "refs", "upstream.git"]);
    assert_eq!(parse_git_ls_remote(&stdout), git_map);
}

#[test]
fn remote_refs_relative_path_matches_git_ls_remote() {
    let (tmp, bare) = bare_fixture_with_annotated_tag();
    let upstream = tmp.path().join("upstream");
    std::fs::rename(bare, &upstream).unwrap();
    let git_map = parse_git_ls_remote(&git(Some(tmp.path()), &["ls-remote", "upstream"]));
    let (stdout, _) = grit_in(tmp.path(), &["remote", "refs", "upstream"]);
    assert_eq!(parse_git_ls_remote(&stdout), git_map);
}

#[test]
fn remote_refs_json_includes_peeled_on_annotated_tag() {
    let (_tmp, bare) = bare_fixture_with_annotated_tag();
    let url = bare.to_str().unwrap();
    let (stdout, _) = grit(&["--json", "remote", "refs", url]);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["action"], "refs");
    let refs = value["refs"].as_array().unwrap();
    let tag = refs
        .iter()
        .find(|e| e["name"] == "refs/tags/v1")
        .expect("refs/tags/v1 entry");
    assert!(
        tag["peeled"].as_str().is_some(),
        "annotated tag must include peeled commit oid"
    );
    assert!(
        !refs
            .iter()
            .any(|e| e["name"].as_str() == Some("refs/tags/v1^{}")),
        "JSON uses peeled field instead of a separate ^{{}} row"
    );
}

#[test]
fn remote_refs_markdown_is_not_json() {
    let (_tmp, bare) = bare_fixture_with_annotated_tag();
    let url = bare.to_str().unwrap();
    let (stdout, _) = grit(&["--markdown", "remote", "refs", url]);
    assert!(
        stdout.trim_start().starts_with("## Remote refs"),
        "markdown must be a heading, not JSON: {stdout}"
    );
    assert!(
        !stdout.trim_start().starts_with('{'),
        "markdown must not be JSON"
    );
    assert!(stdout.contains("| Ref |"));
    assert!(stdout.contains("refs/tags/v1"));
}

#[test]
fn remote_refs_smart_http_matches_git() {
    use std::net::TcpListener;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    fn free_port() -> Option<u16> {
        TcpListener::bind("127.0.0.1:0")
            .ok()
            .and_then(|l| l.local_addr().ok().map(|a| a.port()))
    }

    fn wait_tcp(port: u16) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
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

    let (_tmp, bare) = bare_fixture_with_annotated_tag();
    let grit_bin = PathBuf::from(GRIT);
    let server_bin = grit_http_server_bin();
    let srv = tempfile::tempdir().unwrap();
    let root = srv.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::rename(bare, root.join("repo.git")).unwrap();
    let port = free_port().expect("port");
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GRIT_BIN", &grit_bin)
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
    assert!(git_map.keys().any(|k| k.ends_with("^{}")));
}

#[cfg(unix)]
mod ssh_env {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    fn write_fake_ssh(dir: &Path, log: &Path) -> PathBuf {
        let script = dir.join("fake-ssh.sh");
        let body = format!(
            r#"#!/bin/sh
echo "$*" >> {log}
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${{cmd#git-upload-pack }}" ;;
esac
eval "exec $cmd"
"#,
            log = log.display()
        );
        std::fs::write(&script, body).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    struct EnvRestore {
        key: &'static str,
        prior: Option<OsString>,
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

    #[test]
    fn remote_refs_literal_ssh_url_honors_git_ssh_command_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(Some(&work), &["init", "-q", "-b", "main"]);
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
        let log = tmp.path().join("ssh.log");
        let script = write_fake_ssh(tmp.path(), &log);
        let url = format!("ssh://localhost{}", bare.display());
        let _ssh = EnvRestore::set("GIT_SSH_COMMAND", script.to_str().unwrap());
        let out = Command::new(GRIT)
            .current_dir(tmp.path())
            .args(["remote", "refs", &url])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("grit");
        assert!(
            out.status.success(),
            "grit remote refs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            !calls.trim().is_empty(),
            "GIT_SSH_COMMAND must be used for literal ssh:// URLs outside a repository"
        );
        assert!(parse_git_ls_remote(&String::from_utf8(out.stdout).unwrap())
            .contains_key("refs/heads/main"));
    }
}
