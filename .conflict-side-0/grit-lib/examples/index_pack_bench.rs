//! Index-pack timing helper for benchmarks (`GRIT_INDEX_PACK_BENCH_PACK` path to `.pack`).
//!
//! End-to-end: build index records, write v2 `.idx` (same shape as fetch ingest without
//! staging rename). Set `GRIT_INDEX_PACK_BENCH_RECORDS_ONLY=1` for in-memory records only.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use grit_lib::objects::hash::Parallelism;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::write_v2_pack_index_with_trailer;
use grit_lib::objects::unpack_objects::pack_index_records_with_threads;

fn main() -> grit_lib::error::Result<()> {
    let pack_path = env::var("GRIT_INDEX_PACK_BENCH_PACK").expect("set GRIT_INDEX_PACK_BENCH_PACK");
    let threads: usize = env::var("GRIT_INDEX_PACK_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let records_only = env::var("GRIT_INDEX_PACK_BENCH_RECORDS_ONLY")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let pack = fs::read(&pack_path).map_err(grit_lib::error::Error::Io)?;
    let scratch = tempfile::tempdir()?;
    let git_dir = scratch.path().join(".git");
    fs::create_dir_all(git_dir.join("objects"))?;
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir);
    let start = Instant::now();
    let records =
        pack_index_records_with_threads(&pack, &odb, Parallelism::resolve(Some(threads)))?;
    if !records_only {
        let hb = odb.hash_algo().len();
        let trailer = &pack[pack.len() - hb..];
        let entries: Vec<_> = records
            .into_iter()
            .map(|r| (r.oid, r.offset, r.crc32))
            .collect();
        let idx_path: PathBuf = scratch.path().join("out.idx");
        write_v2_pack_index_with_trailer(&idx_path, &entries, trailer, hb, Some(threads))?;
    }
    eprintln!(
        "grit index-pack threads={threads}{}: {:.3}s",
        if records_only {
            " (records only)"
        } else {
            " (e2e + idx)"
        },
        start.elapsed().as_secs_f64()
    );
    let _ = Path::new(&pack_path);
    Ok(())
}
