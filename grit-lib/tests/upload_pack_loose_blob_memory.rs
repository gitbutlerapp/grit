//! Streaming a large loose blob into a pack must not retain the payload in anon RSS.

#![cfg(all(target_os = "linux", feature = "full-tests"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod upload_pack_blob_memory_support;

use upload_pack_blob_memory_support::{
    anon_rss_kib, git_index_pack_strict, pack_options, reset_peak_rss, stream_blob_to_sink,
    write_blob_pack_file, write_incompressible_blob, BLOB_BYTES, MIB,
};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::{HashAlgo, RepoFixture};

#[test]
fn upload_pack_loose_blob_peak_memory() {
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
    let pack_dir = fx.path().join("objects/pack");
    let want: ObjectId = fx
        .git(&["rev-parse", "HEAD:big.bin"])
        .stdout
        .trim()
        .parse()
        .expect("oid");
    if pack_dir.is_dir() {
        for entry in std::fs::read_dir(&pack_dir).expect("pack dir") {
            std::fs::remove_file(entry.expect("entry").path()).expect("remove pack");
        }
    }
    fx.git(&["hash-object", "-w", "big.bin"]);

    let repo = Repository::discover(Some(fx.path()), &RepositoryOptions::empty()).expect("open");
    assert_eq!(
        repo.odb().big_file_threshold().expect("threshold"),
        1024 * 1024,
        "core.bigFileThreshold must be visible to grit"
    );
    let opts = pack_options(false);
    reset_peak_rss();
    let before = anon_rss_kib();
    let _stats = stream_blob_to_sink(&repo, want, &opts);
    let growth_kib = anon_rss_kib().saturating_sub(before);

    let pack_path = fx.path().join("measured.pack");
    write_blob_pack_file(&repo, want, &pack_path, &opts);
    git_index_pack_strict(&pack_path);
    assert!(
        growth_kib < 32 * 1024,
        "loose blob pack anon RSS grew by {} MiB for a {} MiB object",
        growth_kib / 1024,
        BLOB_BYTES as u64 / MIB
    );
}
