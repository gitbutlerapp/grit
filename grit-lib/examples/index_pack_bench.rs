//! Index-pack timing helper for benchmarks (`GRIT_INDEX_PACK_BENCH_PACK` path to `.pack`).

use std::env;
use std::fs;
use std::path::Path;
use std::time::Instant;

use grit_lib::hash::Parallelism;
use grit_lib::odb::Odb;
use grit_lib::unpack_objects::pack_index_records_with_threads;

fn main() -> grit_lib::error::Result<()> {
    let pack_path = env::var("GRIT_INDEX_PACK_BENCH_PACK").expect("set GRIT_INDEX_PACK_BENCH_PACK");
    let threads: usize = env::var("GRIT_INDEX_PACK_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let pack = fs::read(&pack_path).map_err(grit_lib::error::Error::Io)?;
    let scratch = tempfile::tempdir()?;
    let git_dir = scratch.path().join(".git");
    fs::create_dir_all(git_dir.join("objects"))?;
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir);
    let start = Instant::now();
    let _ = pack_index_records_with_threads(&pack, &odb, Parallelism::resolve(Some(threads)))?;
    eprintln!(
        "grit index-pack threads={threads}: {:.3}s",
        start.elapsed().as_secs_f64()
    );
    let _ = Path::new(&pack_path);
    Ok(())
}
