//! `grit push` rejects updating an existing remote tag without `--force`.

use grit_test_support::{free_port, git, grit_cmd, CliFixture};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

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

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn git_http(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
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

#[test]
fn push_existing_tag_exits_one_and_json_tag_exists() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    std::fs::write(client.join("f"), b"1\n").unwrap();
    git(&client, &["add", "f"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    std::fs::write(client.join("f"), b"1\n2\n").unwrap();
    git(&client, &["commit", "-q", "-am", "c2"]);
    git(&client, &["tag", "v1", "HEAD~1"]);
    git(&client, &["push", "-q", "origin", "main", "v1"]);
    let on_remote = git(bare, &["rev-parse", "refs/tags/v1"]).trim().to_string();
    git(&client, &["tag", "-f", "v1", "HEAD"]);
    let out = grit_cmd(&["push", "--json", "origin", "v1"])
        .in_dir(&client)
        .exec();
    assert!(
        !out.ok(),
        "tag move without force should fail: {}",
        out.stderr
    );
    assert_eq!(
        git(bare, &["rev-parse", "refs/tags/v1"]).trim(),
        on_remote,
        "remote tag unchanged"
    );
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(value["rejected"], true);
    let results = value["results"].as_array().expect("results");
    assert!(
        results
            .iter()
            .any(|r| { r["status"] == "rejected" && r["reason"] == "tag_exists" }),
        "{value}"
    );
}

#[test]
fn push_tags_rejects_existing_while_new_tags_still_push() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    std::fs::write(client.join("f"), b"1\n").unwrap();
    git(&client, &["add", "f"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    git(&client, &["tag", "old", "HEAD"]);
    git(&client, &["push", "-q", "origin", "old"]);
    std::fs::write(client.join("g"), b"g\n").unwrap();
    git(&client, &["add", "g"]);
    git(&client, &["commit", "-q", "-m", "c2"]);
    git(&client, &["tag", "-f", "old", "HEAD"]);
    git(&client, &["tag", "new", "HEAD"]);
    let kept = git(bare, &["rev-parse", "refs/tags/old"])
        .trim()
        .to_string();
    let out = grit_cmd(&["push", "--tags", "--json", "origin"])
        .in_dir(&client)
        .exec();
    assert!(!out.ok(), "partial tag rejection should exit 1");
    assert_eq!(git(bare, &["rev-parse", "refs/tags/old"]).trim(), kept);
    assert!(!git(bare, &["rev-parse", "refs/tags/new"]).trim().is_empty());
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(value["rejected"], true);
}

#[test]
fn push_force_replaces_existing_tag() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    std::fs::write(client.join("f"), b"1\n").unwrap();
    git(&client, &["add", "f"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    git(&client, &["tag", "v1", "HEAD"]);
    git(&client, &["push", "-q", "origin", "v1"]);
    std::fs::write(client.join("f"), b"1\n2\n").unwrap();
    git(&client, &["commit", "-q", "-am", "c2"]);
    git(&client, &["tag", "-f", "v1", "HEAD"]);
    let want = git(&client, &["rev-parse", "refs/tags/v1"])
        .trim()
        .to_string();
    let out = grit_cmd(&["push", "--force", "origin", "v1"])
        .in_dir(&client)
        .exec();
    assert!(out.ok(), "force tag update: {}", out.stderr);
    assert_eq!(git(bare, &["rev-parse", "refs/tags/v1"]).trim(), want);
}

#[test]
fn push_existing_tag_smart_http_tag_exists_when_remote_tag_oid_missing_locally() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed = tmp.path().join("seed");
    git_http(
        tmp.path(),
        &["init", "-q", "-b", "main", seed.to_str().unwrap()],
    );
    git_http(&seed, &["config", "user.name", "t"]);
    git_http(&seed, &["config", "user.email", "t@example.com"]);
    std::fs::write(seed.join("f"), b"1\n").unwrap();
    git_http(&seed, &["add", "f"]);
    git_http(&seed, &["commit", "-qm", "c1"]);
    std::fs::write(seed.join("f"), b"1\n2\n").unwrap();
    git_http(&seed, &["commit", "-qam", "c2"]);
    git_http(&seed, &["tag", "-a", "-m", "rel", "v1", "HEAD~1"]);

    let bare = tmp.path().join("p.git");
    git_http(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git_http(&bare, &["config", "http.receivepack", "true"]);
    let remote_v1 = git(&bare, &["rev-parse", "refs/tags/v1"])
        .trim()
        .to_string();

    let srv = tmp.path().join("srv");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::rename(&bare, srv.join("p.git")).unwrap();
    let port = free_port().expect("port");
    let server_bin = grit_test_support::grit_http_server_binary();
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&srv)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GRIT_BIN", PathBuf::from(GRIT))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn http server");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port), "http server listen");

    let url = format!("http://127.0.0.1:{port}/p.git");
    let client = tmp.path().join("client");
    git_http(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--no-tags",
            url.as_str(),
            client.to_str().unwrap(),
        ],
    );
    git_http(&client, &["tag", "v1", "HEAD"]);
    let out = Command::new(GRIT)
        .args(["push", "--json", "origin", "v1"])
        .current_dir(&client)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit push");
    assert!(
        !out.status.success(),
        "push must fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git(srv.join("p.git").as_path(), &["rev-parse", "refs/tags/v1"]).trim(),
        remote_v1,
        "remote annotated tag unchanged"
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    assert_eq!(value["rejected"], true);
    let results = value["results"].as_array().expect("results");
    assert!(
        results
            .iter()
            .any(|r| { r["status"] == "rejected" && r["reason"] == "tag_exists" }),
        "expected tag_exists, got {value}"
    );
    assert!(
        !results.iter().any(|r| r["reason"] == "fetch first"),
        "must not report fetch first: {value}"
    );
}
