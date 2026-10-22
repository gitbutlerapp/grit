//! CLI behavior for SHAttered vectors: ingest succeeds; fsck stays Git-compatible.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::hash::hash_object;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::Repository;
use grit_test_support::{write_loose_object, CliFixture, HashAlgo as FixtureAlgo};

fn shattered_prefix(which: u8) -> Vec<u8> {
    let name = format!("shattered-{which}-prefix.bin");
    fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../grit-lib/tests/data")
            .join(name),
    )
    .expect("fixture")
}

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let null = null_config();
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} in {}", args, dir.display());
}

#[test]
fn grit_add_shattered_file_succeeds() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let null = null_config();

    assert!(Command::new(grit)
        .current_dir(root)
        .args(["init"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("init")
        .success());

    let data = shattered_prefix(1);
    let path = root.join("collision.bin");
    fs::write(&path, &data).expect("write");

    let out = Command::new(grit)
        .current_dir(root)
        .args(["add", "--json", "collision.bin"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("add");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    assert_eq!(parsed.get("staged"), Some(&serde_json::json!(1)));
}

#[test]
fn grit_commit_shattered_file_succeeds() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let null = null_config();

    assert!(Command::new(grit)
        .current_dir(root)
        .args(["init"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("init")
        .success());

    fs::write(root.join("collision.bin"), shattered_prefix(1)).expect("write");
    assert!(Command::new(grit)
        .current_dir(root)
        .args(["add", "collision.bin"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("add")
        .success());

    let out = Command::new(grit)
        .current_dir(root)
        .args(["commit", "-m", "pdf", "--json"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("commit");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn grit_fsck_passes_on_shattered_loose_object() {
    let fix = CliFixture::empty();
    let data = shattered_prefix(2);
    let repo =
        Repository::discover(Some(fix.path()), &RepositoryOptions::default()).expect("discover");
    write_loose_object(repo.odb().objects_dir(), FixtureAlgo::Sha1, "blob", &data)
        .expect("write loose");

    let out = fix.run(&["fsck", "--json"]);
    assert_eq!(out.status, Some(0), "fsck json: {}", out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(parsed.get("ok"), Some(&serde_json::Value::Bool(true)));
}

#[test]
fn grit_clone_local_repo_with_shattered_blob() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let null = null_config();
    let base = tempfile::tempdir().expect("tempdir");
    let src = base.path().join("src");
    let dst = base.path().join("dst");
    fs::create_dir(&src).expect("src");
    git_in(&src, &["init", "-b", "main"]);
    let data = shattered_prefix(1);
    fs::write(src.join("collision.bin"), &data).expect("write");
    git_in(&src, &["add", "collision.bin"]);
    git_in(&src, &["commit", "-m", "collision"]);

    let out = Command::new(grit)
        .current_dir(base.path())
        .args([
            "clone",
            "--json",
            src.to_str().expect("utf8"),
            dst.to_str().expect("utf8"),
        ])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("clone");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dst.join(".git").exists() || dst.join("HEAD").exists());
    let expected_oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data);
    let cat = Command::new(grit)
        .current_dir(&dst)
        .args(["cat", &expected_oid.to_hex()])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("cat");
    assert!(
        cat.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&cat.stderr)
    );
    assert_eq!(cat.stdout, data);
}

#[test]
fn grit_fetch_shattered_pack_succeeds() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let null = null_config();
    let base = tempfile::tempdir().expect("tempdir");
    let bare = base.path().join("remote.git");
    let upstream = base.path().join("upstream");
    let client = base.path().join("client");

    git_in(
        base.path(),
        &["init", "--bare", bare.to_str().expect("utf8")],
    );
    fs::create_dir(&upstream).expect("upstream dir");
    git_in(&upstream, &["init", "-b", "main"]);
    let data = shattered_prefix(1);
    fs::write(upstream.join("collision.bin"), &data).expect("write");
    git_in(&upstream, &["add", "collision.bin"]);
    git_in(&upstream, &["commit", "-m", "collision"]);
    git_in(
        &upstream,
        &["remote", "add", "origin", bare.to_str().expect("utf8")],
    );
    git_in(&upstream, &["push", "-u", "origin", "main"]);

    fs::create_dir(&client).expect("client dir");
    assert!(Command::new(grit)
        .current_dir(&client)
        .args(["init"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("init")
        .success());
    assert!(Command::new(grit)
        .current_dir(&client)
        .args(["remote", "add", "origin", bare.to_str().expect("utf8"),])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("remote add")
        .success());

    let out = Command::new(grit)
        .current_dir(&client)
        .args(["fetch", "origin", "--json"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("fetch");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    assert_eq!(parsed.get("remote"), Some(&serde_json::json!("origin")));
    assert!(
        parsed.get("updated").and_then(|v| v.as_u64()).is_some(),
        "fetch json: {parsed}"
    );
}
