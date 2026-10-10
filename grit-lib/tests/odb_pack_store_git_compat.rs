//! Git-produced packs vs [`PackedObjects`] (read, enumerate, promisor filter).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use grit_lib::objects::{Object, ObjectId, ObjectKind};
use grit_lib::odb::store::{ObjectStore, PackFilter, PackedObjects};
use grit_lib::pack::clear_pack_cache;
use grit_lib::pack_store::PackStore;
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
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    git_in(dir.path(), &["config", "user.name", "T"]);
    let mut content = b"base line for deep delta chain\n".to_vec();
    fs::write(dir.path().join("file.txt"), &content).expect("write");
    git_in(dir.path(), &["add", "file.txt"]);
    git_in(dir.path(), &["commit", "-m", "base", "-q"]);
    for i in 0..64 {
        content.extend_from_slice(format!("line-{i}\n").as_bytes());
        fs::write(dir.path().join("file.txt"), &content).expect("write");
        git_in(dir.path(), &["commit", "-a", "-m", &format!("c{i}"), "-q"]);
    }
    git_in(dir.path(), &["repack", "-a", "-d", "-f"]);
    git_in(dir.path(), &["gc", "--aggressive", "-q"]);
    let objects = dir.path().join(".git/objects");
    (dir, objects)
}

fn packed_store(objects: &Path) -> PackedObjects {
    clear_pack_cache();
    PackedObjects::new(Arc::new(PackStore::new(objects.to_path_buf())))
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
