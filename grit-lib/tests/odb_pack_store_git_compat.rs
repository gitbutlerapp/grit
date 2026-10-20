//! Git-produced packs vs [`PackedObjects`] (read, enumerate, promisor filter).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use grit_lib::objects::odb::store::{ObjectStore, PackFilter, PackedObjects};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::{clear_pack_cache, read_pack_index, verify_pack_and_collect};
use grit_lib::objects::pack_store::PackStore;
use grit_lib::objects::{Object, ObjectId, ObjectKind};
use tempfile::TempDir;

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_cat_file(repo: &Path, oid: &ObjectId) -> Object {
    let hex = oid.to_hex();
    let kind_line = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-t", &hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file -t");
    assert!(kind_line.status.success());
    let kind_str = String::from_utf8_lossy(&kind_line.stdout);
    let kind_str = kind_str.trim();
    let kind = match kind_str {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        "tag" => ObjectKind::Tag,
        other => panic!("unexpected git type {other}"),
    };
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", kind_str, &hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file");
    assert!(
        out.status.success(),
        "git cat-file {kind_str} {hex}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Object {
        kind,
        data: out.stdout,
    }
}

fn git_batch_all_objects(repo: &Path) -> BTreeMap<ObjectId, (ObjectKind, u64)> {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "--batch-all-objects", "--batch-check"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("batch-all-objects");
    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);
    let mut map = BTreeMap::new();
    for line in reader.lines() {
        let line = line.expect("line");
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hex = parts.next().expect("hex");
        let kind = parts.next().expect("kind");
        let size: u64 = parts.next().expect("size").parse().expect("parse size");
        let oid = ObjectId::from_hex(hex).expect("oid");
        let kind = match kind {
            "blob" => ObjectKind::Blob,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "tag" => ObjectKind::Tag,
            other => panic!("unexpected kind {other}"),
        };
        map.insert(oid, (kind, size));
    }
    assert!(child.wait().expect("wait").success());
    map
}

fn deep_delta_repo() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let (objects, _tip) = deep_delta_repo_in(dir.path(), "");
    (dir, objects)
}

fn deep_delta_repo_in(repo_root: &Path, tag: &str) -> (PathBuf, ObjectId) {
    git_in(repo_root, &["init", "-q", "-b", "main"]);
    git_in(repo_root, &["config", "user.email", "t@example.com"]);
    git_in(repo_root, &["config", "user.name", "T"]);
    let mut content = format!("base line for deep delta chain {tag}\n").into_bytes();
    fs::write(repo_root.join("file.txt"), &content).expect("write");
    git_in(repo_root, &["add", "file.txt"]);
    git_in(repo_root, &["commit", "-m", "base", "-q"]);
    for i in 0..64 {
        content.extend_from_slice(format!("line-{i}\n").as_bytes());
        fs::write(repo_root.join("file.txt"), &content).expect("write");
        git_in(repo_root, &["commit", "-a", "-m", &format!("c{i}"), "-q"]);
    }
    git_in(repo_root, &["repack", "-a", "-d", "-f"]);
    git_in(repo_root, &["gc", "--aggressive", "-q"]);
    let objects = repo_root.join(".git/objects");
    let tip = git_rev_parse_blob(repo_root, "HEAD:file.txt");
    (objects, tip)
}

fn git_rev_parse_blob(repo: &Path, spec: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", spec])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success(), "rev-parse {spec}");
    ObjectId::from_hex(std::str::from_utf8(&out.stdout).expect("utf8").trim()).expect("oid")
}

fn git_cat_file_size(repo: &Path, oid: &ObjectId) -> u64 {
    let hex = oid.to_hex();
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-s", &hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file -s");
    assert!(out.status.success(), "git cat-file -s {hex}");
    std::str::from_utf8(&out.stdout)
        .expect("utf8")
        .trim()
        .parse()
        .expect("size")
}

