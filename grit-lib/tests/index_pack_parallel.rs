//! Parallel index-pack hashing: byte-identical `.idx` vs system `git index-pack`.

use std::path::Path;
use std::process::Command;

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

fn target_object_count() -> usize {
    std::env::var("GRIT_INDEX_PACK_PARALLEL_OBJECTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 100)
        .unwrap_or(50_000)
}

struct DeltaPackFixture {
    pack: Vec<u8>,
    hash_algo: HashAlgo,
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

fn build_delta_pack_fixture(sha256: bool) -> DeltaPackFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if sha256 {
        if !git_ok(dir, &["init", "--object-format=sha256", "-b", "main"]) {
            panic!("system git lacks sha256 object format");
        }
    } else {
        git_run(dir, &["init", "-b", "main"]);
    }
    let n = target_object_count();
    std::fs::write(dir.join("seed.txt"), b"seed\n").expect("seed");
    git_run(dir, &["add", "seed.txt"]);
    git_run(dir, &["commit", "-m", "seed"]);
    const FILES_PER_COMMIT: usize = 8;
    for i in 1..n {
        for f in 0..FILES_PER_COMMIT {
            let rel = format!("d/{i:05}/{f:02}.txt");
            std::fs::create_dir_all(dir.join("d").join(format!("{i:05}"))).expect("dir");
            std::fs::write(dir.join(&rel), format!("payload commit={i} file={f}\n"))
                .expect("write");
            git_run(dir, &["add", &rel]);
        }
        git_run(dir, &["commit", "-m", &format!("c{i}")]);
        if i % 5000 == 0 {
            eprintln!("index_pack_parallel fixture: {i}/{n} commits");
        }
    }
    git_run(dir, &["repack", "-adf", "--depth=50", "-q"]);
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
        count as usize >= n,
        "pack should contain at least {n} objects, header count {count}"
    );
    let git_dir = dir.join(".git");
    let odb = Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir.clone());
    let hash_algo = if sha256 {
        HashAlgo::Sha256
    } else {
        HashAlgo::Sha1
    };
    assert_eq!(
        odb.hash_algo(),
        hash_algo,
        "repository object format must match init"
    );
    let hb = hash_algo.len();
    assert!(
        pack.len() > 12 + hb,
        "pack must include a {hb}-byte trailer"
    );
    // Keep tempdir alive by leaking path into pack bytes consumer only — fixture built fresh per process.
    std::mem::forget(tmp);
    DeltaPackFixture { pack, hash_algo }
}

fn fixture_sha1() -> DeltaPackFixture {
    build_delta_pack_fixture(false)
}

fn fixture_sha256() -> Option<DeltaPackFixture> {
    let tmp = tempfile::tempdir().ok()?;
    if !git_ok(tmp.path(), &["init", "--object-format=sha256"]) {
        return None;
    }
    drop(tmp);
    Some(build_delta_pack_fixture(true))
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
    let fx = fixture_sha1();
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 1);
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 8);
    drop(fx);
}

#[test]
fn parallel_index_pack_matches_git_sha256_threads_1_and_8() {
    let Some(fx) = fixture_sha256() else {
        eprintln!("skip: sha256 object format unavailable");
        return;
    };
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 1);
    assert_idx_matches_git(&fx.pack, fx.hash_algo, 8);
    drop(fx);
}

#[test]
fn install_pack_path_matches_git_index() {
    let fx = fixture_sha1();
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
