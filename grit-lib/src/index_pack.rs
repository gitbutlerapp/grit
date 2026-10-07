//! Install a received packfile into the object store (`index-pack` path).
//!
//! Fetch and clone keep the negotiated pack on disk (`.pack` + `.idx`) instead of
//! exploding every object into loose storage. Small receive-pack pushes may still
//! use [`crate::unpack_objects`] via [`crate::receive_pack::should_use_unpack_objects`].

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::pack::{clear_pack_cache, write_v2_pack_index};
use crate::transfer::fix_thin_pack;
use crate::unpack_objects::{pack_index_records_from_bytes, PackIndexRecord};

/// Options controlling how a received pack is ingested.
#[derive(Debug, Clone, Default)]
pub struct IngestPackOptions {
    /// When true, append missing ref-delta bases from `odb` before indexing
    /// (matches `git index-pack --fix-thin`).
    pub fix_thin: bool,
}

/// Ingest a pack received from fetch/clone by installing it under `objects/pack/`.
///
/// Fetch and clone always index the received pack (Git `fetch-pack` / `index-pack`).
/// Call [`crate::receive_pack::should_use_unpack_objects`] only on the receive-pack path.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the pack cannot be validated or indexed,
/// or I/O / zlib failures from indexing.
pub fn ingest_received_pack(pack: Vec<u8>, odb: &Odb, opts: &IngestPackOptions) -> Result<()> {
    if pack.is_empty() {
        return Ok(());
    }
    if pack.len() < 12 || &pack[0..4] != b"PACK" {
        return Err(Error::CorruptObject(
            "received data is not a pack stream".to_owned(),
        ));
    }
    install_pack_bytes(pack, odb, opts)
}

/// Write `pack` into `odb`'s `objects/pack/` directory with a v2 index.
///
/// # Errors
///
/// Same as [`ingest_received_pack`].
pub fn install_pack_bytes(pack: Vec<u8>, odb: &Odb, opts: &IngestPackOptions) -> Result<()> {
    let pack = if opts.fix_thin {
        fix_thin_pack(pack, odb)?
    } else {
        pack
    };
    let hb = odb.hash_algo().len();
    if pack.len() < 12 + hb {
        return Err(Error::CorruptObject("pack too small".to_owned()));
    }
    let pack_hash = ObjectId::from_bytes(&pack[pack.len() - hb..])?;
    let stem = format!("pack-{}", pack_hash.to_hex());
    let pack_dir = odb.objects_dir().join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    std::fs::write(&pack_path, &pack).map_err(Error::Io)?;

    let records = pack_index_records_from_bytes(&pack, odb)?;
    let entries: Vec<(ObjectId, u64, u32)> = records
        .into_iter()
        .map(|PackIndexRecord { oid, offset, crc32 }| (oid, offset, crc32))
        .collect();
    write_v2_pack_index(&idx_path, &pack_path, &entries, hb)?;
    clear_pack_cache();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use crate::pack::read_pack_index;
    use std::process::Command;

    fn git_index_pack(pack: &[u8]) -> (tempfile::TempDir, Vec<u8>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("in.pack");
        std::fs::write(&pack_path, pack).expect("write");
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(["index-pack", "-v", &pack_path.to_string_lossy()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git index-pack");
        assert!(
            out.status.success(),
            "git index-pack: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let indexed = std::fs::read(dir.path().join("in.pack")).expect("read indexed pack");
        (dir, indexed)
    }

    fn single_blob_pack(data: &[u8]) -> Vec<u8> {
        use crate::transfer::{build_pack, PackBuildOptions};
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let oid = odb
            .write(ObjectKind::Blob, data)
            .expect("write loose blob for pack build");
        build_pack(
            &odb,
            &[oid],
            &[],
            &PackBuildOptions {
                delta: false,
                ..Default::default()
            },
        )
        .expect("build pack")
    }

    #[test]
    fn install_pack_matches_git_index_pack_layout() {
        let blob = b"clone pack retention fixture\n";
        let pack = single_blob_pack(blob);
        let (_git_dir, git_pack) = git_index_pack(&pack);

        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        install_pack_bytes(git_pack, &odb, &IngestPackOptions { fix_thin: false })
            .expect("install");

        let pack_dir = tmp.path().join("pack");
        let packs: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(packs.len(), 1, "expected one pack file");
        let loose_count = std::fs::read_dir(tmp.path())
            .expect("read objects")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_hexdigit()))
            })
            .count();
        assert_eq!(loose_count, 0, "install must not create loose objects");

        let oid = odb.hash(ObjectKind::Blob, blob);
        let obj = odb.read(&oid).expect("read from pack");
        assert_eq!(obj.data.as_slice(), blob);
    }

    #[test]
    fn fetch_ingest_keeps_pack_despite_receive_unpacklimit_zero() {
        let blob = b"unpacklimit must not apply to fetch ingest\n";
        let pack = single_blob_pack(blob);
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::write(git_dir.join("config"), "[receive]\n\tunpackLimit = 0\n").expect("config");
        let objects = git_dir.join("objects");
        let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
        install_pack_bytes(pack, &odb, &IngestPackOptions { fix_thin: false }).expect("install");
        let pack_dir = git_dir.join("objects").join("pack");
        let packs: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(packs.len(), 1, "fetch ingest must index pack, not unpack");
    }

    #[test]
    fn v2_index_large_offset_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pack_path = tmp.path().join("large.pack");
        let pack = single_blob_pack(b"large offset idx fixture");
        std::fs::write(&pack_path, &pack).expect("write pack");
        let idx_path = pack_path.with_extension("idx");
        let fake_oid = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let huge_off = 1u64 << 31;
        write_v2_pack_index(
            &idx_path,
            &pack_path,
            &[(fake_oid, huge_off, 0x1234_5678)],
            20,
        )
        .expect("write idx");
        let idx = read_pack_index(&idx_path).expect("read idx");
        assert_eq!(idx.entries.len(), 1);
        assert_eq!(idx.entries[0].offset, huge_off);
    }
}
