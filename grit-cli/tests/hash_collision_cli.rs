//! CLI add, fsck and fetch of blobs whose raw bytes are a SHA-1 collision prefix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::hash::{hash_object, try_hash_object};
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

fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
    let null = null_config();
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?} in {}", dir.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// Git's own history stores the SHAttered PDFs as blobs (`t/t0013/shattered-*.pdf`).
// Only their raw SHA-1 collides; as blobs they are ordinary objects for Git, so grit
// must add, check and fetch them the same way.

#[test]
fn grit_add_shattered_prefix_file_stages_git_blob_id() {
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
    let oid = try_hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data).expect("blob id");
    fs::write(root.join("collision.bin"), &data).expect("write");

    let out = Command::new(grit)
        .current_dir(root)
        .args(["add", "--json", "collision.bin"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("add");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let git_oid = git_output(root, &["hash-object", "collision.bin"]);
    assert_eq!(git_oid.trim(), oid.to_hex());
    let staged = git_output(root, &["ls-files", "-s", "collision.bin"]);
    assert!(staged.contains(&oid.to_hex()), "staged: {staged}");
    let fsck = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("git fsck");
    assert!(fsck.success());
}

#[test]
fn grit_fsck_accepts_shattered_prefix_blob() {
    let fix = CliFixture::empty();
    let data = shattered_prefix(2);
    let oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data);
    let repo =
        Repository::discover(Some(fix.path()), &RepositoryOptions::default()).expect("discover");
    write_loose_object(repo.odb().objects_dir(), FixtureAlgo::Sha1, "blob", &data)
        .expect("write loose");

    let out = fix.run(&["fsck", "--json"]);
    assert_eq!(out.status, Some(0), "fsck: {}", out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(parsed.get("ok"), Some(&serde_json::Value::Bool(true)));
    let issues = parsed
        .get("issues")
        .and_then(|v| v.as_array())
        .expect("issues array");
    assert!(
        !issues.iter().any(|issue| {
            issue.get("kind") == Some(&serde_json::json!("sha1Collision"))
                || issue.get("oid") == Some(&serde_json::Value::String(oid.to_hex()))
        }),
        "issues: {issues:?}"
    );
}

#[test]
fn grit_fetch_shattered_prefix_blob_from_git_remote() {
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
    let oid = try_hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data).expect("blob id");
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
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let repo = Repository::discover(Some(&client), &RepositoryOptions::default()).expect("client");
    assert_eq!(repo.odb().read(&oid).expect("fetched blob").data, data);
    let fsck = Command::new("git")
        .current_dir(&client)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("git fsck");
    assert!(fsck.success());
}
