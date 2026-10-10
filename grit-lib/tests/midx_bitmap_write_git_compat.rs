//! MIDX reachability bitmap writer compatibility with system Git.

mod midx_support;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::midx::{
    midx_checksum_hex, write_multi_pack_index_with_options, MidxBitmapWriteOutcome,
    WriteMultiPackIndexOptions,
};
use grit_lib::objects::ObjectId;
use grit_lib::pack_bitmap::BitmapIndex;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_test_support::{HashAlgo, RepoFixture};

use midx_support::{assert_git_midx_verify, git_available, pack_objects_layer};

fn git_ok(repo: &RepoFixture, args: &[&str]) {
    let out = repo.git(args);
    assert!(out.ok, "git {:?} failed: {}", args, out.stderr);
}

fn git_stdout(repo: &RepoFixture, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

fn open_repo(path: &Path) -> Repository {
    Repository::discover(Some(path)).expect("open repo")
}

fn pack_objects(repo: &RepoFixture, object_list: &str) -> String {
    let objects = repo.objects_dir();
    let prefix = objects.join("pack/pack");
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["pack-objects", "--delta-base-offset"])
        .arg(&prefix)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("pack-objects");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(object_list.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "pack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

fn build_three_pack_duplicate_fixture(repo: &RepoFixture) -> String {
    git_ok(repo, &["checkout", "-b", "main"]);
    std::fs::write(repo.path().join("base.txt"), b"base").unwrap();
    git_ok(repo, &["add", "base.txt"]);
    git_ok(repo, &["commit", "-m", "base"]);
    let base_tip = git_stdout(repo, &["rev-parse", "HEAD"]);

    std::fs::write(repo.path().join("other.txt"), b"other").unwrap();
    git_ok(repo, &["add", "other.txt"]);
    git_ok(repo, &["commit", "-m", "other"]);

    let p1_list = git_stdout(
        repo,
        &["rev-list", "--objects", "--no-object-names", &base_tip],
    );
    let p2_list = git_stdout(
        repo,
        &["rev-list", "--objects", "--no-object-names", "HEAD"],
    );
    let p1_hash = pack_objects(repo, &format!("{p1_list}\n"));
    let p2_hash = pack_objects(repo, &format!("{p2_list}\n"));
    assert_ne!(p1_hash, p2_hash);

    std::fs::write(repo.path().join("third.txt"), b"third").unwrap();
    git_ok(repo, &["add", "third.txt"]);
    git_ok(repo, &["commit", "-m", "third"]);
    git_ok(
        repo,
        &[
            "pack-objects",
            "--all",
            "--unpacked",
            &repo.objects_dir().join("pack/extra").to_string_lossy(),
        ],
    );

    let pack_dir = repo.objects_dir().join("pack");
    let idx_count = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "idx"))
        .count();
    assert!(idx_count >= 3, "expected >=3 packs, got {idx_count}");
    format!("pack-{p1_hash}.pack")
}

fn grit_write_midx_bitmap(pack_dir: &Path, preferred_pack: Option<&str>) {
    let result = write_multi_pack_index_with_options(
        pack_dir,
        &WriteMultiPackIndexOptions {
            write_bitmap: true,
            preferred_pack_name: preferred_pack.map(str::to_string),
            version: Some(1),
            ..WriteMultiPackIndexOptions::default()
        },
    )
    .expect("grit MIDX write");
    assert_eq!(result.bitmap, MidxBitmapWriteOutcome::Written);
    midx_support::assert_no_zero_byte_midx_bitmaps(pack_dir);
}

fn ref_commit_tips(repo: &RepoFixture) -> Vec<ObjectId> {
    git_stdout(repo, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .filter_map(|refname| {
            let hex = git_stdout(
                repo,
                &[
                    "rev-parse",
                    "-q",
                    "--verify",
                    &format!("{refname}^{{commit}}"),
                ],
            );
            ObjectId::from_hex(hex.trim()).ok()
        })
        .collect()
}

fn reachability_positions(repo: &Repository, index: &BitmapIndex, tip: &ObjectId) -> HashSet<u32> {
    let hex = tip.to_hex();
    let opts = RevListOptions {
        objects: true,
        no_object_names: true,
        quiet: true,
        ..Default::default()
    };
    let result = rev_list(repo, &[hex], &[], &opts).expect("rev-list");
    let mut out = HashSet::new();
    for oid in result
        .commits
        .iter()
        .chain(result.objects.iter().map(|(o, _)| o))
    {
        if let Some(pos) = index.position_of(oid) {
            out.insert(pos);
        }
    }
    out
}

fn verify_all_commit_bitmaps(repo: &Repository, index: &BitmapIndex) {
    let out = Command::new("git")
        .current_dir(repo.work_tree.as_ref().unwrap_or(&repo.git_dir))
        .args(["rev-list", "--all"])
        .output()
        .expect("rev-list");
    assert!(out.status.success());
    for line in out.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let hex = std::str::from_utf8(line).expect("utf8").trim();
        let Ok(commit) = ObjectId::from_hex(hex) else {
            continue;
        };
        let Some(bm) = index.commit_bitmap(&commit) else {
            continue;
        };
        let expected = reachability_positions(repo, index, &commit);
        let got: HashSet<u32> = bm.positions().collect();
        assert_eq!(got, expected, "bitmap mismatch for {commit}");
    }
}

