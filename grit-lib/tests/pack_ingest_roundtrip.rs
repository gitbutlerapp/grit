//! Pack ingest, v2 index + RIDX reverse index, and unpack-objects (upstream t5300/t5325/t5351).
//!
//! Asserts byte-identical `.idx` and `.rev` against `git index-pack --rev-index`, exercises
//! [`grit_lib::index_pack::install_pack_bytes`] / [`grit_lib::index_pack::ingest_received_pack`]
//! under every [`grit_lib::index_pack::IngestPackOptions`] variant, and cross-checks installed
//! packs with `git verify-pack -v` and `git fsck --strict`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use flate2::write::ZlibEncoder;
use grit_lib::hash::Parallelism;
use grit_lib::index::MODE_REGULAR;
use grit_lib::index_pack::{ingest_received_pack, install_pack_bytes, IngestPackOptions};
use grit_lib::objects::{serialize_tree, HashAlgo, ObjectId, ObjectKind, TreeEntry};
use grit_lib::odb::Odb;
use grit_lib::pack::{read_pack_index, write_v2_pack_index, write_v2_pack_index_with_trailer};
use grit_lib::pack_rev::{
    append_hashfile_checksum, build_pack_rev_bytes, build_pack_rev_bytes_from_index_order_offsets,
    build_pack_rev_bytes_from_index_order_offsets_and_checksum, hashfile_checksum_valid,
    hashfile_checksum_valid_sha1, pack_rev_fsck_messages, rev_path_for_index,
    try_rev_positions_in_pack_order, verify_pack_rev_file, verify_pack_rev_file_contents,
    RIDX_HASH_ID_SHA1, RIDX_HASH_ID_SHA256, RIDX_SIGNATURE, RIDX_VERSION,
};
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_lib::unpack_objects::{
    pack_index_records_with_threads, pack_is_thin, strict_verify_packed_references, unpack_objects,
    UnpackOptions,
};
use grit_test_support::git;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
    ("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200"),
    ("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200"),
];

const INGEST_VARIANTS: &[IngestPackOptions] = &[
    IngestPackOptions {
        fix_thin: false,
        threads: Some(1),
    },
    IngestPackOptions {
        fix_thin: false,
        threads: Some(8),
    },
    IngestPackOptions {
        fix_thin: false,
        threads: None,
    },
    IngestPackOptions {
        fix_thin: true,
        threads: Some(1),
    },
    IngestPackOptions {
        fix_thin: true,
        threads: Some(8),
    },
    IngestPackOptions {
        fix_thin: true,
        threads: None,
    },
];

#[derive(Clone, Copy, Debug)]
enum PackShape {
    GitWhole,
    GitDelta,
    GritWhole,
    GritDelta,
}

impl PackShape {
    fn label(self) -> &'static str {
        match self {
            Self::GitWhole => "git-whole",
            Self::GitDelta => "git-delta",
            Self::GritWhole => "grit-whole",
            Self::GritDelta => "grit-delta",
        }
    }
}

struct PackFixture {
    bytes: Vec<u8>,
    algo: HashAlgo,
    _keep: tempfile::TempDir,
}

fn git_run(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_try(dir: &Path, args: &[&str]) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

fn init_repo(dir: &Path, sha256: bool) {
    if sha256 {
        assert!(
            git_try(
                dir,
                &["init", "-q", "--object-format=sha256", "-b", "main", "."]
            ),
            "system git lacks sha256 object format"
        );
    } else {
        git_run(dir, &["init", "-q", "-b", "main", "."]);
    }
}

fn read_pack_from_repo(git_dir: &Path) -> Vec<u8> {
    let pack_dir = git_dir.join("objects/pack");
    let pack_path = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack file");
    std::fs::read(&pack_path).expect("read pack")
}

fn build_git_whole_pack(sha256: bool) -> PackFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir, sha256);
    std::fs::write(dir.join("one.txt"), b"solo blob\n").expect("write");
    git_run(dir, &["add", "one.txt"]);
    git_run(dir, &["commit", "-q", "-m", "one"]);
    git_run(dir, &["repack", "-adf", "-q"]);
    let pack = read_pack_from_repo(&dir.join(".git"));
    let algo = if sha256 {
        HashAlgo::Sha256
    } else {
        HashAlgo::Sha1
    };
    PackFixture {
        bytes: pack,
        algo,
        _keep: tmp,
    }
}

