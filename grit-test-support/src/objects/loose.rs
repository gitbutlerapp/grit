//! Loose object helpers for adversarial object tests.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use flate2::write::ZlibEncoder;
use flate2::Compression;

use super::git_check::object_format_env;
use super::hash::HashAlgo;

/// Hash a Git loose object (`type`, `size`, payload) and return hex oid.
#[must_use]
pub fn hash_loose_object(algo: HashAlgo, kind: &str, body: &[u8]) -> String {
    let header = format!("{kind} {}\0", body.len());
    let mut store = header.into_bytes();
    store.extend_from_slice(body);
    let digest = match algo {
        HashAlgo::Sha1 => {
            use sha1::{Digest, Sha1};
            Sha1::digest(&store).to_vec()
        }
        HashAlgo::Sha256 => {
            use sha2::{Digest, Sha256};
            Sha256::digest(&store).to_vec()
        }
    };
    hex::encode(digest)
}

/// Write a zlib-compressed loose object under `objects_dir` without invoking `git hash-object`.
///
/// Returns the hex object id.
pub fn write_loose_object(
    objects_dir: &Path,
    algo: HashAlgo,
    kind: &str,
    body: &[u8],
) -> std::io::Result<String> {
    let oid = hash_loose_object(algo, kind, body);
    let header = format!("{kind} {}\0", body.len());
    let mut plain = header.into_bytes();
    plain.extend_from_slice(body);
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    if enc.write_all(&plain).is_err() {
        return Err(std::io::Error::other("zlib compress loose object"));
    }
    let compressed = enc.finish().unwrap_or_default();
    let dir = objects_dir.join(&oid[..2]);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(&oid[2..]), compressed)?;
    Ok(oid)
}

/// Write an object with `git hash-object --literally -w` (malformed payloads allowed).
pub fn git_hash_object_literally(
    repo: &Path,
    algo: HashAlgo,
    kind: &str,
    body: &[u8],
) -> std::io::Result<String> {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["hash-object", "--literally", "-w", "-t", kind, "--stdin"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_OBJECT_FORMAT", object_format_env(algo))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git hash-object --literally failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Loose object path under `objects_dir` for a hex oid.
#[must_use]
pub fn loose_object_path(objects_dir: &Path, oid: &str) -> PathBuf {
    objects_dir.join(&oid[..2]).join(&oid[2..])
}
