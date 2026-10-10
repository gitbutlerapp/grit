//! Pack generation with reachability bitmap enumeration.

use std::collections::HashSet;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::rev_list::ObjectFilter;
use grit_lib::pack_objects::{build_pack, build_pack_with_shallow_and_filter, PackBuildOptions};
use grit_lib::unpack_objects::pack_bytes_to_object_map;
use grit_test_support::{HashAlgo, RepoFixture};

fn git_ok(repo: &RepoFixture, args: &[&str]) {
    let out = repo.git(args);
    assert!(out.ok, "git {:?} failed: {}", args, out.stderr);
}

fn git_ok_path(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
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

fn repack_with_bitmap(repo: &RepoFixture) {
    git_ok(repo, &["repack", "-adb"]);
}

fn add_post_repack_commits(repo: &RepoFixture) {
    std::fs::write(repo.path().join("after.txt"), b"after bitmap repack").unwrap();
    git_ok(repo, &["add", "after.txt"]);
    git_ok(repo, &["commit", "-m", "after repack loose"]);
}

fn all_commits(repo: &Path) -> Vec<ObjectId> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-list", "--all"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-list");
    assert!(out.status.success());
    out.stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| ObjectId::from_hex(std::str::from_utf8(l).unwrap().trim()).unwrap())
        .collect()
}

fn pick_subset(commits: &[ObjectId], seed: u64, count: usize) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let n = commits.len();
    if n == 0 {
        return out;
    }
    let mut x = seed;
    for i in 0..count {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        let idx = (x as usize) % n;
        let pick = commits[(idx + i) % n];
        if !out.contains(&pick) {
            out.push(pick);
        }
    }
    out
}

fn pack_object_set(pack: &[u8], odb: &Odb) -> HashSet<ObjectId> {
    let map = pack_bytes_to_object_map(pack, odb).expect("parse pack");
    map.keys().copied().collect()
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

fn rev_parse(repo: &Path, rev: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", rev])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success(), "rev-parse {rev}");
    ObjectId::from_hex(std::str::from_utf8(&out.stdout).unwrap().trim()).unwrap()
}

fn build_three_commits(repo: &RepoFixture) {
    git_ok(repo, &["checkout", "-b", "main"]);
    for i in 0..3 {
        std::fs::write(repo.path().join(format!("f{i}.txt")), format!("v{i}")).unwrap();
        git_ok(repo, &["add", "."]);
        git_ok(repo, &["commit", "-m", &format!("c{i}")]);
    }
}

#[test]
fn bitmap_and_walk_honor_explicit_shallow_boundary() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_three_commits(&fixture);
    repack_with_bitmap(&fixture);

    let git_dir = fixture.path().join(".git");
    let odb = open_odb(&git_dir);
    let head = rev_parse(fixture.path(), "HEAD");
    let boundary = rev_parse(fixture.path(), "HEAD~1");
    let mut shallow = HashSet::new();
    shallow.insert(boundary);

    let opts_on = PackBuildOptions {
        use_bitmaps: true,
        delta: false,
        ..PackBuildOptions::default()
    };
    let opts_off = PackBuildOptions {
        use_bitmaps: false,
        delta: false,
        ..PackBuildOptions::default()
    };
    let pack_on = build_pack_with_shallow_and_filter(
        &odb, &[head], &[], &shallow, None, &opts_on,
    )
    .expect("bitmap");
    let pack_off = build_pack_with_shallow_and_filter(
        &odb, &[head], &[], &shallow, None, &opts_off,
    )
    .expect("walk");
    let on_set = pack_object_set(&pack_on, &odb);
    let off_set = pack_object_set(&pack_off, &odb);
    assert_eq!(
        on_set, off_set,
        "bitmap path must honor explicit shallow_grafts (got {} vs {} objects)",
        on_set.len(),
        off_set.len()
    );
    assert!(
        on_set.len() < 9,
        "shallow boundary should exclude ancestors beyond HEAD~1"
    );
}