fn build_git_delta_pack(sha256: bool) -> PackFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir, sha256);
    std::fs::write(dir.join("data.txt"), b"seed payload for deltas\n").expect("write");
    git_run(dir, &["add", "data.txt"]);
    git_run(dir, &["commit", "-q", "-m", "seed"]);
    for i in 1..=12 {
        let payload = format!("seed payload for deltas — revision {i}\n");
        std::fs::write(dir.join("data.txt"), payload).expect("write");
        git_run(dir, &["commit", "-am", &format!("edit {i}")]);
    }
    git_run(dir, &["repack", "-adf", "--depth=50", "-q"]);
    let pack = read_pack_from_repo(&dir.join(".git"));
    let algo = if sha256 {
        HashAlgo::Sha256
    } else {
        HashAlgo::Sha1
    };
    PackFixture {
        bytes: pack,
        algo,
        _keep: tmp,
    }
}

fn open_odb_for_algo(sha256: bool) -> (tempfile::TempDir, Odb) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
    if sha256 {
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectFormat = sha256\n",
        )
        .expect("config");
    }
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir);
    (tmp, odb)
}

fn build_grit_pack(sha256: bool, delta: bool) -> PackFixture {
    let (tmp, odb) = open_odb_for_algo(sha256);
    let algo = odb.hash_algo();
    let mut oids = Vec::new();
    for i in 0..8 {
        let data = format!("grit pack builder blob {i}\n");
        let oid = odb
            .write(ObjectKind::Blob, data.as_bytes())
            .expect("write blob");
        oids.push(oid);
    }
    let pack = build_pack(
        &odb,
        &oids,
        &[],
        &PackBuildOptions {
            delta,
            ..Default::default()
        },
    )
    .expect("build_pack");
    PackFixture {
        bytes: pack,
        algo,
        _keep: tmp,
    }
}

static GIT_WHOLE_SHA1: OnceLock<PackFixture> = OnceLock::new();
static GIT_DELTA_SHA1: OnceLock<PackFixture> = OnceLock::new();
static GRIT_WHOLE_SHA1: OnceLock<PackFixture> = OnceLock::new();
static GRIT_DELTA_SHA1: OnceLock<PackFixture> = OnceLock::new();

static GIT_WHOLE_SHA256: OnceLock<Option<PackFixture>> = OnceLock::new();
static GIT_DELTA_SHA256: OnceLock<Option<PackFixture>> = OnceLock::new();
static GRIT_WHOLE_SHA256: OnceLock<Option<PackFixture>> = OnceLock::new();
static GRIT_DELTA_SHA256: OnceLock<Option<PackFixture>> = OnceLock::new();

fn shared_pack(shape: PackShape, algo: HashAlgo) -> Option<&'static PackFixture> {
    match (shape, algo) {
        (PackShape::GitWhole, HashAlgo::Sha1) => {
            Some(GIT_WHOLE_SHA1.get_or_init(|| build_git_whole_pack(false)))
        }
        (PackShape::GitDelta, HashAlgo::Sha1) => {
            Some(GIT_DELTA_SHA1.get_or_init(|| build_git_delta_pack(false)))
        }
        (PackShape::GritWhole, HashAlgo::Sha1) => {
            Some(GRIT_WHOLE_SHA1.get_or_init(|| build_grit_pack(false, false)))
        }
        (PackShape::GritDelta, HashAlgo::Sha1) => {
            Some(GRIT_DELTA_SHA1.get_or_init(|| build_grit_pack(false, true)))
        }
        (PackShape::GitWhole, HashAlgo::Sha256) => GIT_WHOLE_SHA256
            .get_or_init(|| {
                let probe = tempfile::tempdir().ok()?;
                if !git_try(probe.path(), &["init", "--object-format=sha256"]) {
                    return None;
                }
                drop(probe);
                Some(build_git_whole_pack(true))
            })
            .as_ref(),
        (PackShape::GitDelta, HashAlgo::Sha256) => GIT_DELTA_SHA256
            .get_or_init(|| {
                let probe = tempfile::tempdir().ok()?;
                if !git_try(probe.path(), &["init", "--object-format=sha256"]) {
                    return None;
                }
                drop(probe);
                Some(build_git_delta_pack(true))
            })
            .as_ref(),
        (PackShape::GritWhole, HashAlgo::Sha256) => GRIT_WHOLE_SHA256
            .get_or_init(|| {
                let probe = tempfile::tempdir().ok()?;
                if !git_try(probe.path(), &["init", "--object-format=sha256"]) {
                    return None;
                }
                drop(probe);
                Some(build_grit_pack(true, false))
            })
            .as_ref(),
        (PackShape::GritDelta, HashAlgo::Sha256) => GRIT_DELTA_SHA256
            .get_or_init(|| {
                let probe = tempfile::tempdir().ok()?;
                if !git_try(probe.path(), &["init", "--object-format=sha256"]) {
                    return None;
                }
                drop(probe);
                Some(build_grit_pack(true, true))
            })
            .as_ref(),
    }
}

