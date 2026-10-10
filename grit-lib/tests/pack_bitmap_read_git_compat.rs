//! Pack / MIDX `.bitmap` read compatibility with system Git.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticsHandle, Warning};
use grit_lib::objects::ObjectId;
use grit_lib::pack_bitmap::BitmapIndex;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_test_support::{HashAlgo, RepoFixture};

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

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::discover(Some(path)).expect("open repo")
}

fn head_commit(repo: &RepoFixture) -> ObjectId {
    ObjectId::from_hex(&git_stdout(repo, &["rev-parse", "HEAD"])).expect("head")
}

fn all_commits(repo: &Repository) -> Vec<ObjectId> {
    let out = Command::new("git")
        .current_dir(repo.work_tree.as_ref().unwrap_or(&repo.git_dir))
        .args(["rev-list", "--all"])
        .output()
        .expect("rev-list");
    assert!(out.status.success());
    out.stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| ObjectId::from_hex(std::str::from_utf8(l).unwrap().trim()).expect("oid"))
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

fn decoded_commit_bitmaps(repo: &Repository, index: &BitmapIndex) -> Vec<ObjectId> {
    all_commits(repo)
        .into_iter()
        .filter(|c| index.commit_bitmap(c).is_some())
        .collect()
}

fn assert_commit_bitmaps_decode(repo: &Repository, index: &BitmapIndex) {
    assert!(
        index.selected_commit_count() > 0,
        "bitmap file reports zero selected commits"
    );
    let decoded = decoded_commit_bitmaps(repo, index);
    assert!(
        !decoded.is_empty(),
        "expected at least one commit reachability bitmap to decode (file has {} entries)",
        index.selected_commit_count()
    );
    assert!(
        decoded.len() <= index.selected_commit_count() as usize,
        "decoded more commit bitmaps ({}) than on-disk entries ({})",
        decoded.len(),
        index.selected_commit_count()
    );
    let head_out = Command::new("git")
        .current_dir(repo.work_tree.as_ref().unwrap_or(&repo.git_dir))
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse HEAD");
    assert!(head_out.status.success());
    let head =
        ObjectId::from_hex(std::str::from_utf8(&head_out.stdout).unwrap().trim()).expect("head");
    assert!(
        index.commit_bitmap(&head).is_some(),
        "HEAD commit {head} must have a decoded bitmap"
    );
}

fn assert_every_bitmapped_commit_matches(repo: &Repository, index: &BitmapIndex) {
    assert_commit_bitmaps_decode(repo, index);
    for commit in decoded_commit_bitmaps(repo, index) {
        let Some(bm) = index.commit_bitmap(&commit) else {
            panic!("commit {commit} was listed as decoded but commit_bitmap returned None");
        };
        let expected = reachability_positions(repo, index, &commit);
        let got: HashSet<u32> = bm.positions().collect();
        assert_eq!(got, expected, "reachability mismatch for commit {commit}");
    }
}

fn build_rich_history(repo: &RepoFixture) {
    git_ok(repo, &["checkout", "-b", "main"]);
    std::fs::write(repo.path().join("readme"), b"v1").unwrap();
    git_ok(repo, &["add", "readme"]);
    git_ok(repo, &["commit", "-m", "root"]);
    std::fs::write(repo.path().join("readme"), b"v2").unwrap();
    git_ok(repo, &["commit", "-am", "second"]);
    git_ok(repo, &["checkout", "-b", "side"]);
    std::fs::write(repo.path().join("side.txt"), b"side").unwrap();
    git_ok(repo, &["add", "side.txt"]);
    git_ok(repo, &["commit", "-m", "side"]);
    git_ok(repo, &["checkout", "main"]);
    git_ok(repo, &["merge", "--no-ff", "side", "-m", "merge side"]);
    git_ok(repo, &["tag", "-a", "annotated", "-m", "anno"]);
    git_ok(repo, &["tag", "tree-tag", "HEAD^{tree}"]);
    let blob = repo
        .git(&["rev-parse", "HEAD:readme"])
        .stdout
        .trim()
        .to_string();
    git_ok(repo, &["tag", "blob-tag", &blob]);
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
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "pack-objects failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

/// Three retained packs (duplicate objects across p1/p2), MIDX bitmap, preferred pack.
fn build_three_pack_midx_bitmap(repo: &RepoFixture) -> String {
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
    assert_ne!(p1_hash, p2_hash, "expected two distinct packs");

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
    assert!(
        idx_count >= 3,
        "expected at least three pack indexes, found {idx_count}"
    );

    let preferred = format!("pack-{p1_hash}.pack");
    git_ok(
        repo,
        &[
            "multi-pack-index",
            "write",
            "--bitmap",
            &format!("--preferred-pack={preferred}"),
        ],
    );
    preferred
}

#[test]
fn opens_pack_bitmap_when_larger_pack_has_no_sidecar() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    git_ok(&fixture, &["checkout", "-b", "main"]);
    std::fs::write(fixture.path().join("seed.txt"), b"seed").unwrap();
    git_ok(&fixture, &["add", "seed.txt"]);
    git_ok(&fixture, &["commit", "-m", "seed"]);
    git_ok(&fixture, &["repack", "-adb"]);
    let bitmapped_commit = head_commit(&fixture);

    for i in 0..100 {
        std::fs::write(
            fixture.path().join(format!("bulk-{i}.txt")),
            format!("data-{i}"),
        )
        .unwrap();
        git_ok(&fixture, &["add", &format!("bulk-{i}.txt")]);
        git_ok(&fixture, &["commit", "-m", &format!("bulk {i}")]);
    }
    git_ok(
        &fixture,
        &[
            "pack-objects",
            "--all",
            "--unpacked",
            &fixture.objects_dir().join("pack/large").to_string_lossy(),
        ],
    );

    let pack_dir = fixture.objects_dir().join("pack");
    let idx_count = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "idx"))
        .count();
    assert!(
        idx_count >= 2,
        "expected multiple pack indexes, found {idx_count}"
    );
    let bitmap_count = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "bitmap"))
        .count();
    assert_eq!(
        bitmap_count, 1,
        "expected exactly one pack bitmap sidecar, found {bitmap_count}"
    );

    let git_test = Command::new("git")
        .current_dir(fixture.path())
        .args(["rev-list", "--test-bitmap", &bitmapped_commit.to_hex()])
        .output()
        .expect("rev-list --test-bitmap");
    assert!(
        git_test.status.success(),
        "git rev-list --test-bitmap failed: {}",
        String::from_utf8_lossy(&git_test.stderr)
    );

    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo)
        .expect("open")
        .expect("pack bitmap must open when a smaller pack retains the sidecar");
    assert!(
        index.commit_bitmap(&bitmapped_commit).is_some(),
        "bitmapped commit must decode when a larger pack has no .bitmap"
    );
}