fn assert_bitmap_checksum_matches_midx(pack_dir: &Path, objects_dir: &Path) {
    let midx_hex = midx_checksum_hex(objects_dir).expect("midx checksum");
    let bitmap_path = pack_dir.join(format!("multi-pack-index-{midx_hex}.bitmap"));
    let data = std::fs::read(&bitmap_path).expect("read bitmap");
    assert!(data.len() > 64, "bitmap too small");
    let hash_len = midx_hex.len() / 2;
    let midx_hash = hex::decode(midx_hex).expect("hex");
    let off = 4 + 2 + 2 + 4;
    assert_eq!(&data[off..off + hash_len], midx_hash.as_slice());
}

#[test]
fn grit_midx_bitmap_passes_git_test_bitmap() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    let preferred = build_three_pack_duplicate_fixture(&fixture);
    let pack_dir = fixture.objects_dir().join("pack");
    grit_write_midx_bitmap(&pack_dir, Some(&preferred));
    assert_git_midx_verify(&fixture);
    assert_bitmap_checksum_matches_midx(&pack_dir, &fixture.objects_dir());

    for tip in ref_commit_tips(&fixture) {
        let out = Command::new("git")
            .current_dir(fixture.path())
            .args(["rev-list", "--test-bitmap", &tip.to_hex()])
            .output()
            .expect("test-bitmap");
        assert!(
            out.status.success(),
            "git --test-bitmap tip {tip}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn git_uses_grit_midx_bitmap_for_counts() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    let preferred = build_three_pack_duplicate_fixture(&fixture);
    let pack_dir = fixture.objects_dir().join("pack");
    grit_write_midx_bitmap(&pack_dir, Some(&preferred));
    let plain = git_stdout(&fixture, &["rev-list", "--count", "--objects", "--all"]);
    let bitmap = git_stdout(
        &fixture,
        &[
            "rev-list",
            "--count",
            "--objects",
            "--all",
            "--use-bitmap-index",
        ],
    );
    assert_eq!(plain, bitmap);
}

#[test]
fn grit_reads_git_midx_bitmap_after_grit_write_and_vice_versa() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    let preferred = build_three_pack_duplicate_fixture(&fixture);
    let pack_dir = fixture.objects_dir().join("pack");
    grit_write_midx_bitmap(&pack_dir, Some(&preferred));
    let repo = open_repo(fixture.path());
    let grit_index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    verify_all_commit_bitmaps(&repo, &grit_index);

    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    git_ok(
        &fixture,
        &[
            "multi-pack-index",
            "write",
            "--bitmap",
            &format!("--preferred-pack={preferred}"),
        ],
    );
    let git_index = BitmapIndex::open(&repo)
        .expect("open git bitmap")
        .expect("bitmap");
    assert!(git_index.selected_commit_count() > 0);
    verify_all_commit_bitmaps(&repo, &git_index);
}

#[test]
fn incremental_layer_writes_no_placeholder() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_three_pack_duplicate_fixture(&fixture);
    let pack_dir = fixture.objects_dir().join("pack");
    grit_write_midx_bitmap(&pack_dir, None);
    std::fs::write(fixture.path().join("layer.txt"), b"layer\n").unwrap();
    git_ok(&fixture, &["add", "layer.txt"]);
    git_ok(&fixture, &["commit", "-q", "-m", "layer"]);
    assert!(pack_objects_layer(fixture.path(), 200));
    let result = write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            write_bitmap: true,
            version: Some(1),
            ..Default::default()
        },
    )
    .expect("incremental write");
    assert_eq!(
        result.bitmap,
        MidxBitmapWriteOutcome::SkippedIncrementalLayer
    );
    midx_support::assert_no_zero_byte_midx_bitmaps(&pack_dir);
}

#[test]
fn sha256_midx_bitmap() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let fixture = RepoFixture::init(HashAlgo::Sha256).expect("init sha256");
    git_ok(&fixture, &["checkout", "-b", "main"]);
    std::fs::write(fixture.path().join("sha256.txt"), b"sha256").unwrap();
    git_ok(&fixture, &["add", "sha256.txt"]);
    git_ok(&fixture, &["commit", "-m", "sha256"]);
    git_ok(&fixture, &["repack", "-adf"]);
    let pack_dir = fixture.objects_dir().join("pack");
    grit_write_midx_bitmap(&pack_dir, None);
    assert_git_midx_verify(&fixture);
    assert_bitmap_checksum_matches_midx(&pack_dir, &fixture.objects_dir());
    let out = Command::new("git")
        .current_dir(fixture.path())
        .args(["rev-list", "--test-bitmap", "HEAD"])
        .output()
        .expect("test-bitmap");
    assert!(
        out.status.success(),
        "sha256 midx bitmap: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