fn git_aggressive_single_pack_repo(seed: u32) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["config", "user.email", "t@example.com"]);
    git_in(root, &["config", "user.name", "T"]);
    let mut content = format!("seed-{seed}\n").into_bytes();
    fs::write(root.join("data.txt"), &content).expect("write");
    git_in(root, &["add", "data.txt"]);
    git_in(root, &["commit", "-m", "base", "-q"]);
    for i in 0..96 {
        content.extend_from_slice(format!("line-{seed}-{i}\n").as_bytes());
        fs::write(root.join("data.txt"), &content).expect("write");
        git_in(root, &["commit", "-a", "-m", &format!("c{i}"), "-q"]);
    }
    git_in(root, &["repack", "-a", "-d", "-f"]);
    git_in(root, &["gc", "--aggressive", "-q"]);
    let idx_path = fs::read_dir(root.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack idx");
    let idx = read_pack_index(&idx_path).expect("read idx");
    assert!(
        idx.len() >= 150,
        "fixture should contain many packed objects, got {}",
        idx.len()
    );
    (dir, idx_path)
}

fn git_fsck_and_verify_pack(repo: &Path, idx_path: &Path) {
    git_in(repo, &["fsck", "--strict"]);
    let out = Command::new("git")
        .current_dir(repo)
        .arg("verify-pack")
        .args(["-v", idx_path.to_str().expect("idx utf8")])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "git verify-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn single_local_pack_offset(objects: &Path, oid: &ObjectId) -> u64 {
    let pack_dir = objects.join("pack");
    let idx_path = fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack idx");
    let idx = read_pack_index(&idx_path).expect("read idx");
    idx.find_offset(oid)
        .expect("lookup")
        .unwrap_or_else(|| panic!("oid {} missing from {}", oid.to_hex(), idx_path.display()))
}

fn packed_store(objects: &Path) -> PackedObjects {
    clear_pack_cache();
    PackedObjects::new(Arc::new(PackStore::new(objects.to_path_buf())))
}

/// Regression: sequential [`verify_pack_and_collect`] on independent git packs must not reuse
/// delta-base cache entries from earlier packs on the same thread (implicit [`PackStore`]).
#[test]
fn verify_pack_and_collect_sequential_git_packs_without_intermediate_cache_clear() {
    clear_pack_cache();
    let mut repos: Vec<TempDir> = Vec::new();
    let mut idx_paths: Vec<PathBuf> = Vec::new();
    for seed in 0..6 {
        let (dir, idx_path) = git_aggressive_single_pack_repo(seed);
        git_fsck_and_verify_pack(dir.path(), &idx_path);
        idx_paths.push(idx_path);
        repos.push(dir);
    }
    let _keep = repos;
    for (i, idx_path) in idx_paths.iter().enumerate() {
        verify_pack_and_collect(idx_path).unwrap_or_else(|err| {
            panic!("verify_pack_and_collect repo {i} failed without clear_pack_cache between repos: {err}")
        });
    }
}

/// Regression: per-thread `PACK_READ_INFO_CACHE` must not reuse `(pack_id, offset)` across
/// distinct [`Odb`] / [`PackStore`] instances on one thread.
#[test]
fn pack_read_info_cache_scoped_per_pack_store_on_one_thread() {
    clear_pack_cache();
    let mut pair = None;
    let mut keep_dirs: Vec<TempDir> = Vec::new();
    'search: for tag_a in 0u32..24 {
        for tag_b in (tag_a + 1)..24 {
            let dir_a = TempDir::new().expect("tempdir a");
            let dir_b = TempDir::new().expect("tempdir b");
            let (objects_a, tip_a) = deep_delta_repo_in(dir_a.path(), &format!("a{tag_a}"));
            let (objects_b, tip_b) = deep_delta_repo_in(dir_b.path(), &format!("b{tag_b}"));
            let repo_a = dir_a.path();
            let repo_b = dir_b.path();
            let size_a = git_cat_file_size(repo_a, &tip_a);
            let size_b = git_cat_file_size(repo_b, &tip_b);
            if size_a == size_b {
                continue;
            }
            let off_a = single_local_pack_offset(&objects_a, &tip_a);
            let off_b = single_local_pack_offset(&objects_b, &tip_b);
            if off_a != off_b {
                continue;
            }
            keep_dirs.push(dir_a);
            keep_dirs.push(dir_b);
            pair = Some((objects_a, objects_b, tip_a, tip_b, size_a, size_b, off_a));
            break 'search;
        }
    }
    let _keep_dirs = keep_dirs;
    let (objects_a, objects_b, tip_a, tip_b, git_size_a, git_size_b, shared_offset) =
        pair.expect("find two git packs with same delta offset and different expanded sizes");
    let odb_a = Odb::new(&objects_a);
    let odb_b = Odb::new(&objects_b);
    let grit_size_a = odb_a
        .with_pack_read_context(|| odb_a.read_info(&tip_a))
        .expect("read_info a")
        .size;
    let grit_size_b = odb_b
        .with_pack_read_context(|| odb_b.read_info(&tip_b))
        .expect("read_info b")
        .size;
    assert_eq!(
        grit_size_a, git_size_a,
        "repo A at pack offset {shared_offset}"
    );
    assert_eq!(
        grit_size_b, git_size_b,
        "repo B at pack offset {shared_offset} must not reuse repo A cached info"
    );
    assert_ne!(git_size_a, git_size_b);
}

