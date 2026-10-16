//! Large-index read/write and mutation coverage (index.rs hot paths).

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;

fn make_entry(path: &str, oid: ObjectId, stage: u8) -> IndexEntry {
    let mut e = IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 0,
        mtime_sec: 1,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 4,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    };
    e.flags = (e.flags & 0x0FFF) | ((stage as u16 & 3) << 12);
    e
}

#[test]
fn large_index_v2_v4_write_read_and_mutations() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let blob = repo.odb().write(ObjectKind::Blob, b"data").expect("blob");

    for version in [2u32, 4] {
        let mut index = Index::new();
        index.version = version;
        for i in 0..1500 {
            let path = format!("d{}/f{i:04}.txt", i / 50);
            index.add_or_replace(make_entry(&path, blob, 0));
        }
        if version == 4 {
            index.add_or_replace(make_entry("z/skip.txt", blob, 0));
        }
        repo.index_handle().write(&mut index).expect("write");
        let loaded = Index::load(&repo.index_handle().path()).expect("load");
        assert_eq!(loaded.version, version);
        assert_eq!(loaded.entries().len(), index.entries().len());

        let mut sorted = loaded;
        sorted.remove_paths_and_insert(
            ["d0/f0000.txt"].map(|p| p.as_bytes()),
            vec![make_entry("d0/f0000.txt", blob, 0)],
        );
        sorted.invalidate_cache_tree_for_path(b"d0");
        repo.index_handle().write(&mut sorted).expect("rewrite");
    }
}
