//! CLI JSON for SHA-1 collision detection (sha1dc) on add and fsck.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error as LibError;
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

#[test]
fn grit_add_shattered_file_fails_with_hash_collision_json() {
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
    let oid = match try_hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data).expect_err("collision")
    {
        LibError::HashCollision { oid } => oid,
        other => panic!("expected HashCollision, got {other:?}"),
    };
    let path = root.join("collision.bin");
    fs::write(&path, &data).expect("write");

    let out = Command::new(grit)
        .current_dir(root)
        .args(["add", "--json", "collision.bin"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("add");

    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    assert_eq!(
        parsed.get("kind"),
        Some(&serde_json::json!("hash_collision"))
    );
    assert_eq!(
        parsed.get("oid"),
        Some(&serde_json::Value::String(oid.to_hex()))
    );
    assert!(parsed.get("error").and_then(|v| v.as_str()).is_some());
}

#[test]
fn grit_fsck_json_reports_hash_collision() {
    let fix = CliFixture::empty();
    let data = shattered_prefix(2);
    let oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data);
    let repo =
        Repository::discover(Some(fix.path()), &RepositoryOptions::default()).expect("discover");
    write_loose_object(repo.odb().objects_dir(), FixtureAlgo::Sha1, "blob", &data)
        .expect("write loose");

    let out = fix.run(&["fsck", "--json"]);
    assert_eq!(out.status, Some(1), "fsck should fail on sha1 collision");

    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(parsed.get("ok"), Some(&serde_json::Value::Bool(false)));

    let issues = parsed
        .get("issues")
        .and_then(|v| v.as_array())
        .expect("issues array");
    let collision = issues
        .iter()
        .find(|issue| issue.get("kind") == Some(&serde_json::json!("sha1Collision")))
        .expect("sha1Collision issue");
    assert_eq!(collision.get("severity"), Some(&serde_json::json!("error")));
    assert_eq!(
        collision.get("oid"),
        Some(&serde_json::Value::String(oid.to_hex()))
    );
    assert!(
        collision
            .get("detail")
            .and_then(|d| d.as_str())
            .is_some_and(|d| d.contains("collision")),
        "detail: {:?}",
        collision.get("detail")
    );
}
