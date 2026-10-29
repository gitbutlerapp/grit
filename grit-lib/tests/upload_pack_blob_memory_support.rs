use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use grit_lib::objects::pack_objects::{
    write_upload_pack_stream, PackBuildOptions, PackProgress, PackStats,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

/// Incompressible payload size for memory regression tests (well below CI disk limits).
pub const BLOB_BYTES: usize = 48 * 1024 * 1024;
pub const MIB: u64 = 1024 * 1024;

/// Write `len` bytes of pseudo-random data (zlib does not shrink it materially in a pack).
pub fn write_incompressible_blob(path: &Path, len: usize) {
    let mut f = std::fs::File::create(path).expect("create blob");
    let mut urandom = std::fs::File::open("/dev/urandom").expect("urandom");
    let mut buf = [0u8; 1024 * 1024];
    let mut written = 0usize;
    while written < len {
        let n = (len - written).min(buf.len());
        urandom.read_exact(&mut buf[..n]).expect("read urandom");
        f.write_all(&buf[..n]).expect("write blob");
        written += n;
    }
}

pub fn anon_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("RssAnon")
}

pub fn reset_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

pub fn git_index_pack_strict(pack_path: &std::path::Path) {
    let scratch = tempfile::tempdir().expect("scratch");
    Command::new("git")
        .current_dir(scratch.path())
        .args(["init", "-q", "--bare"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    let out = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", "--strict", pack_path.to_str().expect("path")])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        out.status.success(),
        "git index-pack: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
}

pub fn pack_options(use_bitmaps: bool) -> PackBuildOptions {
    PackBuildOptions {
        reuse_objects: true,
        use_bitmaps,
        ..PackBuildOptions::default()
    }
}

pub fn stream_blob_to_sink(
    repo: &Repository,
    want: ObjectId,
    opts: &PackBuildOptions,
) -> PackStats {
    write_upload_pack_stream(
        repo,
        &[want],
        &[],
        &HashSet::new(),
        &HashSet::new(),
        None,
        opts,
        &mut |_p: PackProgress| {},
        &mut std::io::sink(),
    )
    .expect("upload-pack stream")
}

pub fn write_blob_pack_file(
    repo: &Repository,
    want: ObjectId,
    path: &std::path::Path,
    opts: &PackBuildOptions,
) {
    let mut file = std::fs::File::create(path).expect("create pack");
    write_upload_pack_stream(
        repo,
        &[want],
        &[],
        &HashSet::new(),
        &HashSet::new(),
        None,
        opts,
        &mut |_p: PackProgress| {},
        &mut file,
    )
    .expect("upload-pack stream");
}