fn git_index_pack_rev(pack: &[u8], algo: HashAlgo) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let dir = tempfile::tempdir().expect("scratch");
    if algo == HashAlgo::Sha256 {
        git_run(
            dir.path(),
            &["init", "-q", "--object-format=sha256", "-b", "main", "."],
        );
    } else {
        git_run(dir.path(), &["init", "-q", "-b", "main", "."]);
    }
    let pack_path = dir.path().join("fixture.pack");
    std::fs::write(&pack_path, pack).expect("write pack");
    git_run(
        dir.path(),
        &[
            "index-pack",
            "--rev-index",
            pack_path
                .file_name()
                .and_then(|s| s.to_str())
                .expect("name"),
        ],
    );
    let idx = std::fs::read(dir.path().join("fixture.idx")).expect("git idx");
    let rev = std::fs::read(dir.path().join("fixture.rev")).expect("git rev");
    let pack_on_disk = std::fs::read(&pack_path).expect("indexed pack");
    (idx, rev, pack_on_disk)
}

fn grit_idx_rev_bytes(
    pack: &[u8],
    odb: &Odb,
    threads: usize,
    use_write_v2_pack_index: bool,
) -> (Vec<u8>, Vec<u8>) {
    let parallelism = Parallelism::resolve(Some(threads));
    let records = pack_index_records_with_threads(pack, odb, parallelism).expect("index records");
    let hb = odb.hash_algo().len();
    let trailer = &pack[pack.len() - hb..];
    let entries: Vec<_> = records.iter().map(|r| (r.oid, r.offset, r.crc32)).collect();
    let scratch = tempfile::tempdir().expect("idx scratch");
    let pack_path = scratch.path().join("in.pack");
    let idx_path = scratch.path().join("in.idx");
    std::fs::write(&pack_path, pack).expect("write pack");
    if use_write_v2_pack_index {
        write_v2_pack_index(&idx_path, &pack_path, &entries, hb).expect("write_v2_pack_index");
    } else {
        write_v2_pack_index_with_trailer(&idx_path, &entries, trailer, hb).expect("write idx");
    }
    let idx_bytes = std::fs::read(&idx_path).expect("read idx");
    let index = read_pack_index(&idx_path).expect("parse idx");
    let offsets: Vec<u64> = index.iter().map(|e| e.offset()).collect();
    let rev_bytes = build_pack_rev_bytes_from_index_order_offsets_and_checksum(&offsets, trailer);
    (idx_bytes, rev_bytes)
}

fn assert_idx_rev_matches_git(pack: &[u8], algo: HashAlgo, shape: PackShape) {
    let tmp = tempfile::tempdir().expect("odb");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
    if algo == HashAlgo::Sha256 {
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectFormat = sha256\n",
        )
        .expect("config");
    }
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir);
    assert_eq!(odb.hash_algo(), algo);

    let (git_idx, git_rev, pack_on_disk) = git_index_pack_rev(pack, algo);
    for use_pack_path in [false, true] {
        for threads in [1_usize, 8] {
            let (grit_idx, grit_rev) =
                grit_idx_rev_bytes(&pack_on_disk, &odb, threads, use_pack_path);
            assert_eq!(
                grit_idx,
                git_idx,
                "{} {} threads={threads} write_via_pack_path={use_pack_path}: idx mismatch",
                shape.label(),
                algo.name()
            );
            assert_eq!(
                grit_rev,
                git_rev,
                "{} {} threads={threads} write_via_pack_path={use_pack_path}: rev mismatch",
                shape.label(),
                algo.name()
            );
        }
    }
}

