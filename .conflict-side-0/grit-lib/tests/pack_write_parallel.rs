//! Byte-identical delta packs across pack-write worker counts (1/2/4/8 threads).

use std::path::Path;
use std::process::Command;

use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack_objects::{build_pack_with_write_threads, PackBuildOptions};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::remote::transfer::build_pack;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
];

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8(out.stdout).expect("utf8")
}

fn git_try(dir: &Path, args: &[&str]) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir)
}

fn git_validate_pack(pack: &[u8], algo: HashAlgo, fix_thin: bool) {
    if algo == HashAlgo::Sha256 {
        assert!(
            git_index_and_fsck_ok(pack, algo),
            "sha256 pack must index-pack and fsck clean"
        );
        return;
    }
    let mut args = vec!["index-pack", "--stdin"];
    if fix_thin {
        args.push("--fix-thin");
    } else {
        args.push("--strict");
    }
    let mut child = Command::new("git")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("spawn index-pack");
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(pack)
            .expect("write pack");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git index-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_index_and_fsck_ok(pack: &[u8], algo: HashAlgo) -> bool {
    let scratch = tempfile::tempdir().expect("scratch");
    let mut init_args = vec!["init", "-q", "--bare"];
    let fmt = format!("--object-format={}", algo.name());
    if algo == HashAlgo::Sha256 {
        init_args.push(&fmt);
    }
    init_args.push(".");
    if !git_try(scratch.path(), &init_args) {
        return false;
    }
    let pack_path = scratch.path().join("objects/pack/in.pack");
    std::fs::write(&pack_path, pack).unwrap();
    let idx = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", &pack_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    if !idx.status.success() {
        return false;
    }
    Command::new("git")
        .current_dir(scratch.path())
        .args(["fsck", "--strict", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn assert_byte_identical_threads(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
    label: &str,
    fix_thin: bool,
) {
    let baseline = build_pack_with_write_threads(odb, wants, haves, opts, Some(1))
        .unwrap_or_else(|e| panic!("{label} baseline threads=1: {e}"));
    git_validate_pack(&baseline, odb.hash_algo(), fix_thin);
    for threads in [2_usize, 4, 8] {
        let pack = build_pack_with_write_threads(odb, wants, haves, opts, Some(threads))
            .unwrap_or_else(|e| panic!("{label} threads={threads}: {e}"));
        assert_eq!(
            pack, baseline,
            "{label}: pack bytes differ at threads={threads}"
        );
    }
}

struct DeltaFixture {
    dir: tempfile::TempDir,
    tip: ObjectId,
    prev: ObjectId,
}

fn build_delta_fixture() -> DeltaFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!("line {i:05} lorem ipsum dolor sit amet\n"));
    }
    for rev in 0..6 {
        body.push_str(&format!("--- edit {rev} ---\n"));
        std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
        git(dir, &["add", "big.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
    }
    let tip = rev_parse(dir, "HEAD");
    let prev = rev_parse(dir, "HEAD~1");
    DeltaFixture {
        dir: tmp,
        tip,
        prev,
    }
}

#[test]
fn pack_write_threads_byte_identical_delta_fixture() {
    let fx = build_delta_fixture();
    let odb = open_odb(fx.dir.path());
    let opts = PackBuildOptions {
        delta: true,
        ..PackBuildOptions::default()
    };
    assert_byte_identical_threads(&odb, &[fx.tip], &[], &opts, "delta fixture", false);
}

#[test]
fn pack_write_threads_byte_identical_reuse_deltas() {
    let fx = build_delta_fixture();
    let dir = fx.dir.path();
    git(
        dir,
        &[
            "repack",
            "-q",
            "-a",
            "-d",
            "-f",
            "--window=10",
            "--depth=50",
        ],
    );
    let odb = open_odb(dir);
    let opts = PackBuildOptions {
        delta: true,
        reuse_deltas: true,
        ..PackBuildOptions::default()
    };
    assert_byte_identical_threads(&odb, &[fx.tip], &[], &opts, "reuse deltas", false);
}

#[test]
fn pack_write_threads_byte_identical_thin_pack() {
    let fx = build_delta_fixture();
    let odb = open_odb(fx.dir.path());
    let opts = PackBuildOptions {
        delta: true,
        thin: true,
        ..PackBuildOptions::default()
    };
    assert_byte_identical_threads(&odb, &[fx.tip], &[fx.prev], &opts, "thin pack", true);
}

#[test]
fn pack_write_threads_byte_identical_delta_islands() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "a"]);
    let mut prefix = String::new();
    for i in 0..3000 {
        prefix.push_str(&format!("shared {i:05}\n"));
    }
    let mut a_body = prefix.clone();
    a_body.push_str("branch-a-only tail\n");
    std::fs::write(dir.join("data.txt"), a_body.as_bytes()).unwrap();
    git(dir, &["add", "data.txt"]);
    git(dir, &["commit", "-q", "-m", "a"]);
    let tip_a = rev_parse(dir, "HEAD");

    git(dir, &["checkout", "-q", "--orphan", "b"]);
    git(dir, &["rm", "-q", "-f", "--cached", "data.txt"]);
    std::fs::write(dir.join("data.txt"), prefix.as_bytes()).unwrap();
    git(dir, &["add", "data.txt"]);
    git(dir, &["commit", "-q", "-m", "b"]);
    let tip_b = rev_parse(dir, "HEAD");

    git(dir, &["config", "pack.island", "refs/heads/(.*)"]);
    let odb = open_odb(dir);
    let opts = PackBuildOptions {
        delta: true,
        respect_islands: true,
        ..PackBuildOptions::default()
    };
    assert_byte_identical_threads(&odb, &[tip_a, tip_b], &[], &opts, "delta islands", false);
}

fn build_sha256_delta_fixture() -> Option<(tempfile::TempDir, ObjectId)> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let fmt = format!("--object-format={}", HashAlgo::Sha256.name());
    if !git_try(dir, &["init", "-q", "-b", "main", fmt.as_str(), "."]) {
        return None;
    }
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!("line {i:05} lorem ipsum dolor sit amet\n"));
    }
    for rev in 0..6 {
        body.push_str(&format!("--- edit {rev} ---\n"));
        std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
        git(dir, &["add", "big.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
    }
    let tip = rev_parse(dir, "HEAD");
    Some((tmp, tip))
}

#[test]
fn pack_write_threads_byte_identical_sha256() {
    let Some((dir, tip)) = build_sha256_delta_fixture() else {
        eprintln!("SKIP: system git lacks sha256 object format");
        return;
    };
    let odb = open_odb(dir.path());
    assert_eq!(odb.hash_algo(), HashAlgo::Sha256);
    let opts = PackBuildOptions {
        delta: true,
        ..PackBuildOptions::default()
    };
    assert_byte_identical_threads(&odb, &[tip], &[], &opts, "sha256 repo", false);
}

#[test]
fn pack_write_config_threads_matches_explicit_one() {
    let fx = build_delta_fixture();
    let dir = fx.dir.path();
    git(dir, &["config", "pack.threads", "4"]);
    let odb = open_odb(dir);
    let opts = PackBuildOptions {
        delta: true,
        ..PackBuildOptions::default()
    };
    let explicit_one =
        build_pack_with_write_threads(&odb, &[fx.tip], &[], &opts, Some(1)).expect("explicit 1");
    let from_config = build_pack(&odb, &[fx.tip], &[], &opts).expect("config auto");
    assert_eq!(
        explicit_one, from_config,
        "pack.threads=4 must not change bytes vs single-threaded reference"
    );
}
