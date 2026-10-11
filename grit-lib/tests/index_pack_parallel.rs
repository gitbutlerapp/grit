//! Parallel index-pack hashing: byte-identical `.idx` vs system `git index-pack`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use grit_lib::hash::Parallelism;
use grit_lib::index_pack::{install_pack_bytes, IngestPackOptions};
use grit_lib::objects::HashAlgo;
use grit_lib::odb::Odb;
use grit_lib::pack::write_v2_pack_index_with_trailer;
use grit_lib::unpack_objects::pack_index_records_with_threads;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
];

const DEFAULT_MIN_PACK_OBJECTS: u32 = 50_000;
const FILES_PER_COMMIT: usize = 8;

struct DeltaPackFixture {
    pack: Vec<u8>,
    hash_algo: HashAlgo,
    _keep: tempfile::TempDir,
}

fn target_min_pack_objects() -> u32 {
    std::env::var("GRIT_INDEX_PACK_PARALLEL_OBJECTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 100)
        .unwrap_or(DEFAULT_MIN_PACK_OBJECTS)
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

fn git_run(dir: &Path, args: &[&str]) {
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

fn write_fast_import_script(out: &mut Vec<u8>, min_objects: u32) -> std::io::Result<()> {
    let min_commits = (min_objects as usize).div_ceil(FILES_PER_COMMIT) + 2;
    out.extend_from_slice(b"feature done\n");
    out.extend_from_slice(b"commit refs/heads/main\nmark :1\n");
    out.extend_from_slice(b"committer T <t@example.com> 1000000000 +0000\n");
    out.extend_from_slice(b"data 4\ninit\n");
    out.extend_from_slice(b"M 100644 inline seed.txt\n");
    out.extend_from_slice(b"data 5\nseed\n\n");

    for i in 1..min_commits {
        let msg = format!("c{i}");
        writeln!(out, "commit refs/heads/main")?;
        writeln!(out, "mark :{}", i + 1)?;
        writeln!(out, "committer T <t@example.com> 1000000000 +0000")?;
        writeln!(out, "data {}", msg.len())?;
        writeln!(out, "{msg}")?;
        writeln!(out, "from :{i}")?;
        for f in 0..FILES_PER_COMMIT {
            let path = format!("d/{i:05}/{f:02}.txt");
            let payload = format!("payload commit={i} file={f}\n");
            writeln!(out, "M 100644 inline {path}")?;
            writeln!(out, "data {}", payload.len())?;
            write!(out, "{payload}")?;
            out.push(b'\n');
        }
    }
    out.extend_from_slice(b"done\n");
    Ok(())
}

fn build_delta_pack_fixture(sha256: bool) -> DeltaPackFixture {
    let min_objects = target_min_pack_objects();
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if sha256 {
        if !git_ok(&dir, &["init", "--object-format=sha256", "-b", "main"]) {
            panic!("system git lacks sha256 object format");
        }
    } else {
        git_run(&dir, &["init", "-b", "main"]);
    }

    let mut script = Vec::new();
    write_fast_import_script(&mut script, min_objects).expect("fast-import script");
    let mut child = Command::new("git");
    child
        .current_dir(&dir)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in GIT_ENV {
        child.env(k, v);
    }
    let mut child = child.spawn().expect("spawn fast-import");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(&script).expect("write fast-import");
    }
    let out = child.wait_with_output().expect("wait fast-import");
    assert!(
        out.status.success(),
        "git fast-import (script {} bytes): {}",
        script.len(),
        String::from_utf8_lossy(&out.stderr)
    );

    git_run(&dir, &["repack", "-adf", "--depth=50", "-q"]);
    let pack_dir = dir.join(".git/objects/pack");
    let pack_path = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("repack must produce a .pack");
    let pack = std::fs::read(&pack_path).expect("read pack");
    let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]);
    assert!(
        count >= min_objects,
        "pack should contain at least {min_objects} objects, header count {count}"
    );

    let git_dir = dir.join(".git");
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir.clone());
    let hash_algo = if sha256 {
        HashAlgo::Sha256
    } else {
        HashAlgo::Sha1
    };
    assert_eq!(odb.hash_algo(), hash_algo);
    let hb = hash_algo.len();
    assert!(
        pack.len() > 12 + hb,
        "pack must include a {hb}-byte trailer"
    );

    DeltaPackFixture {
        pack,
        hash_algo,
        _keep: tmp,
    }
}

static FIXTURE_SHA1: OnceLock<DeltaPackFixture> = OnceLock::new();
static FIXTURE_SHA256: OnceLock<Option<DeltaPackFixture>> = OnceLock::new();

fn shared_fixture_sha1() -> &'static DeltaPackFixture {
    FIXTURE_SHA1.get_or_init(|| build_delta_pack_fixture(false))
}

fn shared_fixture_sha256() -> Option<&'static DeltaPackFixture> {
    FIXTURE_SHA256
        .get_or_init(|| {
            let tmp = tempfile::tempdir().ok()?;
            if !git_ok(tmp.path(), &["init", "--object-format=sha256"]) {
                return None;
            }
            drop(tmp);
            Some(build_delta_pack_fixture(true))
        })
        .as_ref()
}