fn git_verify_and_fsck(pack_dir: &Path, algo: HashAlgo) {
    let idx_path = std::fs::read_dir(pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("idx");
    let mut verify_args = vec!["verify-pack", "-v"];
    let fmt = "sha256";
    if algo == HashAlgo::Sha256 {
        verify_args.push("--object-format");
        verify_args.push(fmt);
    }
    verify_args.push(
        idx_path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("idx name"),
    );
    let out = Command::new("git")
        .current_dir(pack_dir)
        .args(&verify_args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "git verify-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let git_dir = pack_dir.parent().unwrap().parent().unwrap();
    let git_dir_arg = git_dir.to_string_lossy().into_owned();
    let mut fsck_args = vec![
        "--git-dir",
        git_dir_arg.as_str(),
        "fsck",
        "--strict",
        "--no-progress",
    ];
    if algo == HashAlgo::Sha256 {
        fsck_args.push("--object-format");
        fsck_args.push(fmt);
    }
    let out = Command::new("git")
        .args(&fsck_args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "git fsck --strict: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn install_into_git_repo(
    pack: Vec<u8>,
    algo: HashAlgo,
    opts: &IngestPackOptions,
) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("install repo");
    let git_dir = tmp.path().join("repo.git");
    if algo == HashAlgo::Sha256 {
        git_run(
            tmp.path(),
            &["init", "--bare", "-q", "--object-format=sha256", "repo.git"],
        );
    } else {
        git_run(tmp.path(), &["init", "--bare", "-q", "repo.git"]);
    }
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir.clone());
    install_pack_bytes(pack, &odb, opts).expect("install pack");
    (tmp, git_dir)
}

#[test]
fn idx_and_rev_match_git_for_all_pack_shapes_sha1() {
    for shape in [
        PackShape::GitWhole,
        PackShape::GitDelta,
        PackShape::GritWhole,
        PackShape::GritDelta,
    ] {
        let fx = shared_pack(shape, HashAlgo::Sha1).expect("fixture");
        assert_idx_rev_matches_git(&fx.bytes, fx.algo, shape);
    }
}

#[test]
fn idx_and_rev_match_git_for_all_pack_shapes_sha256() {
    for shape in [
        PackShape::GitWhole,
        PackShape::GitDelta,
        PackShape::GritWhole,
        PackShape::GritDelta,
    ] {
        let Some(fx) = shared_pack(shape, HashAlgo::Sha256) else {
            eprintln!("skip: sha256 object format unavailable ({})", shape.label());
            continue;
        };
        assert_idx_rev_matches_git(&fx.bytes, fx.algo, shape);
    }
}

#[test]
fn pack_index_records_with_threads_matches_single_thread() {
    let fx = shared_pack(PackShape::GitDelta, HashAlgo::Sha1).expect("fixture");
    let tmp = tempfile::tempdir().expect("odb");
    let odb = Odb::new(tmp.path());
    let one = pack_index_records_with_threads(&fx.bytes, &odb, Parallelism::resolve(Some(1)))
        .expect("threads=1");
    let eight = pack_index_records_with_threads(&fx.bytes, &odb, Parallelism::resolve(Some(8)))
        .expect("threads=8");
    assert_eq!(one.len(), eight.len());
    for (a, b) in one.iter().zip(&eight) {
        assert_eq!(a.oid, b.oid);
        assert_eq!(a.offset, b.offset);
        assert_eq!(a.crc32, b.crc32);
    }
}

#[test]
fn ingest_every_option_passes_git_verify_and_fsck_sha1() {
    for shape in [
        PackShape::GitWhole,
        PackShape::GitDelta,
        PackShape::GritWhole,
        PackShape::GritDelta,
    ] {
        let fx = shared_pack(shape, HashAlgo::Sha1).expect("fixture");
        for opts in INGEST_VARIANTS {
            let (_keep, git_dir) = install_into_git_repo(fx.bytes.clone(), fx.algo, opts);
            git_verify_and_fsck(&git_dir.join("objects/pack"), fx.algo);
        }
    }
}

#[test]
fn ingest_received_pack_matches_install_sha1() {
    let fx = shared_pack(PackShape::GitDelta, HashAlgo::Sha1).expect("fixture");
    for opts in INGEST_VARIANTS {
        let tmp = tempfile::tempdir().expect("odb");
        let odb = Odb::new(tmp.path().join("objects").as_path());
        let via_install = install_pack_bytes(fx.bytes.clone(), &odb, opts).expect("install");
        let tmp2 = tempfile::tempdir().expect("odb2");
        let odb2 = Odb::new(tmp2.path().join("objects").as_path());
        let via_ingest = ingest_received_pack(fx.bytes.clone(), &odb2, opts).expect("ingest");
        assert_eq!(via_install, via_ingest, "oid sets differ for {opts:?}");
    }
}

#[test]
fn pack_is_thin_classifies_thin_and_full_packs() {
    let full = shared_pack(PackShape::GritWhole, HashAlgo::Sha1).expect("fixture");
    assert!(
        !pack_is_thin(&full.bytes, full.algo),
        "whole-object pack must not be thin"
    );

    let delta_fx = shared_pack(PackShape::GitDelta, HashAlgo::Sha1).expect("delta fixture");
    assert!(
        !pack_is_thin(&delta_fx.bytes, delta_fx.algo),
        "full git delta pack must not be thin"
    );

    let tmp = tempfile::tempdir().expect("thin");
    init_repo(tmp.path(), false);
    let base = b"0".repeat(200_000);
    std::fs::write(tmp.path().join("big.txt"), &base).expect("write");
    git_run(tmp.path(), &["add", "big.txt"]);
    git_run(tmp.path(), &["commit", "-q", "-m", "base"]);
    let have = ObjectId::from_hex(git(tmp.path(), &["rev-parse", "HEAD"]).trim()).unwrap();
    let mut tip_blob = base.clone();
    tip_blob.extend_from_slice(b"\nappend for thin delta\n");
    std::fs::write(tmp.path().join("big.txt"), &tip_blob).expect("write");
    git_run(tmp.path(), &["commit", "-am", "tip"]);
    let want = ObjectId::from_hex(git(tmp.path(), &["rev-parse", "HEAD"]).trim()).unwrap();
    let odb = Odb::new(tmp.path().join(".git/objects").as_path())
        .with_config_git_dir(tmp.path().join(".git"));
    let thin = build_pack(
        &odb,
        &[want],
        &[have],
        &PackBuildOptions {
            delta: true,
            thin: true,
            ..Default::default()
        },
    )
    .expect("thin pack");
    assert!(pack_is_thin(&thin, HashAlgo::Sha1));
}

#[test]
fn verify_pack_rev_accepts_git_rev_and_rejects_corruption() {
    let fx = shared_pack(PackShape::GitDelta, HashAlgo::Sha1).expect("fixture");
    let (git_idx, git_rev, pack_on_disk) = git_index_pack_rev(&fx.bytes, fx.algo);
    let scratch = tempfile::tempdir().expect("idx");
    let idx_path = scratch.path().join("f.idx");
    std::fs::write(&idx_path, &git_idx).expect("write idx");
    let index = read_pack_index(&idx_path).expect("read idx");
    verify_pack_rev_file_contents(&git_rev, &index, "fixture.rev").expect("valid git rev");

    let positions = try_rev_positions_in_pack_order(&git_rev, index.len()).expect("rev order");
    assert_eq!(positions.len(), index.len());

    let mut bad_sig = git_rev.clone();
    bad_sig[0..4].copy_from_slice(&0xDEADBEEF_u32.to_be_bytes());
    assert!(
        verify_pack_rev_file_contents(&bad_sig, &index, "bad.rev").is_err(),
        "bad signature must fail"
    );

    let mut bad_ver = git_rev.clone();
    bad_ver[4..8].copy_from_slice(&99u32.to_be_bytes());
    assert!(verify_pack_rev_file_contents(&bad_ver, &index, "bad.rev").is_err());

    let mut bad_hash_id = git_rev.clone();
    bad_hash_id[8..12].copy_from_slice(&99u32.to_be_bytes());
    assert!(verify_pack_rev_file_contents(&bad_hash_id, &index, "bad.rev").is_err());

    let mut bad_perm = git_rev.clone();
    let n = index.len();
    if n >= 2 {
        let pos = 12usize;
        let first = u32::from_be_bytes(bad_perm[pos..pos + 4].try_into().unwrap());
        bad_perm[pos..pos + 4].copy_from_slice(&first.to_be_bytes()); // duplicate on purpose
        bad_perm[pos + 4..pos + 8].copy_from_slice(&first.to_be_bytes());
        assert!(verify_pack_rev_file_contents(&bad_perm, &index, "bad.rev").is_err());
    }

    let mut bad_checksum = git_rev.clone();
    let hb = fx.algo.len();
    let last = bad_checksum.len();
    bad_checksum[last - hb] ^= 0xFF;
    assert!(verify_pack_rev_file_contents(&bad_checksum, &index, "bad.rev").is_err());

    let truncated = &git_rev[..git_rev.len().saturating_sub(hb + 4)];
    assert!(verify_pack_rev_file_contents(truncated, &index, "bad.rev").is_err());

    let _ = pack_on_disk;
}

#[test]
fn hashfile_checksum_append_and_validate() {
    let body = b"RIDX test body";
    let mut out = body.to_vec();
    append_hashfile_checksum(&mut out, HashAlgo::Sha1.len());
    assert!(hashfile_checksum_valid(&out, HashAlgo::Sha1.len()));
    out.pop();
    assert!(!hashfile_checksum_valid(&out, HashAlgo::Sha1.len()));
}

#[test]
fn try_rev_positions_rejects_malformed_ridx() {
    let fx = shared_pack(PackShape::GitWhole, HashAlgo::Sha1).expect("fixture");
    let (_, git_rev, _) = git_index_pack_rev(&fx.bytes, fx.algo);
    let scratch = tempfile::tempdir().expect("idx");
    let idx_path = scratch.path().join("f.idx");
    let (git_idx, _, _) = git_index_pack_rev(&fx.bytes, fx.algo);
    std::fs::write(&idx_path, &git_idx).expect("write idx");
    let index = read_pack_index(&idx_path).expect("read idx");
    let n = index.len();

    assert!(try_rev_positions_in_pack_order(&git_rev, n).is_some());
    assert!(try_rev_positions_in_pack_order(&git_rev, n + 1).is_none());
    assert!(try_rev_positions_in_pack_order(&git_rev[..git_rev.len() - 1], n).is_none());

    let mut bad_sig = git_rev.clone();
    bad_sig[0..4].copy_from_slice(&0u32.to_be_bytes());
    assert!(try_rev_positions_in_pack_order(&bad_sig, n).is_none());

    let mut bad_ver = git_rev.clone();
    bad_ver[4..8].copy_from_slice(&99u32.to_be_bytes());
    assert!(try_rev_positions_in_pack_order(&bad_ver, n).is_none());

    let mut bad_hash = git_rev.clone();
    bad_hash[8..12].copy_from_slice(&99u32.to_be_bytes());
    assert!(try_rev_positions_in_pack_order(&bad_hash, n).is_none());

    let mut bad_ck = git_rev.clone();
    let pos = bad_ck.len() - fx.algo.len();
    bad_ck[pos] ^= 0xAA;
    assert!(try_rev_positions_in_pack_order(&bad_ck, n).is_none());

    let mut wrong_slot = git_rev.clone();
    wrong_slot[12..16].copy_from_slice(&99u32.to_be_bytes());
    assert!(pack_rev_fsck_messages(&wrong_slot, &index, "wrong.slot")
        .iter()
        .any(|m| m.contains("invalid rev-index position")));

    let msgs = pack_rev_fsck_messages(&git_rev[..12 + 4], &index, "trunc.entries");
    assert!(
        msgs.iter().any(|m| {
            m.contains("corrupt")
                || m.contains("truncated")
                || m.contains("too small")
                || m.contains("invalid rev-index position")
        }),
        "unexpected fsck msgs: {msgs:?}"
    );
}

#[test]
fn try_rev_positions_rejects_non_permutation() {
    let fx = shared_pack(PackShape::GitWhole, HashAlgo::Sha1).expect("fixture");
    let (_, git_rev, _) = git_index_pack_rev(&fx.bytes, fx.algo);
    let scratch = tempfile::tempdir().expect("idx");
    let idx_path = scratch.path().join("f.idx");
    let (git_idx, _, _) = git_index_pack_rev(&fx.bytes, fx.algo);
    std::fs::write(&idx_path, &git_idx).expect("write idx");
    let index = read_pack_index(&idx_path).expect("read idx");
    let mut corrupt = git_rev.clone();
    if corrupt.len() >= 16 {
        corrupt[12..16].copy_from_slice(&0u32.to_be_bytes());
        corrupt[16..20].copy_from_slice(&0u32.to_be_bytes());
    }
    assert!(try_rev_positions_in_pack_order(&corrupt, index.len()).is_none());
}

#[test]
fn unpack_large_blob_matches_git_unpack_objects() {
    const SIZE: usize = 512 * 1024;
    let tmp = tempfile::tempdir().expect("repo");
    init_repo(tmp.path(), false);
    let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    std::fs::write(tmp.path().join("big.bin"), &payload).expect("write");
    git_run(tmp.path(), &["add", "big.bin"]);
    git_run(tmp.path(), &["commit", "-q", "-m", "large"]);
    git_run(tmp.path(), &["repack", "-adf", "-q"]);
    let pack = read_pack_from_repo(&tmp.path().join(".git"));

    let grit_dir = tempfile::tempdir().expect("grit odb");
    let grit_odb = Odb::new(grit_dir.path());
    let count = unpack_objects(&mut pack.as_slice(), &grit_odb, &UnpackOptions::default())
        .expect("grit unpack");
    assert!(count >= 3, "expect commit+tree+blob at minimum");

    let git_dir = tempfile::tempdir().expect("git odb");
    git_run(git_dir.path(), &["init", "-q", "-b", "main", "."]);
    let mut child = Command::new("git");
    child
        .current_dir(git_dir.path())
        .args(["unpack-objects"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in GIT_ENV {
        child.env(k, v);
    }
    let mut child = child.spawn().expect("spawn unpack-objects");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&pack)
        .expect("write pack");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git unpack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let grit_oids = loose_object_ids(grit_dir.path());
    let git_oids = loose_object_ids(&git_dir.path().join(".git/objects"));
    assert_eq!(grit_oids, git_oids, "loose object id set");
    for oid in &grit_oids {
        let grit_obj = grit_odb.read(oid).expect("grit read");
        let hex = oid.to_hex();
        let git_type = git(git_dir.path(), &["cat-file", "-t", &hex]);
        assert_eq!(git_type.trim(), grit_obj.kind.as_str());
        if grit_obj.kind == ObjectKind::Blob {
            let git_data = git_cat_file_blob(git_dir.path(), &hex);
            assert_eq!(
                git_data.as_slice(),
                grit_obj.data.as_slice(),
                "blob {hex} payload mismatch (git len {}, grit len {})",
                git_data.len(),
                grit_obj.data.len()
            );
        }
    }
}

fn git_cat_file_blob(repo: &Path, oid_hex: &str) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "blob", oid_hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git cat-file blob");
    assert!(
        out.status.success(),
        "git cat-file blob {oid_hex}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn loose_object_ids(objects_dir: &Path) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    for ent in std::fs::read_dir(objects_dir).expect("read objects") {
        let ent = ent.expect("dirent");
        let shard = ent.file_name().to_string_lossy().to_string();
        if shard.len() == 2 && shard.chars().all(|c| c.is_ascii_hexdigit()) {
            for obj in std::fs::read_dir(ent.path()).expect("read shard") {
                let obj = obj.expect("obj");
                let rest = obj.file_name().to_string_lossy().into_owned();
                let hex = format!("{shard}{rest}");
                out.insert(ObjectId::from_hex(&hex).expect("oid"));
            }
        }
    }
    out
}

fn make_single_tree_pack(tree_body: &[u8], algo: HashAlgo) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(tree_body).expect("zlib");
    let compressed = enc.finish().expect("finish");
    let type_code: u8 = 2;
    let mut header = Vec::new();
    let mut size = tree_body.len();
    let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    if size > 0 {
        header.push(first | 0x80);
        while size > 0 {
            let b = (size & 0x7f) as u8;
            size >>= 7;
            header.push(if size > 0 { b | 0x80 } else { b });
        }
    } else {
        header.push(first);
    }
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&1u32.to_be_bytes());
    pack.extend_from_slice(&header);
    pack.extend_from_slice(&compressed);
    let trailer = algo.digest(&pack);
    pack.extend_from_slice(trailer.as_bytes());
    pack
}

