//! GC/repack with `pack.compression=0` / `core.compression=0` uses stored zlib (issue #1653).

use std::io::Write;
use std::path::Path;
use std::process::Command;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::maintenance::GcOptions;
use grit_lib::objects::HashAlgo;
use grit_lib::repo::Repository;
use tempfile::TempDir;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn seed_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "gc.auto", "0"]);
    git(dir, &["config", "pack.threads", "1"]);
    std::fs::write(
        dir.join("data.txt"),
        b"seed blob for compression level zero\n",
    )
    .expect("write");
    git(dir, &["add", "data.txt"]);
    git(dir, &["commit", "-q", "-m", "seed"]);
    for i in 0..8 {
        std::fs::write(
            dir.join("data.txt"),
            format!("seed blob iteration {i} with padding\n"),
        )
        .expect("write");
        git(dir, &["add", "data.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
}

fn pack_path(git_dir: &Path) -> std::path::PathBuf {
    let pack_dir = git_dir.join("objects/pack");
    std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
        .expect("one pack")
        .path()
}

fn read_type_size(b: &[u8]) -> (u8, u64, usize) {
    let mut c = b[0];
    let type_code = (c >> 4) & 0x7;
    let mut size = (c & 0x0f) as u64;
    let mut shift = 4u32;
    let mut i = 1usize;
    while c & 0x80 != 0 {
        c = b[i];
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
        i += 1;
    }
    (type_code, size, i)
}

fn zlib_compress_level_zero(payload: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::none());
    enc.write_all(payload).expect("write");
    enc.finish().expect("finish")
}

/// Level 0 uses FLEVEL 0 in the zlib header (Git `pack.compression=0` / flate2 `Compression::none()`).
fn assert_level_zero_zlib_member(zlib: &[u8], expected_plain: &[u8]) {
    assert!(
        zlib.len() >= 2,
        "zlib member too short (len {})",
        zlib.len()
    );
    let flevel = (zlib[1] >> 6) & 3;
    assert_eq!(flevel, 0, "expected FLEVEL 0 (stored level), got {flevel}");
    let expect = zlib_compress_level_zero(expected_plain);
    assert_eq!(
        zlib, expect,
        "pack zlib must match Compression::none() for this payload"
    );
}

fn assert_pack_uses_level_zero_zlib(pack: &[u8]) {
    assert!(pack.starts_with(b"PACK"));
    let count = u32::from_be_bytes(pack[8..12].try_into().expect("count")) as usize;
    let mut pos = 12usize;
    for _ in 0..count {
        let (ty, size, hdr) = read_type_size(&pack[pos..]);
        pos += hdr;
        if ty == 6 {
            while pack[pos] & 0x80 != 0 {
                pos += 1;
            }
            pos += 1;
        } else if ty == 7 {
            pos += HashAlgo::Sha1.len();
        }
        let zlib = &pack[pos..];
        let plain_len = usize::try_from(size).expect("object size");
        let mut dec = flate2::read::ZlibDecoder::new(zlib);
        let mut plain = vec![0u8; plain_len];
        use std::io::Read;
        dec.read_exact(&mut plain).expect("inflate pack object");
        let consumed = dec.total_in() as usize;
        assert_level_zero_zlib_member(&zlib[..consumed], &plain);
        pos += consumed;
    }
}

fn gc_with_compression_config(dir: &Path, config_lines: &[&str]) {
    for line in config_lines {
        let (key, value) = line.split_once('=').expect("key=value");
        git(dir, &["config", key, value]);
    }
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    env.git_config_nosystem = Some("1".into());
    let repo =
        Repository::discover(Some(dir), &RepositoryOptions::with_environment(env)).expect("open");
    let mut opts = GcOptions::with_reflog_policy(1_700_000_000, false);
    opts.repack.threads = Some(1);
    repo.maintenance()
        .gc(
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("grit gc");
    git(dir, &["fsck", "--strict"]);
    let pack = std::fs::read(pack_path(&dir.join(".git"))).expect("read pack");
    assert_pack_uses_level_zero_zlib(&pack);
    let idx = pack_path(&dir.join(".git")).with_extension("idx");
    let idx_arg = format!(
        ".git/objects/pack/{}",
        idx.file_name().and_then(|s| s.to_str()).expect("idx name")
    );
    git(dir, &["verify-pack", &idx_arg]);
}

#[test]
fn gc_repack_pack_compression_zero_uses_stored_zlib() {
    let dir = TempDir::new().expect("tempdir");
    seed_repo(dir.path());
    gc_with_compression_config(dir.path(), &["pack.compression=0"]);
}

#[test]
fn gc_repack_core_compression_zero_uses_stored_zlib() {
    let dir = TempDir::new().expect("tempdir");
    seed_repo(dir.path());
    gc_with_compression_config(dir.path(), &["core.compression=0"]);
}
