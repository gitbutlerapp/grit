//! Packed verbatim reuse while serving a large blob must not retain the payload in anon RSS.

#![cfg(all(target_os = "linux", feature = "full-tests"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod upload_pack_blob_memory_support;

use upload_pack_blob_memory_support::{
    anon_rss_kib, git_index_pack_strict, pack_options, reset_peak_rss, stream_blob_to_sink,
    write_blob_pack_file, write_incompressible_blob, BLOB_BYTES, MIB,
};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::pack::packed_full_object_slice;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::{HashAlgo, RepoFixture};

#[test]
fn upload_pack_packed_blob_peak_memory() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("repo");
    fx.git(&["config", "core.bigFileThreshold", "1m"]);
    let blob_path = fx.path().join("big.bin");
    write_incompressible_blob(&blob_path, BLOB_BYTES);
    fx.git(&["add", "big.bin"]);
    fx.git(&["commit", "-qm", "big"]);
    fx.git(&["repack", "-adb"]);
    fx.git(&["config", "pack.writeBitmapHashCache", "true"]);
    fx.git(&["repack", "-adb"]);
    let want: ObjectId = fx
        .git(&["rev-parse", "HEAD:big.bin"])
        .stdout
        .trim()
        .parse()
        .expect("oid");

    let repo = Repository::discover(Some(fx.path()), &RepositoryOptions::empty()).expect("open");
    assert_eq!(
        repo.odb().big_file_threshold().expect("threshold"),
        1024 * 1024
    );
    assert!(
        packed_full_object_slice(repo.odb().objects_dir(), &want)
            .expect("slice")
            .is_some(),
        "blob must be a whole pack entry for reuse"
    );
    let opts = pack_options(true);
    grit_lib::objects::pack::clear_pack_cache();
    reset_peak_rss();
    let before = anon_rss_kib();
    let stats = stream_blob_to_sink(&repo, want, &opts);
    let growth_kib = anon_rss_kib().saturating_sub(before);
    assert!(
        stats.verbatim_reuse_bytes > 0,
        "expected verbatim reuse, got stats {stats:?}"
    );
    assert!(
        growth_kib < 32 * 1024,
        "packed blob reuse anon RSS grew by {} MiB (verbatim {} bytes) for a {} MiB object",
        growth_kib / 1024,
        stats.verbatim_reuse_bytes,
        BLOB_BYTES as u64 / MIB
    );

    let pack_path = fx.path().join("measured.pack");
    write_blob_pack_file(&repo, want, &pack_path, &opts);
    git_index_pack_strict(&pack_path);
}