#[test]
fn unpack_strict_rejects_missing_tree_reference() {
    let missing = ObjectId::from_hex(&"ab".repeat(20)).expect("oid");
    let tree = serialize_tree(&[TreeEntry {
        mode: MODE_REGULAR,
        name: b"missing".to_vec(),
        oid: missing,
    }]);
    let pack = make_single_tree_pack(&tree, HashAlgo::Sha1);
    let tmp = tempfile::tempdir().expect("odb");
    let odb = Odb::new(tmp.path());
    let err = unpack_objects(
        &mut pack.as_slice(),
        &odb,
        &UnpackOptions {
            strict: true,
            ..Default::default()
        },
    )
    .expect_err("strict unpack must fail");
    assert!(
        matches!(err, grit_lib::error::Error::CorruptObject(_)),
        "expected corrupt object, got {err:?}"
    );

    let mut map = HashMap::new();
    let tree_oid = HashAlgo::Sha1.hash_object(ObjectKind::Tree, &tree);
    map.insert(tree_oid, (ObjectKind::Tree, tree));
    assert!(strict_verify_packed_references(None, &map).is_err());
}

#[test]
fn build_pack_rev_header_constants_match_git_ridx() {
    assert_eq!(RIDX_SIGNATURE, 0x5249_4458);
    assert_eq!(RIDX_VERSION, 1);
    assert_eq!(RIDX_HASH_ID_SHA1, 1);
    assert_eq!(RIDX_HASH_ID_SHA256, 2);
}