#[test]
fn commit_bitmap_head_decodes_after_repack() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-adb"]);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = head_commit(&fixture);
    assert!(
        index.commit_bitmap(&head).is_some(),
        "HEAD commit bitmap must decode after repack -adb"
    );
}

#[test]
fn reads_git_repack_bitmap_every_entry_matches_walk() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-adb"]);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    assert_every_bitmapped_commit_matches(&repo, &index);
}

#[test]
fn reads_git_repack_bitmap_with_lookup_table() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(
        &fixture,
        &["-c", "pack.writeBitmapLookupTable=true", "repack", "-adb"],
    );
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    assert!(index.has_lookup_table());
    assert_every_bitmapped_commit_matches(&repo, &index);
}

#[test]
fn reads_git_repack_bitmap_without_hash_cache() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(
        &fixture,
        &["-c", "pack.writeBitmapHashCache=false", "repack", "-adb"],
    );
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    assert_every_bitmapped_commit_matches(&repo, &index);
}

#[test]
fn reads_git_midx_bitmap() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_three_pack_midx_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo)
        .expect("open")
        .expect("midx bitmap");
    assert_every_bitmapped_commit_matches(&repo, &index);
}

#[test]
fn name_hash_cache_matches_pack_name_hash() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-adb"]);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let n = index.object_count();
    for pos in 0..n {
        let Some(cached) = index.name_hash(pos) else {
            continue;
        };
        let oid = index.oid_at(pos).expect("oid");
        let path = Command::new("git")
            .current_dir(fixture.path())
            .args(["rev-list", "--objects", "--all"])
            .output()
            .expect("rev-list");
        let mut name = None;
        let oid_hex = oid.to_hex();
        for line in std::str::from_utf8(&path.stdout).unwrap().lines() {
            let mut parts = line.split_whitespace();
            let id = parts.next().unwrap();
            if id == oid_hex {
                name = parts.next().map(str::to_string);
                break;
            }
        }
        if let Some(pathname) = name {
            let v1 = grit_lib::pack_name_hash::pack_name_hash(&pathname);
            let v2 = grit_lib::pack_name_hash::pack_name_hash_v2(pathname.as_bytes());
            assert!(
                cached == v1 || cached == v2,
                "pos {pos}: cached {cached} v1 {v1} v2 {v2} path {pathname}"
            );
        }
    }
}

#[test]
fn sha256_repo_bitmap_reads() {
    let fixture = RepoFixture::init(HashAlgo::Sha256).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-adb"]);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    assert!(index.object_count() > 0);
    assert_every_bitmapped_commit_matches(&repo, &index);
}

#[test]
fn zero_byte_and_corrupt_bitmap_fall_back_with_warning() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-adb"]);
    let pack_dir = fixture.objects_dir().join("pack");
    let bitmap = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "bitmap"))
        .expect("bitmap path");
    let good = std::fs::read(&bitmap).unwrap();
    let mut perms = std::fs::metadata(&bitmap).unwrap().permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&bitmap, perms).unwrap();

    std::fs::write(&bitmap, []).unwrap();
    let sink = std::sync::Arc::new(CollectingDiagnostics::new());
    let mut repo = open_repo(fixture.path());
    repo.set_diagnostics(std::sync::Arc::clone(&sink) as DiagnosticsHandle);
    assert!(BitmapIndex::open(&repo).expect("open").is_none());
    assert!(sink
        .warnings()
        .iter()
        .any(|w| matches!(w, Warning::PackBitmapIgnored { .. })));

    std::fs::write(&bitmap, &good).unwrap();
    std::fs::write(&bitmap, &good[..good.len().saturating_sub(20)]).unwrap();
    let sink2 = std::sync::Arc::new(CollectingDiagnostics::new());
    let mut repo2 = open_repo(fixture.path());
    repo2.set_diagnostics(std::sync::Arc::clone(&sink2) as DiagnosticsHandle);
    assert!(BitmapIndex::open(&repo2).expect("open").is_none());

    std::fs::write(&bitmap, &good).unwrap();
    let mut bad = good.clone();
    if bad.len() > 24 {
        bad[20] ^= 0xff;
    }
    std::fs::write(&bitmap, &bad).unwrap();
    let sink3 = std::sync::Arc::new(CollectingDiagnostics::new());
    let mut repo3 = open_repo(fixture.path());
    repo3.set_diagnostics(std::sync::Arc::clone(&sink3) as DiagnosticsHandle);
    assert!(BitmapIndex::open(&repo3).expect("open").is_none());
}
