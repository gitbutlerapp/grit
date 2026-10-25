//! Pack bitmap writer compatibility with system Git.

use std::collections::HashSet;
use std::process::Command;
use std::time::SystemTime;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::bitmap::{BitmapIndex, PackBitmapWriteOptions, PackBitmapWriter};
use grit_lib::objects::pack::{read_local_pack_indexes, read_pack_index};
use grit_lib::objects::pack_name_hash::pack_name_hash;
use grit_lib::objects::pack_rev::{rev_path_for_index, verify_pack_rev_file_contents};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
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
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("open repo")
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
    let dir = repo.work_tree().unwrap_or(repo.git_dir());
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
                verify_reachability_closure: true,
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
        grit_lib::objects::bitmap::PackBitmapWriteError::NotClosed { .. }
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
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open");
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

/// More commits than the writer bitmaps unconditionally, so only tip selection can pick the
/// commit an annotated tag of a tag names.
fn fast_import_long_history_with_nested_tag_only_tip(repo: &RepoFixture) -> ObjectId {
    let mut stream = String::new();
    for i in 1..=150u32 {
        let msg = format!("c{i}\n");
        let body = format!("{i}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\nmark :{i}\ncommitter T <t@example.com> {} +0000\ndata {}\n{msg}",
            1_000_000_000 + i,
            msg.len()
        ));
        if i > 1 {
            stream.push_str(&format!("from :{}\n", i - 1));
        }
        stream.push_str(&format!("M 644 inline f\ndata {}\n{body}\n", body.len()));
    }
    let msg = "lonely\n";
    stream.push_str(&format!(
        "commit refs/heads/lonely\nmark :999\ncommitter T <t@example.com> 900000000 +0000\ndata {}\n{msg}M 644 inline g\ndata 2\ng\n\n",
        msg.len()
    ));
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["fast-import", "--quiet"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("fast-import");
    std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), stream.as_bytes())
        .expect("write stream");
    assert!(child.wait().expect("wait").success());
    git_ok(repo, &["tag", "-a", "-m", "inner", "inner", "lonely"]);
    git_ok(repo, &["tag", "-a", "-m", "outer", "outer", "inner"]);
    git_ok(repo, &["tag", "-d", "inner"]);
    git_ok(repo, &["branch", "-D", "lonely"]);
    git_ok(repo, &["checkout", "-q", "main"]);
    ObjectId::from_hex(&git_stdout(repo, &["rev-parse", "outer^{commit}"])).expect("oid")
}

#[test]
fn nested_tag_tip_gets_a_bitmap() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    let lonely = fast_import_long_history_with_nested_tag_only_tip(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    assert!(
        index.commit_bitmap(&lonely).is_some(),
        "commit named only by a nested tag has no bitmap"
    );
    let out = Command::new("git")
        .current_dir(fixture.path())
        .args(["rev-list", "--test-bitmap", &lonely.to_hex()])
        .output()
        .expect("test-bitmap");
    assert!(
        out.status.success(),
        "git --test-bitmap {lonely}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    verify_all_commit_bitmaps(&repo, &index);
}

fn fast_import(repo: &RepoFixture, stream: &str) {
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["fast-import", "--quiet"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("fast-import");
    std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), stream.as_bytes())
        .expect("write stream");
    assert!(child.wait().expect("wait").success());
}

/// Fixed-identity, fixed-date history (so object ids are stable across Git versions) with a
/// merge, nested trees, a renamed blob, one blob at two paths in one tree, a copied subtree,
/// a gitlink, and tags naming a commit, a tree and a blob.
fn build_name_hash_fixture(repo: &RepoFixture) {
    let commit = |r: &str, mark: u32, time: u32, msg: &str| {
        format!(
            "commit refs/heads/{r}\nmark :{mark}\ncommitter T <t@example.com> {time} +0000\ndata {}\n{msg}\n",
            msg.len() + 1
        )
    };
    let file =
        |path: &str, body: &str| format!("M 644 inline {path}\ndata {}\n{body}\n", body.len() + 1);
    let mut s = String::new();
    s += &commit("main", 1, 1_000_000_001, "root");
    s += &file("a.txt", "alpha");
    s += &file("lib/mod.rs", "m1");
    s += &file("lib/deep/x.rs", "x1");
    s += &commit("main", 2, 1_000_000_002, "edit mod");
    s += "from :1\n";
    s += &file("lib/mod.rs", "m2");
    s += &commit("side", 3, 1_000_000_003, "move a into docs");
    s += "from :2\nD a.txt\n";
    s += &file("docs/a.txt", "alpha");
    s += &file("docs/b.txt", "beta");
    s += &commit("main", 4, 1_000_000_004, "gitlink and y");
    s += "from :2\nM 160000 1111111111111111111111111111111111111111 vendor/sub\n";
    s += &file("lib/deep/y.rs", "y1");
    s += &commit("main", 5, 1_000_000_005, "merge side");
    s += "from :4\nmerge :3\nD a.txt\n";
    s += &file("docs/a.txt", "alpha");
    s += &file("docs/b.txt", "beta");
    s += &commit("main", 6, 1_000_000_006, "copy lib");
    s += "from :5\nC lib lib2\n";
    s += &commit("main", 7, 1_000_000_007, "alpha at two paths");
    s += "from :6\n";
    s += &file("a.txt", "alpha");
    fast_import(repo, &s);
    git_ok(repo, &["checkout", "-q", "main"]);
    git_ok(repo, &["tag", "-a", "-m", "side tag", "side-tag", "side"]);
    git_ok(
        repo,
        &["tag", "-a", "-m", "tree tag", "tree-tag", "main~5^{tree}"],
    );
    git_ok(repo, &["tag", "blob-tag", "main:docs/b.txt"]);
}