#[test]
fn pack_rev_fsck_messages_cover_header_and_body_errors() {
    let fx = shared_pack(PackShape::GitDelta, HashAlgo::Sha1).expect("fixture");
    let (git_idx, git_rev, _) = git_index_pack_rev(&fx.bytes, fx.algo);
    let scratch = tempfile::tempdir().expect("idx");
    let idx_path = scratch.path().join("f.idx");
    std::fs::write(&idx_path, &git_idx).expect("write idx");
    let index = read_pack_index(&idx_path).expect("read idx");

    assert!(pack_rev_fsck_messages(&[], &index, "empty.rev")
        .iter()
        .any(|m| m.contains("too small")));
    assert!(pack_rev_fsck_messages(b"short", &index, "short.rev")
        .iter()
        .any(|m| m.contains("too small")));
    assert!(
        pack_rev_fsck_messages(&git_rev[..git_rev.len() / 2], &index, "trunc.rev")
            .iter()
            .any(|m| m.contains("corrupt"))
    );

    let mut bad_sig = git_rev.clone();
    bad_sig[0..4].copy_from_slice(&0x1111_1111_u32.to_be_bytes());
    assert!(pack_rev_fsck_messages(&bad_sig, &index, "bad.sig")
        .iter()
        .any(|m| m.contains("unknown signature")));

    let mut bad_ver = git_rev.clone();
    bad_ver[4..8].copy_from_slice(&8u32.to_be_bytes());
    assert!(pack_rev_fsck_messages(&bad_ver, &index, "bad.ver")
        .iter()
        .any(|m| m.contains("unsupported version")));

    let mut bad_hash = git_rev.clone();
    bad_hash[8..12].copy_from_slice(&9u32.to_be_bytes());
    assert!(pack_rev_fsck_messages(&bad_hash, &index, "bad.hash")
        .iter()
        .any(|m| m.contains("unsupported hash id")));

    let mut bad_ck = git_rev.clone();
    let hb = fx.algo.len();
    let ck_pos = bad_ck.len() - hb;
    bad_ck[ck_pos] ^= 0x55;
    assert!(pack_rev_fsck_messages(&bad_ck, &index, "bad.ck")
        .iter()
        .any(|m| m.contains("invalid checksum")));

    let _ = build_pack_rev_bytes(&index);
    let _ = build_pack_rev_bytes_from_index_order_offsets(
        &index.iter().map(|e| e.offset()).collect::<Vec<_>>(),
    );
    assert!(hashfile_checksum_valid_sha1(&git_rev));
    std::fs::write(rev_path_for_index(&idx_path), &git_rev).expect("write rev");
    assert!(verify_pack_rev_file(&rev_path_for_index(&idx_path), &index).is_ok());
    assert!(verify_pack_rev_file(&scratch.path().join("missing.rev"), &index).is_ok());

    let mut bad_perm = git_rev.clone();
    if index.len() >= 2 {
        bad_perm[12..16].copy_from_slice(&1u32.to_be_bytes());
        bad_perm[16..20].copy_from_slice(&1u32.to_be_bytes());
        assert!(pack_rev_fsck_messages(&bad_perm, &index, "bad.perm")
            .iter()
            .any(|m| m.contains("invalid rev-index position")));
        std::fs::write(rev_path_for_index(&idx_path), &bad_perm).expect("write bad rev");
        assert!(verify_pack_rev_file(&rev_path_for_index(&idx_path), &index).is_err());
    }
}

