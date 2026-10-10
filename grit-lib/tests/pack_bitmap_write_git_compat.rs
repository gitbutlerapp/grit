//! Pack bitmap writer compatibility with system Git.

use std::collections::HashSet;
use std::process::Command;
use std::time::SystemTime;

use grit_lib::objects::ObjectId;
use grit_lib::pack::{read_local_pack_indexes, read_pack_index};
use grit_lib::pack_bitmap::{BitmapIndex, PackBitmapWriteOptions, PackBitmapWriter};
use grit_lib::pack_name_hash::pack_name_hash;
use grit_lib::pack_rev::{rev_path_for_index, verify_pack_rev_file_contents};
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

fn build_rich_history_with_nested_tree(repo: &RepoFixture) {
    build_rich_history(repo);
    std::fs::create_dir_all(repo.path().join("nested/deep")).unwrap();
    std::fs::write(repo.path().join("nested/deep/leaf.txt"), b"leaf").unwrap();
    git_ok(repo, &["add", "nested/deep/leaf.txt"]);
    git_ok(repo, &["commit", "-m", "nested tree"]);
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
}

fn largest_pack_idx(repo: &RepoFixture) -> std::path::PathBuf {
    let indexes = read_local_pack_indexes(&repo.objects_dir()).expect("indexes");
    indexes
        .into_iter()
        .max_by_key(|idx| idx.len())
        .expect("pack idx")
        .idx_path
}