/// `<oid> <name hash>` for every object in the bitmap, sorted by oid.
fn name_hash_dump(index: &BitmapIndex) -> String {
    let mut lines: Vec<String> = (0..index.object_count())
        .map(|pos| {
            let oid = index.oid_at(pos).expect("oid");
            let hash = index.name_hash(pos).expect("hash cache");
            format!("{oid} {hash:08x}")
        })
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Name hashes written by the writer before blob reads were removed from its tree walk.
///
/// The walk visits selected commits oldest first and overwrites a blob's hash from every
/// newly walked tree naming it (so the newest path wins unless its tree was already covered),
/// while a tree keeps the path it was first reached by. `0` means no path (roots, commits, tags).
const NAME_HASH_FIXTURE_EXPECTED: &[&str] = &[
    "04ea9a14ec64c3692b9c4d9f87387c870563a0ba 00000000",
    "07b3376956aaed77c47aabd0808bdce2a6d8e332 00000000",
    "08171685dfb0b922abf6571d79b9102be5a1b0ec 916fc000",
    "08bb2331e777f431177c40df6841c0034f89fb58 947f3c00",
    "2ac155193e8c5f16aefdb7c3e80ae9483b8988e3 83000000",
    "3b21b028535e320a424f2fda361353f8f7d01b84 00000000",
    "4a58007052a65fbc2fc3f910f2855f45a4058e74 9a590000",
    "4f5ea0414ceb4e5f73bddf75cbd2fa2d70e9c9ee 00000000",
    "547bf2d3b335d6836af266bc00b2e914eb506952 83000000",
    "550b2383c31ea966a3595d51068e46855ee9e5cf 00000000",
    "58ae7059fb3ce6c9ceac46c0b2913185a3638faf 00000000",
    "5b37c0a95e36b686ec99abbed773674f654ae259 00000000",
    "63a911f26fe84ea7fd8a863a636cfac908895ec9 947f3c00",
    "656f4f9dcc676581418546503875242f26ea8e47 94975bf0",
    "65b2df87f7df3aeedef04be96703e55ac19c2cfb 9a6f0400",
    "68ba7e4f796cbce5ed86bad3e9df986fb138d99f 94400000",
    "6ca20b1bab0e40fa534abaeabff0a01892f53a91 00000000",
    "7d0640946b560372c9265462925f64f2e7684215 00000000",
    "8e85f04be889fd1030eb967bd05fa3c3bc6e22f1 00000000",
    "8feebe089afb64b8bfe27ebe53486ec42cc78bf8 00000000",
    "99f44f57469204098a0376adb7e01a7ed9dda891 00000000",
    "9e35a1c81996422d0d7950fd2a1f8af69b3e349a 94935bf0",
    "a810a7071c2889128af9a2a7947be99b50d616e3 00000000",
    "abb0d5d713fdd663edbd98f2d76703e96dc6a703 963a8000",
    "c6f2b3b232d38f6959d4b0ab28da39dbc8e5b8c8 00000000",
    "db7d7a8c758ca8ca1ca73ce530fa0eb2de261e52 00000000",
    "e7638c71b3e69d64e186f2890b9d59213885eaf1 83000000",
    "eace2a250a25b0378d342e5281a15090edb7e90f 916fc000",
    "f84f042ae0242e83afd01ca43e2b1f71d203c18c 00000000",
];

#[test]
fn name_hashes_and_bitmaps_match_reference_output() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_name_hash_fixture(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    let repo = open_repo(fixture.path());
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    assert_eq!(
        name_hash_dump(&index),
        NAME_HASH_FIXTURE_EXPECTED.join("\n")
    );
    let hash_of = |rev: &str| {
        let oid = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", rev])).expect("oid");
        index.name_hash(index.position_of(&oid).expect("in pack"))
    };
    assert_eq!(hash_of("main:a.txt"), Some(pack_name_hash("a.txt")));
    assert_eq!(
        hash_of("main:docs/b.txt"),
        Some(pack_name_hash("docs/b.txt"))
    );
    assert_eq!(hash_of("main:lib2"), Some(pack_name_hash("lib")));
    assert_eq!(
        hash_of("main:lib/deep/x.rs"),
        Some(pack_name_hash("lib/deep/x.rs"))
    );
    let commits = all_commits(&repo);
    assert_eq!(index.selected_commit_count() as usize, commits.len());
    verify_all_commit_bitmaps(&repo, &index);
    for tip in ["main", "side"] {
        let out = Command::new("git")
            .current_dir(fixture.path())
            .args(["rev-list", "--test-bitmap", tip])
            .output()
            .expect("test-bitmap");
        assert!(
            out.status.success(),
            "git --test-bitmap {tip}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn tree_tag_closure_is_verified() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    let hash_blob = |body: &str| {
        let out = Command::new("git")
            .current_dir(fixture.path())
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                std::io::Write::write_all(
                    &mut child.stdin.take().expect("stdin"),
                    body.as_bytes(),
                )?;
                child.wait_with_output()
            })
            .expect("hash-object");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    };
    let only_in_tree = hash_blob("only reachable from the tagged tree\n");
    let mut mktree = Command::new("git")
        .current_dir(fixture.path())
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("mktree");
    std::io::Write::write_all(
        &mut mktree.stdin.take().expect("stdin"),
        format!("100644 blob {only_in_tree}\tlonely.txt\n").as_bytes(),
    )
    .expect("write");
    let tree_out = mktree.wait_with_output().expect("mktree");
    let tree = String::from_utf8(tree_out.stdout)
        .expect("utf8")
        .trim()
        .to_string();
    git_ok(&fixture, &["tag", "-a", "-m", "tree", "tree-tag", &tree]);

    let all_objects = git_stdout(&fixture, &["rev-list", "--objects", "--all"]);
    let without_blob: String = all_objects
        .lines()
        .filter(|l| !l.starts_with(&only_in_tree))
        .map(|l| format!("{l}\n"))
        .collect();
    let prefix = fixture.objects_dir().join("pack/no-tree-blob");
    let mut child = Command::new("git")
        .current_dir(fixture.path())
        .args(["pack-objects", &prefix.to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("pack-objects");
    std::io::Write::write_all(
        &mut child.stdin.take().expect("stdin"),
        without_blob.as_bytes(),
    )
    .expect("write");
    let pack_out = child.wait_with_output().expect("wait");
    assert!(pack_out.status.success());
    let pack_hash = String::from_utf8(pack_out.stdout)
        .expect("utf8")
        .trim()
        .to_string();
    let idx = fixture
        .objects_dir()
        .join(format!("pack/no-tree-blob-{pack_hash}.idx"));
    let repo = open_repo(fixture.path());
    let err = PackBitmapWriter::write(
        &repo,
        &idx,
        &PackBitmapWriteOptions::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect_err("tagged tree's blob is not in the pack");
    let missing = ObjectId::from_hex(&only_in_tree).expect("oid");
    assert_eq!(err.not_closed_missing(), Some(&[missing][..]));
}

/// Deterministic incompressible bytes, so the blob is stored whole and its zlib stream is long.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn writer_does_not_read_blob_contents() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history_with_nested_tree(&fixture);
    std::fs::write(fixture.path().join("nested/noise.bin"), noise(8192)).unwrap();
    git_ok(&fixture, &["add", "nested/noise.bin"]);
    git_ok(&fixture, &["commit", "-m", "noise"]);
    git_ok(&fixture, &["repack", "-ad"]);
    let blob = ObjectId::from_hex(&git_stdout(
        &fixture,
        &["rev-parse", "HEAD:nested/noise.bin"],
    ))
    .expect("oid");
    let idx_path = largest_pack_idx(&fixture);
    let idx = read_pack_index(&idx_path).expect("idx");
    let offset = idx
        .find_offset(&blob)
        .expect("lookup")
        .expect("blob packed");
    let mut pack = std::fs::read(&idx.pack_path).expect("read pack");
    let start = usize::try_from(offset).expect("offset") + 64;
    for b in &mut pack[start..start + 256] {
        *b ^= 0x5a;
    }
    let mut perms = std::fs::metadata(&idx.pack_path)
        .expect("stat")
        .permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&idx.pack_path, perms).expect("chmod");
    std::fs::write(&idx.pack_path, &pack).expect("write pack");

    let repo = open_repo(fixture.path());
    assert!(
        repo.odb().read(&blob).is_err(),
        "corrupting the blob's zlib stream must make it unreadable"
    );
    grit_write_bitmap(&repo, &fixture, PackBitmapWriteOptions::default());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).expect("oid");
    let pos = index.position_of(&blob).expect("blob position");
    let bitmap = index.commit_bitmap(&head).expect("HEAD bitmap");
    assert!(bitmap.positions().any(|p| p == pos));
    assert_eq!(
        index.name_hash(pos),
        Some(pack_name_hash("nested/noise.bin"))
    );
}
