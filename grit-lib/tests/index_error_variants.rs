//! Typed [`IndexError`] variants on Git-written indexes damaged in controlled ways.

use std::fs;
use std::io::Write;

use grit_lib::error::Error;
use grit_lib::index::{Index, IndexCorruptReason, IndexError, IndexLock};
use grit_lib::objects::HashAlgo;
use grit_test_support::git;

fn git_index_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(tmp.path().join("f.txt"), b"content").expect("write file");
    git(tmp.path(), &["add", "f.txt"]);
    let index_path = tmp.path().join(".git/index");
    assert!(index_path.is_file());
    (tmp, index_path)
}

#[test]
fn index_error_variants() {
    let (_tmp, index_path) = git_index_fixture();
    let algo = HashAlgo::Sha1;

    // Structurally truncated body: header intact, entry region incomplete, trailer recomputed.
    {
        const INDEX_HEADER_LEN: usize = 12;
        let bytes = fs::read(&index_path).expect("read index");
        let hash_len = algo.len();
        let body_len = bytes.len() - hash_len;
        assert!(
            body_len > INDEX_HEADER_LEN + 8,
            "fixture index body too small"
        );
        let mut body = bytes[..body_len].to_vec();
        body.truncate(INDEX_HEADER_LEN + 8);
        let digest = algo.checksum_digest(&body);
        body.extend_from_slice(&digest);
        let trunc_path = index_path.with_extension("trunc_body");
        fs::write(&trunc_path, &body).expect("write trunc_body");
        let err = Index::load(&trunc_path).unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Index(IndexError::Corrupt {
                    corrupt,
                    ..
                }) if corrupt.path.as_deref() == Some(trunc_path.as_path())
                    && corrupt.offset == Some(12)
                    && matches!(
                        corrupt.reason,
                        IndexCorruptReason::EntryCountMismatch {
                            declared: 1,
                            max_fit: 0,
                            remaining: 8
                        }
                    )
            ),
            "trunc_body: {err:?}"
        );
        assert_eq!(err.kind_name(), "index_error");
    }

    // Bad checksum trailer.
    {
        let mut bytes = fs::read(&index_path).expect("read index");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(index_path.with_extension("badsum"), &bytes).expect("write badsum");
        let badsum = index_path.with_extension("badsum");
        let err = Index::load(&badsum).unwrap_err();
        assert!(matches!(
            &err,
            Error::Index(IndexError::Corrupt {
                corrupt,
                ..
            }) if corrupt.path.as_deref() == Some(badsum.as_path())
                && corrupt.offset.is_some()
                && matches!(corrupt.reason, IndexCorruptReason::ChecksumMismatch { .. })
        ));
        assert_eq!(err.kind_name(), "index_error");
    }

    // Unsupported index version 5 (skip checksum verify so the header is what we assert on).
    {
        let mut bytes = fs::read(&index_path).expect("read index");
        let hash_len = algo.len();
        let len = bytes.len();
        bytes[4..8].copy_from_slice(&5u32.to_be_bytes());
        bytes[len - hash_len..].fill(0);
        let err = Index::parse(
            &bytes,
            &grit_lib::index::IndexParseOptions::default().with_hash_algo(algo),
        )
        .unwrap_err();
        assert!(matches!(
            &err,
            Error::Index(IndexError::Corrupt {
                corrupt,
                ..
            }) if corrupt.offset == Some(4)
                && matches!(
                    corrupt.reason,
                    IndexCorruptReason::UnsupportedVersion { version: 5 }
                )
        ));
        assert_eq!(err.kind_name(), "index_error");
    }

    // Corrupt fsmonitor (`FSMN`) extension payload.
    {
        let mut idx = Index::load(&index_path).expect("load git index");
        idx.set_fsmonitor_last_update(Some("token".into()));
        let corrupt_path = index_path.with_extension("fsmon");
        idx.write(&corrupt_path).expect("write fsmonitor index");
        let mut bytes = fs::read(&corrupt_path).expect("read");
        let fsmn = bytes
            .windows(4)
            .position(|w| w == b"FSMN")
            .expect("FSMN extension present");
        let payload = fsmn + 8;
        bytes[payload..payload + 4].copy_from_slice(&99u32.to_be_bytes());
        let hash_len = algo.len();
        let body_len = bytes.len() - hash_len;
        let digest = algo.checksum_digest(&bytes[..body_len]);
        bytes[body_len..].copy_from_slice(&digest);
        let err = Index::parse(
            &bytes,
            &grit_lib::index::IndexParseOptions::default().with_hash_algo(algo),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Index(IndexError::FsmonitorExtensionBadVersion(99))
            ),
            "fsmonitor damage: {err:?}"
        );
        assert_eq!(err.kind_name(), "index_fsmonitor_extension_bad_version");
    }

    // Held `index.lock` → top-level lock (legacy slug unchanged).
    {
        let lock_path = index_path.with_file_name("index.lock");
        let mut lock = fs::File::create(&lock_path).expect("create lock");
        lock.write_all(b"held").expect("write lock");
        let acquire_err = match IndexLock::acquire(&index_path, None) {
            Err(e) => e,
            Ok(_) => panic!("expected lock held"),
        };
        assert!(matches!(acquire_err, Error::LockHeld(_)));
        assert_eq!(acquire_err.kind_name(), "lock_held");
        let mut idx = Index::load(&index_path).expect("load");
        let write_err = match idx.write(&index_path) {
            Err(e) => e,
            Ok(()) => panic!("expected write to fail under lock"),
        };
        assert!(matches!(write_err, Error::LockHeld(_)));
        drop(lock);
        let _ = fs::remove_file(lock_path);
    }
}