#[test]
fn bitmap_and_walk_select_identical_objects() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    add_post_repack_commits(&fixture);

    let git_dir = fixture.path().join(".git");
    let odb = open_odb(&git_dir);
    let commits = all_commits(fixture.path());
    let head = ObjectId::from_hex(
        &String::from_utf8(
            Command::new("git")
                .current_dir(fixture.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string(),
    )
    .unwrap();

    for seed in 1u64..12 {
        let wants = vec![head];
        let haves = if seed % 3 == 0 {
            pick_subset(&commits, seed, 3)
        } else {
            pick_subset(&commits, seed, 1)
        };
        let opts_on = PackBuildOptions {
            use_bitmaps: true,
            delta: false,
            ..PackBuildOptions::default()
        };
        let opts_off = PackBuildOptions {
            use_bitmaps: false,
            delta: false,
            ..PackBuildOptions::default()
        };
        let pack_on = build_pack(&odb, &wants, &haves, &opts_on).expect("bitmap pack");
        let pack_off = build_pack(&odb, &wants, &haves, &opts_off).expect("walk pack");
        let on_set = pack_object_set(&pack_on, &odb);
        let off_set = pack_object_set(&pack_off, &odb);
        assert_eq!(
            on_set,
            off_set,
            "seed {seed}: bitmap vs walk object sets differ ({} vs {})",
            on_set.len(),
            off_set.len()
        );
    }
}

#[test]
fn filters_respected_with_bitmaps() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);

    let git_dir = fixture.path().join(".git");
    let odb = open_odb(&git_dir);
    let head = ObjectId::from_hex(
        Command::new("git")
            .current_dir(fixture.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout
            .split(|&b| b == b'\n')
            .next()
            .map(|l| std::str::from_utf8(l).unwrap().trim())
            .unwrap(),
    )
    .unwrap();

    let filter = ObjectFilter::BlobNone;
    let empty = HashSet::new();
    let opts = PackBuildOptions {
        use_bitmaps: true,
        delta: false,
        ..PackBuildOptions::default()
    };
    let pack = build_pack_with_shallow_and_filter(
        &odb, &[head], &[], &empty, Some(&filter), &opts,
    )
    .expect("filtered pack");
    let map = pack_bytes_to_object_map(&pack, &odb).expect("parse");
    assert!(
        map.values()
            .all(|o| o.kind != grit_lib::objects::ObjectKind::Blob),
        "blob:none filter must exclude blobs"
    );
}

fn find_binary(name: &str) -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.parent()?;
    for profile in ["debug", "release"] {
        let p = workspace.join("target").join(profile).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn free_port() -> Option<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let p = l.local_addr().ok()?.port();
    Some(p)
}

struct ServerHandle {
    _child: Child,
    root: PathBuf,
}

fn spawn_grit_http_server(bare: &Path, port: u16) -> Option<ServerHandle> {
    let server_bin = find_binary("grit-http-server")?;
    let root = bare.parent()?.to_path_buf();
    let child = Command::new(&server_bin)
        .args([
            "--root",
            root.to_str()?,
            "--bind",
            &format!("127.0.0.1:{port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    for _ in 0..50 {
        if TcpListener::bind(("127.0.0.1", port)).is_err() {
            return Some(ServerHandle {
                _child: child,
                root,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn prepare_bare_with_git_bitmap(source: &Path) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_ok_path(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            "repo.git",
        ],
    );
    let bare = tmp.path().join("repo.git");
    git_ok_path(&bare, &["repack", "-adb"]);
    (tmp, bare)
}

#[test]
fn fetch_from_grit_server_with_bitmaps_fsck_clean() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    add_post_repack_commits(&fixture);

    let port = free_port().expect("port");
    let (_tmp, bare) = prepare_bare_with_git_bitmap(fixture.path());
    let Some(_handle) = spawn_grit_http_server(&bare, port) else {
        eprintln!("SKIP: grit-http-server unavailable");
        return;
    };

    let clone_dir = tempfile::tempdir().expect("clone");
    let url = format!("http://127.0.0.1:{port}/repo.git");
    git_ok_path(clone_dir.path(), &["clone", "-q", url.as_str(), "work"]);
    let work = clone_dir.path().join("work");
    git_ok_path(&work, &["fsck", "--full"]);
}

#[test]
fn push_with_bitmaps_fsck_clean() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);

    let port = free_port().expect("port");
    let (_tmp, bare) = prepare_bare_with_git_bitmap(fixture.path());
    let Some(_handle) = spawn_grit_http_server(&bare, port) else {
        eprintln!("SKIP: grit-http-server unavailable");
        return;
    };

    let client = tempfile::tempdir().expect("client");
    let url = format!("http://127.0.0.1:{port}/repo.git");
    git_ok_path(client.path(), &["clone", "-q", url.as_str(), "work"]);
    let work = client.path().join("work");
    std::fs::write(work.join("push.txt"), b"push\n").unwrap();
    git_ok_path(&work, &["add", "push.txt"]);
    git_ok_path(&work, &["commit", "-m", "push commit"]);
    git_ok_path(&work, &["push", "origin", "main"]);
    git_ok_path(&bare, &["fsck", "--full"]);
}