fn grit_write_bitmap(repo: &Repository, fixture: &RepoFixture, options: PackBitmapWriteOptions) {
    let idx = largest_pack_idx(fixture);
    PackBitmapWriter::write(repo, &idx, &options, SystemTime::UNIX_EPOCH).expect("write bitmap");
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

fn all_commits(repo: &Repository) -> Vec<ObjectId> {
    let dir = repo.work_tree.as_deref().unwrap_or(&repo.git_dir);
    let out = Command::new("git")
        .current_dir(dir)
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

fn verify_all_commit_bitmaps(repo: &Repository, index: &BitmapIndex) {
    for commit in all_commits(repo) {
        let Some(bm) = index.commit_bitmap(&commit) else {
            continue;
        };
        let expected = reachability_positions(repo, index, &commit);
        let got: HashSet<u32> = bm.positions().collect();
        assert_eq!(got, expected, "bitmap mismatch for {commit}");
    }
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

fn historical_commits(repo: &RepoFixture, n: usize) -> Vec<ObjectId> {
    git_stdout(repo, &["rev-list", "--all"])
        .lines()
        .filter_map(|l| ObjectId::from_hex(l.trim()).ok())
        .take(n)
        .collect()
}

#[test]
fn grit_bitmap_passes_git_test_bitmap() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history_with_nested_tree(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
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
    for commit in historical_commits(&fixture, 8) {
        let out = Command::new("git")
            .current_dir(fixture.path())
            .args(["rev-list", "--test-bitmap", &commit.to_hex()])
            .output()
            .expect("test-bitmap");
        assert!(
            out.status.success(),
            "git --test-bitmap history {commit}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn git_counts_match_with_grit_bitmap() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
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
fn grit_reader_roundtrip() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let n = index.selected_commit_count();
    assert!(n > 0);
    for pos in 0..index.object_count() {
        let _ = index.oid_at(pos).expect("oid");
        if index.has_lookup_table() {
            continue;
        }
    }
    verify_all_commit_bitmaps(&repo, &index);
}

#[test]
fn lookup_table_and_hash_cache_variants() {
    let combos = [(false, false), (false, true), (true, false), (true, true)];
    for (lookup, hash_cache) in combos {
        let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
        build_rich_history(&fixture);
        git_ok(&fixture, &["repack", "-ad"]);
        let repo = open_repo(fixture.path());
        grit_write_bitmap(
            &repo,
            &fixture,
            PackBitmapWriteOptions {
                full_dag: true,
                hash_cache,
                lookup_table: lookup,
                prefer_bitmap_tips: Vec::new(),
            },
        );
        let out = Command::new("git")
            .current_dir(fixture.path())
            .args(["rev-list", "--test-bitmap", "HEAD"])
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "lookup={lookup} hash_cache={hash_cache}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn grit_writes_rev_sidecar_when_missing() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let idx_path = largest_pack_idx(&fixture);
    let rev_path = rev_path_for_index(&idx_path);
    if rev_path.is_file() {
        std::fs::remove_file(&rev_path).expect("remove rev");
    }
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
    let index = read_pack_index(&idx_path).expect("idx");
    let rev_bytes = std::fs::read(&rev_path).expect("rev written");
    verify_pack_rev_file_contents(&rev_bytes, &index, "fixture.rev").expect("valid rev");
}

#[test]
fn deterministic_output() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    let idx = largest_pack_idx(&fixture);
    let opts = PackBitmapWriteOptions::default();
    let path1 = PackBitmapWriter::write(&repo, &idx, &opts, SystemTime::UNIX_EPOCH).expect("w1");
    let bytes1 = std::fs::read(&path1).expect("read1");
    std::fs::remove_file(&path1).expect("rm");
    let path2 = PackBitmapWriter::write(&repo, &idx, &opts, SystemTime::UNIX_EPOCH).expect("w2");
    let bytes2 = std::fs::read(&path2).expect("read2");
    assert_eq!(bytes1, bytes2);
}

#[test]
fn hash_cache_entries_match_path_name_hashes() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    git_ok(&fixture, &["checkout", "-b", "main"]);
    for i in 0..32 {
        let name = format!("path-{i:02}.txt");
        std::fs::write(fixture.path().join(&name), format!("body-{i}")).unwrap();
        git_ok(&fixture, &["add", &name]);
        git_ok(&fixture, &["commit", "-m", &format!("add {name}")]);
    }
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(
        &repo,
        &fixture,
        PackBitmapWriteOptions {
            hash_cache: true,
            ..Default::default()
        },
    );
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    for i in 0..32 {
        let name = format!("path-{i:02}.txt");
        let rev_arg = format!("HEAD:{name}");
        let hex = git_stdout(&fixture, &["rev-parse", &rev_arg]);
        let oid = ObjectId::from_hex(hex.trim()).expect("oid");
        let pos = index.position_of(&oid).expect("blob in pack");
        assert_eq!(
            index.name_hash(pos),
            Some(pack_name_hash(&name)),
            "name hash for {name} at bitmap pos {pos}"
        );
    }
}

#[test]
fn full_dag_allows_extra_unreachable_pack_object() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let mut hash_child = Command::new("git")
        .current_dir(fixture.path())
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("hash-object");
    hash_child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"orphan blob not reachable from refs\n")
        .expect("write");
    let hash_out = hash_child.wait_with_output().expect("wait");
    assert!(hash_out.status.success());
    let orphan_hex = String::from_utf8(hash_out.stdout)
        .unwrap()
        .trim()
        .to_string();
    let mut object_list = git_stdout(&fixture, &["rev-list", "--objects", "--all"]);
    object_list.push('\n');
    object_list.push_str(&orphan_hex);
    object_list.push('\n');
    let prefix = fixture.objects_dir().join("pack/extra-unreachable");
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(fixture.path())
        .args(["pack-objects", &prefix.to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("pack-objects");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(object_list.as_bytes())
        .expect("write");
    let pack_out = child.wait_with_output().expect("wait");
    assert!(pack_out.status.success());
    let pack_hash = String::from_utf8(pack_out.stdout)
        .unwrap()
        .trim()
        .to_string();
    let extra_idx = fixture
        .objects_dir()
        .join(format!("pack/extra-unreachable-{pack_hash}.idx"));
    let repo = open_repo(fixture.path());
    PackBitmapWriter::write(
        &repo,
        &extra_idx,
        &PackBitmapWriteOptions::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect("FULL_DAG pack with unreachable blob");
}

#[test]
fn not_closed_pack_is_rejected() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    let partial_list = git_stdout(&fixture, &["rev-list", "--objects", "HEAD~1"]);
    let prefix = fixture.objects_dir().join("pack/partial");
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(fixture.path())
        .args(["pack-objects", &prefix.to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("pack-objects");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(partial_list.as_bytes())
        .expect("write");
    let pack_out = child.wait_with_output().expect("wait");
    assert!(pack_out.status.success());
    let pack_hash = String::from_utf8(pack_out.stdout)
        .unwrap()
        .trim()
        .to_string();
    let partial_idx = fixture
        .objects_dir()
        .join(format!("pack/partial-{pack_hash}.idx"));
    let err = PackBitmapWriter::write(
        &repo,
        &partial_idx,
        &PackBitmapWriteOptions::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect_err("partial pack");
    assert!(matches!(
        err,
        grit_lib::pack_bitmap::PackBitmapWriteError::NotClosed { .. }
    ));
}

#[test]
fn sha256_write() {
    let fixture = RepoFixture::init(HashAlgo::Sha256).expect("init");
    build_rich_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
    let out = Command::new("git")
        .current_dir(fixture.path())
        .args(["rev-list", "--test-bitmap", "HEAD"])
        .output()
        .expect("git");
    assert!(out.status.success());
}

/// When `/tmp/git.git` (or `GRIT_GIT_GIT_BARE`) exists, compare grit writer output size to Git's.
#[test]
#[ignore = "expensive; run locally after `git repack -ad` on a bare git.git clone"]
fn git_git_sized_write_smoke() {
    let bare = std::env::var("GRIT_GIT_GIT_BARE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/git.git"));
    if !bare.is_dir() {
        return;
    }
    let pack_dir = bare.join("objects/pack");
    for entry in std::fs::read_dir(&pack_dir).into_iter().flatten().flatten() {
        if entry.path().extension().is_some_and(|e| e == "bitmap") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let repo = Repository::open(&bare, None).expect("open");
    let idx = read_local_pack_indexes(&bare.join("objects"))
        .expect("idx")
        .into_iter()
        .max_by_key(|i| i.len())
        .expect("pack");
    let start = std::time::Instant::now();
    let path = PackBitmapWriter::write(
        &repo,
        &idx.idx_path,
        &PackBitmapWriteOptions::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect("write");
    let elapsed = start.elapsed();
    let bytes = std::fs::read(&path).expect("read bitmap");
    let entries = u32::from_be_bytes(bytes[8..12].try_into().expect("header"));
    eprintln!(
        "git_git grit_write_ms={} grit_bitmap_bytes={} grit_bitmap_entries={}",
        elapsed.as_millis(),
        bytes.len(),
        entries
    );
    let git_test = Command::new("git")
        .current_dir(&bare)
        .args(["rev-list", "--test-bitmap", "HEAD"])
        .output()
        .expect("test-bitmap");
    assert!(
        git_test.status.success(),
        "{}",
        String::from_utf8_lossy(&git_test.stderr)
    );
}