#[test]
fn pack_index_build_errors_and_thin_pack_with_odb_base() {
    let fx = shared_pack(PackShape::GritWhole, HashAlgo::Sha1).expect("fixture");
    let tmp = tempfile::tempdir().expect("odb");
    let odb = Odb::new(tmp.path());
    let mut corrupt = fx.bytes.clone();
    corrupt.truncate(corrupt.len().saturating_sub(8));
    assert!(
        pack_index_records_with_threads(&corrupt, &odb, Parallelism::resolve(Some(1))).is_err()
    );
    assert!(
        pack_index_records_with_threads(&corrupt, &odb, Parallelism::resolve(Some(4))).is_err()
    );

    let repo = tempfile::tempdir().expect("repo");
    init_repo(repo.path(), false);
    let base = b"x".repeat(128 * 1024);
    std::fs::write(repo.path().join("big.txt"), &base).expect("write");
    git_run(repo.path(), &["add", "big.txt"]);
    git_run(repo.path(), &["commit", "-q", "-m", "base"]);
    let have = ObjectId::from_hex(git(repo.path(), &["rev-parse", "HEAD"]).trim()).unwrap();
    let mut tip_data = base.clone();
    tip_data.push(b'\n');
    std::fs::write(repo.path().join("big.txt"), &tip_data).expect("write");
    git_run(repo.path(), &["commit", "-am", "tip"]);
    let want = ObjectId::from_hex(git(repo.path(), &["rev-parse", "HEAD"]).trim()).unwrap();
    let odb = Odb::new(repo.path().join(".git/objects").as_path())
        .with_config_git_dir(repo.path().join(".git"));
    let thin = build_pack(
        &odb,
        &[want],
        &[have],
        &PackBuildOptions {
            delta: true,
            thin: true,
            ..Default::default()
        },
    )
    .expect("thin");
    assert!(pack_is_thin(&thin, HashAlgo::Sha1));
    let records = pack_index_records_with_threads(&thin, &odb, Parallelism::resolve(Some(4)))
        .expect("thin index");
    assert!(!records.is_empty());
    install_pack_bytes(
        thin,
        &odb,
        &IngestPackOptions {
            fix_thin: true,
            threads: Some(4),
        },
    )
    .expect("fix thin install");
}

