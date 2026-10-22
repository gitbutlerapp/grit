//! `grit tag --verify` integration tests.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GS)
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("grit")
}

fn install_tag_object(root: &Path, tag_bytes: &[u8], ref_name: &str) {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "-w", "-t", "tag", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .spawn()
        .expect("hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(tag_bytes)
        .expect("write tag");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "hash-object tag");
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(root, &["update-ref", ref_name, &oid]);
}

fn tree_sha256_fixture_root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../grit-lib/tests/fixtures/tree_sha256_tag");
    Command::new("cp")
        .args([
            "-a",
            fixture.join("dot-git").to_str().unwrap(),
            root.join(".git").to_str().unwrap(),
        ])
        .status()
        .expect("cp fixture");
    let allowed = fixture.join("allowed_signers");
    git(
        &root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );
    git(&root, &["config", "gpg.format", "ssh"]);
    let tag_bytes = std::fs::read(fixture.join("tag-object.txt")).expect("tag bytes");
    install_tag_object(&root, &tag_bytes, "refs/tags/v-fixture");
    (tmp, root)
}

#[test]
fn tag_verify_ok_exit_0_json_shape() {
    let (_tmp, root) = tree_sha256_fixture_root();
    let out = grit(&root, &["tag", "--verify", "v-fixture", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["action"], "verify");
    assert_eq!(v["ok"], true);
    let tag = &v["tags"][0];
    assert_eq!(tag["name"], "v-fixture");
    assert_eq!(tag["signature"]["verdict"], "good");
    assert!(tag["tree_sha256"]["match_digest"].as_bool().unwrap());
    assert!(tag["elapsed"]["ms"].as_u64().is_some());
}

#[test]
fn tag_verify_mismatch_exit_1() {
    let (_tmp, root) = tree_sha256_fixture_root();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../grit-lib/tests/fixtures/tree_sha256_tag");
    let tag_bytes = std::fs::read(fixture.join("tag-object.txt")).expect("tag bytes");
    let tag_text = String::from_utf8_lossy(&tag_bytes);
    let tag_text = tag_text.replace(
        "a119881755f081b9f694f1f05eb31fa4771a108bc854d61fdd4d12482e2a14e2",
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    install_tag_object(&root, tag_text.as_bytes(), "refs/tags/bad");

    let out = grit(&root, &["tag", "--verify", "bad", "--json"]);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["ok"], false);
    assert_eq!(v["tags"][0]["tree_sha256"]["match_digest"], false);
}

#[test]
fn tag_verify_global_filter_after_subcommand() {
    let (_tmp, root) = tree_sha256_fixture_root();
    let out = grit(
        &root,
        &[
            "tag",
            "--verify",
            "does-not-exist",
            "--filter",
            ".tags",
            "--json",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("cannot be used with"),
        "global --filter must not be parsed as a tag name: {stderr}"
    );
    serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("filtered json stdout");
}

#[test]
fn tag_verify_global_threads_after_subcommand() {
    let (_tmp, root) = tree_sha256_fixture_root();
    let out = grit(
        &root,
        &[
            "tag",
            "--verify",
            "does-not-exist",
            "--threads",
            "1",
            "--json",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("value is required for '--threads"),
        "threads value must not be treated as a tag name"
    );
}

#[test]
fn tag_verify_two_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["commit", "--allow-empty", "-m", "init"]);
    assert!(grit(root, &["tag", "one", "-m", "a"]).status.success());
    assert!(grit(root, &["tag", "two", "-m", "b"]).status.success());
    let out = grit(
        root,
        &["tag", "--verify", "one", "two", "--no-signature", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["tags"].as_array().map(|a| a.len()), Some(2));
}

#[test]
fn tag_verify_missing_submodule_keeps_signature_verdict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    let fake = "0000000000000000000000000000000000000001";
    Command::new("git")
        .current_dir(root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{fake},sub"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("index");
    git(root, &["commit", "-m", "sub"]);
    let head = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("rev-parse");
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let body = format!(
        "object {head}\ntype commit\ntag t\ntagger T <t@example.com> 1 +0000\n\
tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\nnote\n"
    );
    install_tag_object(root, body.as_bytes(), "refs/tags/t");

    let out = grit(root, &["tag", "--verify", "t", "--json"]);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["tags"][0]["signature"]["verdict"], "missing");
    assert!(v["tags"][0]["not_verifiable_submodules"]
        .as_array()
        .is_some_and(|a| !a.is_empty()));
}

#[test]
fn tag_verify_no_signature_flag() {
    let (_tmp, root) = tree_sha256_fixture_root();
    let out = grit(
        &root,
        &["tag", "--verify", "v-fixture", "--no-signature", "--json"],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["tags"][0]["signature"]["verdict"], "not_checked");
    assert_eq!(v["ok"], true);
}

#[test]
fn tag_verify_markdown_snapshot() {
    let grit = GS;
    let script = format!(
        r#"
set -euo pipefail
TMP=$(mktemp -d)
cd "$TMP"
git init -q -b main
git config user.email t@example.com
git config user.name T
git commit --allow-empty -m init -q
{grit} tag ann -m note
{grit} tag --verify ann --no-signature --markdown | grep -qE '(signature|tree-sha256|elapsed)'
"#
    );
    let status = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("bash");
    assert!(status.success());
}

#[test]
fn tag_verify_lightweight_tag_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["commit", "--allow-empty", "-m", "init"]);
    git(root, &["tag", "light"]);
    let out = grit(root, &["tag", "--verify", "light"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("lightweight") || stderr.contains("annotated"),
        "{stderr}"
    );
}