fn git_index_pack_idx(pack: &[u8], hash_algo: HashAlgo, threads: usize) -> (Vec<u8>, Vec<u8>) {
    let dir = tempfile::tempdir().expect("scratch");
    if hash_algo == HashAlgo::Sha256 {
        git_run(
            dir.path(),
            &["init", "--object-format=sha256", "-b", "main"],
        );
    } else {
        git_run(dir.path(), &["init", "-b", "main"]);
    }
    let pack_path = dir.path().join("fixture.pack");
    std::fs::write(&pack_path, pack).expect("write pack");
    let mut args = vec!["index-pack"];
    let threads_arg = format!("--threads={threads}");
    args.push(&threads_arg);
    args.push(
        pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("pack name"),
    );
    let mut cmd = Command::new("git");
    cmd.current_dir(dir.path()).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    cmd.env("GIT_TEST_NO_WRITE_REV_INDEX", "1");
    let out = cmd.output().expect("git index-pack");
    assert!(
        out.status.success(),
        "git index-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let idx = std::fs::read(dir.path().join("fixture.idx")).expect("read git idx");
    let pack_on_disk = std::fs::read(&pack_path).expect("read indexed pack");
    verify_pack_with_git(&pack_on_disk, &idx, hash_algo);
    (idx, pack_on_disk)
}

fn grit_index_bytes(pack: &[u8], odb: &Odb, threads: usize) -> Vec<u8> {
    let parallelism = Parallelism::resolve(Some(threads));
    let records = pack_index_records_with_threads(pack, odb, parallelism).expect("grit index");
    let hb = odb.hash_algo().len();
    let trailer = &pack[pack.len() - hb..];
    let entries: Vec<_> = records
        .into_iter()
        .map(|r| (r.oid, r.offset, r.crc32))
        .collect();
    let dir = tempfile::tempdir().expect("idx scratch");
    let idx_path = dir.path().join("grit.idx");
    write_v2_pack_index_with_trailer(&idx_path, &entries, trailer, hb).expect("write idx");
    std::fs::read(&idx_path).expect("read grit idx")
}

fn verify_pack_with_git(pack: &[u8], idx: &[u8], hash_algo: HashAlgo) {
    let dir = tempfile::tempdir().expect("verify scratch");
    std::fs::write(dir.path().join("f.pack"), pack).expect("write pack");
    std::fs::write(dir.path().join("f.idx"), idx).expect("write idx");
    let mut args = vec!["verify-pack", "-v"];
    let sha256_flag = "sha256";
    if hash_algo == HashAlgo::Sha256 {
        args.push("--object-format");
        args.push(sha256_flag);
    }
    args.push("f.idx");
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(&args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "git verify-pack -v: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_repo_format(git_dir: &Path, sha256: bool) {
    std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
    if sha256 {
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectFormat = sha256\n",
        )
        .expect("write sha256 config");
    } else {
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("write config");
    }
}

fn assert_idx_matches_git(pack: &[u8], hash_algo: HashAlgo, threads: usize) {
    let tmp = tempfile::tempdir().expect("odb");
    let git_dir = tmp.path().join(".git");
    write_repo_format(&git_dir, hash_algo == HashAlgo::Sha256);
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir);
    assert_eq!(odb.hash_algo(), hash_algo);
    let (git_idx, pack_on_disk) = git_index_pack_idx(pack, hash_algo, threads);
    let grit_idx = grit_index_bytes(&pack_on_disk, &odb, threads);
    assert_eq!(
        grit_idx, git_idx,
        "grit .idx must match git index-pack (--threads={threads})"
    );
    verify_pack_with_git(&pack_on_disk, &grit_idx, hash_algo);
}

#[test]
fn parallel_index_pack_matches_git_sha1_threads_1_and_8() {
    let fx = shared_fixture_sha1();
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 1);
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 8);
}

#[test]
fn parallel_index_pack_matches_git_sha256_threads_1_and_8() {
    let Some(fx) = shared_fixture_sha256() else {
        eprintln!("skip: sha256 object format unavailable");
        return;
    };
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 1);
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 8);
}

#[test]
fn install_pack_path_matches_git_index() {
    let fx = shared_fixture_sha1();
    let pack = fx.pack.clone();
    let hash_algo = fx.hash_algo;
    let tmp = tempfile::tempdir().expect("install");
    let odb = Odb::new(tmp.path().join("objects").as_path());
    install_pack_bytes(
        pack.clone(),
        &odb,
        &IngestPackOptions {
            fix_thin: false,
            threads: Some(8),
            skip_post_index_verify: false,
        },
    )
    .expect("install");
    let pack_dir = tmp.path().join("objects").join("pack");
    let idx_path = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("installed idx");
    let grit_idx = std::fs::read(&idx_path).expect("read installed idx");
    let (git_idx, pack_on_disk) = git_index_pack_idx(&pack, hash_algo, 8);
    let _ = pack_on_disk;
    assert_eq!(grit_idx, git_idx);
}