#[test]
fn pack_index_build_sha256_delta_pack_threads() {
    let Some(fx) = shared_pack(PackShape::GitDelta, HashAlgo::Sha256) else {
        eprintln!("skip: sha256 unavailable");
        return;
    };
    let tmp = tempfile::tempdir().expect("odb");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
    std::fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectFormat = sha256\n",
    )
    .expect("config");
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir);
    let one = pack_index_records_with_threads(&fx.bytes, &odb, Parallelism::resolve(Some(1)))
        .expect("sha256 index t=1");
    let eight = pack_index_records_with_threads(&fx.bytes, &odb, Parallelism::resolve(Some(8)))
        .expect("sha256 index t=8");
    assert_eq!(one.len(), eight.len());
    for (a, b) in one.iter().zip(&eight) {
        assert_eq!(a.oid, b.oid);
        assert_eq!(a.offset, b.offset);
        assert_eq!(a.crc32, b.crc32);
    }
    let (git_idx, git_rev, pack_on_disk) = git_index_pack_rev(&fx.bytes, fx.algo);
    let (grit_idx, grit_rev) = grit_idx_rev_bytes(&pack_on_disk, &odb, 4, false);
    assert_eq!(grit_idx, git_idx);
    assert_eq!(grit_rev, git_rev);
}
