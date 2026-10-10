//! Git compatibility for [`grit_lib::pack_objects`] packs: `index-pack --strict`, `fsck`,
//! thin-pack fix, size parity vs `git pack-objects --revs`, and SHA-256 repos.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::pack_objects::{PackBuildOptions, PackObjects, PackObjectsOptions};
use grit_lib::transfer::build_pack;
use grit_lib::unpack_objects::pack_bytes_to_object_map;

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

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir)
}

fn grit_delta_pack(odb: &Odb, wants: &[ObjectId], haves: &[ObjectId]) -> Vec<u8> {
    build_pack(
        odb,
        wants,
        haves,
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack")
}

fn git_pack_objects(repo: &Path, tips: &[ObjectId], haves: &[ObjectId]) -> Vec<u8> {
    let mut child = Command::new("git");
    child
        .current_dir(repo)
        .args(["pack-objects", "--stdout", "--revs", "--delta-base-offset"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in GIT_ENV {
        child.env(k, v);
    }
    let mut child = child.spawn().expect("spawn");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for t in tips {
            writeln!(stdin, "{}", t.to_hex()).unwrap();
        }
        for h in haves {
            writeln!(stdin, "^ {}", h.to_hex()).unwrap();
        }
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git pack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_index_pack_stdin_fix_thin(pack: &[u8]) {
    let mut child = Command::new("git")
        .args(["index-pack", "--fix-thin", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("spawn");
    child.stdin.take().unwrap().write_all(pack).unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "index-pack --fix-thin: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_index_pack_strict(_repo: &Path, pack: &[u8]) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let init = Command::new("git")
        .current_dir(tmp.path())
        .args(["init", "-q", "--bare", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("init");
    assert!(init.status.success());
    let pack_path = tmp.path().join("in.pack");
    std::fs::write(&pack_path, pack).unwrap();
    let idx = Command::new("git")
        .current_dir(tmp.path())
        .args(["index-pack", &pack_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        idx.status.success(),
        "index-pack: {}",
        String::from_utf8_lossy(&idx.stderr)
    );
    let fsck = Command::new("git")
        .current_dir(tmp.path())
        .args(["fsck", "--strict", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

fn build_deep_history_repo(commits: usize) -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..commits {
        std::fs::write(dir.join(format!("f{i:04}.txt")), format!("body {i}\n")).unwrap();
        std::fs::write(dir.join("shared.txt"), format!("shared {i}\n")).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(dir, &["repack", "-adf", "--depth=50", "-q"]);
    let tip = rev_parse(dir, "HEAD");
    let have = rev_parse(dir, &format!("HEAD~{}", commits.saturating_sub(2).max(1)));
    (tmp, tip, have)
}

#[test]
fn pack_objects_full_and_incremental_pass_git_index_pack_and_fsck() {
    let (dir, tip, have) = build_deep_history_repo(80);
    let odb = open_odb(dir.path());
    let full = grit_delta_pack(&odb, &[tip], &[]);
    git_index_pack_strict(dir.path(), &full);
    let inc = grit_delta_pack(&odb, &[tip], &[have]);
    git_index_pack_strict(dir.path(), &inc);
    let git_full = git_pack_objects(dir.path(), &[tip], &[]);
    assert!(
        full.len() <= git_full.len() * 11 / 10,
        "full pack size {} vs git {} (ratio {:.2})",
        full.len(),
        git_full.len(),
        full.len() as f64 / git_full.len() as f64
    );
}

#[test]
fn pack_objects_thin_pack_fixes_with_git_index_pack() {
    let (dir, tip, have) = build_deep_history_repo(40);
    let odb = open_odb(dir.path());
    let thin = {
        let mut buf = Vec::new();
        PackObjects::new(
            &odb,
            PackObjectsOptions::from(PackBuildOptions {
                delta: true,
                thin: true,
                ..PackBuildOptions::default()
            }),
        )
        .wants(&[tip])
        .haves(&[have])
        .write_to(&mut buf)
        .expect("thin");
        buf
    };
    git_index_pack_stdin_fix_thin(&thin);
    let map = pack_bytes_to_object_map(&thin, &odb).expect("resolve");
    assert!(map.contains_key(&tip));
}

#[test]
fn pack_objects_write_to_matches_build_pack_bytes() {
    let (dir, tip, have) = build_deep_history_repo(12);
    let odb = open_odb(dir.path());
    let opts = PackBuildOptions {
        delta: true,
        ..PackBuildOptions::default()
    };
    let via_build = build_pack(&odb, &[tip], &[have], &opts).expect("build_pack");
    let mut via_api = Vec::new();
    PackObjects::new(&odb, PackObjectsOptions::from(opts))
        .wants(&[tip])
        .haves(&[have])
        .write_to(&mut via_api)
        .expect("write_to");
    assert_eq!(
        via_build, via_api,
        "PackObjects must match build_pack output"
    );
}

// SHA-256 pack indexing is covered by `matrix_packs::sha256_repo_packs_index_and_fsck_clean`
// (same `build_pack` / `PackObjects` serializer and trailer width).
