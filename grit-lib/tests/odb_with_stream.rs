//! `Odb::with_stream`: unbuffered payload streams for loose, whole-packed and deltified objects,
//! checked against objects written by grit and packed by the system `git`.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use grit_lib::error::Error;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Text that compresses moderately, like source code, so streams span many zlib blocks.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut n = seed;
    while out.len() < len {
        out.extend_from_slice(format!("line {n} of the large blob\n").as_bytes());
        n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1) % 1_000_003;
    }
    out.truncate(len);
    out
}

/// Read the whole stream in small chunks, as a work-tree copy would.
fn stream_in_chunks(odb: &Odb, oid: &ObjectId) -> std::io::Result<(ObjectKind, u64, Vec<u8>)> {
    odb.with_stream(oid, |stream| {
        let mut reader = stream.reader;
        let mut out = Vec::new();
        let mut chunk = [0u8; 8 * 1024];
        let result = loop {
            match reader.read(&mut chunk) {
                Ok(0) => break Ok((stream.kind, stream.size, out)),
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(e) => break Err(e),
            }
        };
        Ok(result)
    })
    .expect("with_stream")
    .expect("object present")
}

/// Pack exactly `oids` with the system `git` and drop their loose copies; returns the `.pack` path.
fn git_pack(dir: &Path, oids: &[ObjectId]) -> std::path::PathBuf {
    let list: String = oids.iter().map(|o| format!("{}\n", o.to_hex())).collect();
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", "-q", ".git/objects/pack/pack"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn pack-objects");
    std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), list.as_bytes())
        .expect("feed pack-objects");
    assert!(child.wait().expect("pack-objects").success());
    git(dir, &["prune-packed"]);
    fs::read_dir(dir.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack written")
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    dir
}

#[test]
fn with_stream_reads_loose_object_payload() {
    let dir = init_repo();
    let odb = Odb::new(&dir.path().join(".git/objects"));
    let data = payload(3 * 1024 * 1024 + 17, 1);
    let oid = odb.write(ObjectKind::Blob, &data).expect("write");

    let (kind, size, read) = stream_in_chunks(&odb, &oid).expect("stream");
    assert_eq!(kind, ObjectKind::Blob);
    assert_eq!(size, data.len() as u64);
    assert!(read == data, "streamed loose payload differs");
    assert_eq!(
        git(dir.path(), &["cat-file", "-s", &oid.to_hex()]).trim(),
        data.len().to_string()
    );
}

#[test]
fn with_stream_reads_whole_and_deltified_packed_objects() {
    let dir = init_repo();
    let odb = Odb::new(&dir.path().join(".git/objects"));
    let base = payload(2 * 1024 * 1024, 2);
    let mut edited = base.clone();
    edited.extend_from_slice(b"one more line\n");
    let base_oid = odb.write(ObjectKind::Blob, &base).expect("write base");
    let edited_oid = odb.write(ObjectKind::Blob, &edited).expect("write edited");
    let pack = git_pack(dir.path(), &[base_oid, edited_oid]);
    assert!(!odb.object_path(&base_oid).exists(), "base must be packed");
    let verify = git(
        dir.path(),
        &[
            "verify-pack",
            "-v",
            &pack.with_extension("idx").to_string_lossy(),
        ],
    );
    let deltas = verify
        .lines()
        .filter(|l| l.split_whitespace().count() >= 7)
        .count();
    assert_eq!(deltas, 1, "git should store one blob as a delta:\n{verify}");

    let odb = Odb::new(&dir.path().join(".git/objects"));
    for (oid, data) in [(&base_oid, &base), (&edited_oid, &edited)] {
        let (kind, size, read) = stream_in_chunks(&odb, oid).expect("stream");
        assert_eq!(kind, ObjectKind::Blob);
        assert_eq!(size, data.len() as u64);
        assert!(read == *data, "streamed packed payload differs for {oid}");
    }
}

#[test]
fn with_stream_missing_object_is_none_and_callback_errors_propagate() {
    let dir = init_repo();
    let odb = Odb::new(&dir.path().join(".git/objects"));
    let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").expect("oid");
    let called = std::cell::Cell::new(false);
    let result = odb
        .with_stream(&missing, |_| {
            called.set(true);
            Ok(())
        })
        .expect("missing is not an error");
    assert!(result.is_none());
    assert!(!called.get(), "callback must not run for a missing object");

    let oid = odb.write(ObjectKind::Blob, b"small").expect("write");
    let err = odb
        .with_stream(&oid, |_| -> grit_lib::error::Result<()> {
            Err(Error::Message("stop".to_owned()))
        })
        .expect_err("callback error propagates");
    assert!(matches!(err, Error::Message(m) if m == "stop"));
}