#[test]
fn git_repack_packs_stay_identical_with_tiny_delta_base_cache() {
    let (_dir, objects) = deep_delta_repo();
    let repo = _dir.path();
    let git_map = git_batch_all_objects(repo);
    let git_dir = repo.join(".git");
    git_in(repo, &["config", "core.deltaBaseCacheLimit", "512"]);
    clear_pack_cache();
    let odb = Odb::new(&objects).with_config_git_dir(git_dir);
    for round in 0..3 {
        odb.with_pack_read_context(|| {
            for (oid, (git_kind, git_size)) in &git_map {
                let grit = odb.read(oid).expect("read");
                assert_eq!(grit.kind, *git_kind, "round {round} kind");
                assert_eq!(grit.data.len() as u64, *git_size, "round {round} size");
                let git_obj = git_cat_file(repo, oid);
                assert_eq!(grit.data, git_obj.data, "round {round} payload");
            }
        });
    }
}

#[test]
fn git_repack_and_gc_packs_read_identical_to_cat_file() {
    let (_dir, objects) = deep_delta_repo();
    let repo = _dir.path();
    let git_map = git_batch_all_objects(repo);
    let store = packed_store(&objects);
    for (oid, (git_kind, git_size)) in &git_map {
        let grit = store.read(oid).expect("read").expect("hit");
        assert_eq!(grit.kind, *git_kind);
        assert_eq!(grit.data.len() as u64, *git_size);
        let git_obj = git_cat_file(repo, oid);
        assert_eq!(grit.kind, git_obj.kind);
        assert_eq!(grit.data, git_obj.data);
    }
}

#[test]
fn git_packs_for_each_matches_batch_all_objects() {
    let (_dir, objects) = deep_delta_repo();
    let repo = _dir.path();
    let git_map = git_batch_all_objects(repo);
    let store = packed_store(&objects);
    let mut grit_ids = HashSet::new();
    store
        .for_each_object(&mut |oid| {
            grit_ids.insert(*oid);
            ControlFlow::<()>::Continue(())
        })
        .expect("for_each");
    assert_eq!(grit_ids.len(), git_map.len());
    for oid in grit_ids {
        assert!(git_map.contains_key(&oid));
    }
}

#[test]
fn promisor_marker_excluded_from_contains_filtered() {
    let (_dir, objects) = deep_delta_repo();
    let repo = _dir.path();
    let git_map = git_batch_all_objects(repo);
    let oid = *git_map.keys().next().expect("at least one object");
    let pack_path = fs::read_dir(objects.join("pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack file");
    fs::write(pack_path.with_extension("promisor"), b"").expect("promisor marker");
    filetime::set_file_mtime(objects.join("pack"), filetime::FileTime::now()).expect("mtime");
    clear_pack_cache();
    let store = packed_store(&objects);
    assert!(
        store.refresh().expect("refresh"),
        "sidecar flags must reload after promisor marker"
    );
    assert!(
        store.read(&oid).expect("read").is_some(),
        "PackedObjects read still resolves promisor pack objects"
    );
    assert!(
        !store
            .contains_filtered(
                &oid,
                PackFilter {
                    include_promisor: false,
                    include_cruft: true,
                },
            )
            .expect("contains_filtered"),
        "promisor-only visibility must be excluded when include_promisor=false"
    );
}
