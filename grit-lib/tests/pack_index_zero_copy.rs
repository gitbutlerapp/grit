//! Zero-copy pack index compatibility and lookup oracles against system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::ObjectId;
use grit_lib::pack::{parse_pack_index_bytes, read_pack_index, read_pack_index_no_verify};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_show_index_lines(idx_path: &Path) -> Vec<(String, u64, Option<u32>)> {
    let bytes = std::fs::read(idx_path).expect("read idx");
    let mut child = Command::new("git")
        .args(["show-index", "--object-format=sha1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("spawn show-index");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&bytes)
        .expect("write idx to show-index");
    let out = child.wait_with_output().expect("show-index wait");
    assert!(
        out.status.success(),
        "git show-index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let off = parts.next().expect("offset").parse::<u64>().expect("off");
        let oid = parts.next().expect("oid").to_string();
        let crc = parts
            .next()
            .and_then(|s| s.strip_prefix('(').and_then(|t| t.strip_suffix(')')))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok());
        rows.push((oid, off, crc));
    }
    rows
}

fn make_repo_with_pack() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(
        dir.path().join("blob.txt"),
        b"pack index zero-copy fixture\n",
    )
    .unwrap();
    git(dir.path(), &["add", "blob.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    git(dir.path(), &["gc", "--quiet"]);
    let objects = dir.path().join(".git").join("objects").join("pack");
    let idx = std::fs::read_dir(&objects)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack idx");
    (dir, idx)
}

#[test]
fn v2_idx_matches_git_show_index() {
    let (_dir, idx_path) = make_repo_with_pack();
    let idx = read_pack_index(&idx_path).expect("parse v2");
    assert!(!idx.is_empty());
    let git_rows = git_show_index_lines(&idx_path);
    assert_eq!(idx.len(), git_rows.len());
    for (hex, off, crc) in git_rows {
        let oid = ObjectId::from_hex(&hex).expect("oid");
        let pos = idx.find_position(&oid).expect("oid in idx");
        assert_eq!(idx.find_offset(&oid), Some(off));
        assert_eq!(idx.offset_at(pos), off);
        assert_eq!(idx.oid_at(pos), oid.as_bytes());
        assert_eq!(idx.crc32_at(pos), crc);
    }
}

#[test]
fn v1_idx_matches_git_show_index() {
    let (_dir, v2_path) = make_repo_with_pack();
    let parsed = read_pack_index(&v2_path).expect("v2");
    let mut body = Vec::new();
    for slot in parsed.fanout {
        body.extend_from_slice(&slot.to_be_bytes());
    }
    for entry in parsed.iter() {
        body.extend_from_slice(&(entry.offset() as u32).to_be_bytes());
        body.extend_from_slice(entry.oid());
    }
    let digest = grit_lib::objects::HashAlgo::Sha1.digest(&body);
    body.extend_from_slice(digest.as_bytes());
    let v1_path = v2_path.with_file_name("test-v1.idx");
    std::fs::write(&v1_path, &body).expect("write v1");
    let idx = read_pack_index(&v1_path).expect("parse v1");
    let git_rows = git_show_index_lines(&v1_path);
    assert_eq!(idx.len(), git_rows.len());
    for (hex, off, _crc) in git_rows {
        let oid = ObjectId::from_hex(&hex).expect("oid");
        let pos = idx.find_position(&oid).expect("oid in idx");
        assert_eq!(idx.find_offset(&oid), Some(off));
        assert_eq!(idx.offset_at(pos), off);
        assert_eq!(idx.crc32_at(pos), None);
    }
}

#[test]
fn lookup_oracle_random_hits_and_misses() {
    let (_dir, idx_path) = make_repo_with_pack();
    let idx = read_pack_index_no_verify(&idx_path).expect("idx");
    let mut oracle = HashSet::new();
    for entry in idx.iter() {
        oracle.insert(ObjectId::from_bytes(entry.oid()).unwrap());
    }
    for oid in oracle.iter().copied() {
        assert!(idx.contains(&oid));
        assert!(idx.find_position(&oid).is_some());
    }
    let miss = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
    assert!(!oracle.contains(&miss));
    assert!(!idx.contains(&miss));
}

fn git_empty_v2_idx(dir: &Path, sha256: bool) -> PathBuf {
    if sha256 {
        git(dir, &["init", "-q", "-b", "main", "--object-format=sha256"]);
    } else {
        git(dir, &["init", "-q", "-b", "main"]);
    }
    let out = Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", "--index-version=2", "empty"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("pack-objects");
    assert!(
        out.status.success(),
        "git pack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stem = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!stem.is_empty(), "pack-objects hash");
    dir.join(format!("empty-{stem}.idx"))
}

#[test]
fn empty_sha256_v2_index_from_git_verified_and_no_verify() {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx_path = git_empty_v2_idx(dir.path(), true);
    assert_eq!(
        std::fs::metadata(&idx_path).expect("stat idx").len(),
        1096,
        "expected SHA-256 empty v2 idx size"
    );
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["verify-pack", "-v"])
        .arg(idx_path.with_extension("pack"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "git verify-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(dir.path(), &["fsck"]);
    let no_verify = read_pack_index_no_verify(&idx_path).expect("parse without verify");
    assert_eq!(no_verify.len(), 0);
    assert_eq!(no_verify.hash_bytes(), 32);
    let verified = read_pack_index(&idx_path).expect("parse with verify");
    assert_eq!(verified.len(), 0);
    assert_eq!(verified.hash_bytes(), 32);
}

#[test]
fn empty_sha1_v2_index_from_git_hash_width() {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx_path = git_empty_v2_idx(dir.path(), false);
    assert_eq!(
        std::fs::metadata(&idx_path).expect("stat idx").len(),
        1072,
        "expected SHA-1 empty v2 idx size"
    );
    let idx = read_pack_index(&idx_path).expect("parse sha1 empty idx");
    assert_eq!(idx.len(), 0);
    assert_eq!(idx.hash_bytes(), 20);
}

#[test]
fn corrupt_fanout_rejected() {
    let (_dir, idx_path) = make_repo_with_pack();
    let mut bytes = std::fs::read(&idx_path).expect("read");
    if bytes.len() > 16 {
        bytes[12] = bytes[12].wrapping_add(1);
        bytes[13] = bytes[13].wrapping_sub(1);
    }
    let err = parse_pack_index_bytes(Path::new("corrupt.idx"), bytes, false).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("non-monotonic") || msg.contains("corrupt") || msg.contains("truncated"),
        "{msg}"
    );
}

#[test]
fn open_large_git_index_fast_and_no_per_entry_alloc() {
    let candidates = [
        PathBuf::from("/usr/share/doc/git-doc"),
        PathBuf::from("/tmp/git.git"),
    ];
    let mut idx_path = None;
    for base in candidates {
        let p = base.join(".git/objects/pack");
        if let Ok(rd) = std::fs::read_dir(p) {
            if let Some(found) = rd
                .filter_map(|e| e.ok())
                .find(|e| e.path().extension().is_some_and(|x| x == "idx"))
            {
                idx_path = Some(found.path());
                break;
            }
        }
    }
    let Some(idx_path) = idx_path else {
        eprintln!("SKIP: no large git pack index fixture on this machine");
        return;
    };
    let start = Instant::now();
    let idx = read_pack_index_no_verify(&idx_path).expect("open idx");
    let elapsed = start.elapsed();
    assert!(
        idx.len() > 10_000,
        "expected a large index, got {}",
        idx.len()
    );
    assert!(
        elapsed.as_millis() < 50,
        "open took {:?} (expected <50ms without per-entry allocs)",
        elapsed
    );
    let sample = idx.find_position(&ObjectId::from_bytes(idx.oid_at(0)).unwrap());
    assert_eq!(sample, Some(0));
}
