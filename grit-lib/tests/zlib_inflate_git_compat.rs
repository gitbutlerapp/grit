//! Zlib inflate compatibility: grit-written objects must pass system `git`, and
//! grit must read git-written objects identically.

use std::process::Command;

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::transfer::{build_pack, PackBuildOptions};
use tempfile::TempDir;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    String::from_utf8(git_bytes(dir, args))
        .expect("utf-8 git output")
        .trim()
        .to_string()
}

fn git_bytes(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn init_git_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(
        dir.path(),
        &["init", "--initial-branch=main", "--object-format=sha1"],
    );
    dir
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).expect("mkdir");
    for entry in std::fs::read_dir(src).expect("read dir").flatten() {
        let ty = entry.file_type().expect("file type");
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).expect("copy");
        }
    }
}

fn index_pack_into(objects_dir: &std::path::Path, pack_bytes: &[u8], stem: &str) {
    let scratch = tempfile::tempdir().expect("scratch");
    git(
        scratch.path(),
        &[
            "init",
            "--initial-branch=main",
            "--object-format=sha1",
            "--bare",
        ],
    );
    let pack_path = scratch.path().join(format!("{stem}.pack"));
    std::fs::write(&pack_path, pack_bytes).expect("write pack");
    git(scratch.path(), &["index-pack", &format!("{stem}.pack")]);
    let pack_src = scratch.path().join("objects/pack");
    let pack_dst = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dst).expect("pack dst");
    copy_dir_recursive(&pack_src, &pack_dst);
}

#[test]
fn grit_loose_and_pack_round_trip_git_fsck_and_cat_file() {
    let grit_dir = tempfile::tempdir().expect("grit dir");
    let objects = grit_dir.path().join("objects");
    std::fs::create_dir_all(&objects).expect("objects");
    let odb = Odb::new(&objects);

    let blob_body = b"round-trip payload for zlib inflate step\n".repeat(128);
    let tree_entries = {
        let blob_oid = odb.hash(ObjectKind::Blob, &blob_body);
        let mut tree = Vec::new();
        tree.extend_from_slice(b"100644 file.txt\0");
        tree.extend_from_slice(blob_oid.as_bytes());
        tree
    };
    let blob_oid = odb.write(ObjectKind::Blob, &blob_body).expect("write blob");
    let tree_oid = odb
        .write(ObjectKind::Tree, &tree_entries)
        .expect("write tree");

    let pack_bytes = build_pack(
        &odb,
        &[blob_oid, tree_oid],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    index_pack_into(&objects, &pack_bytes, "roundtrip");

    let git_dir = init_git_repo();
    let git_objects = git_dir.path().join(".git").join("objects");
    copy_dir_recursive(&objects, &git_objects);

    git(git_dir.path(), &["fsck", "--strict"]);

    let grit_blob = odb.read(&blob_oid).expect("grit read blob");
    let git_blob = git_bytes(git_dir.path(), &["cat-file", "blob", &blob_oid.to_hex()]);
    assert_eq!(git_blob, grit_blob.data);

    let grit_tree = odb.read(&tree_oid).expect("grit read tree");
    assert_eq!(grit_tree.kind, ObjectKind::Tree);
    assert!(!grit_tree.data.is_empty());
}

#[test]
fn git_written_loose_objects_read_identically_in_grit() {
    let git_dir = init_git_repo();
    std::fs::write(
        git_dir.path().join("hello.txt"),
        b"git writes, grit reads\n",
    )
    .expect("file");
    git(git_dir.path(), &["add", "hello.txt"]);
    git(git_dir.path(), &["commit", "-m", "seed"]);
    let oid_hex = git(git_dir.path(), &["rev-parse", "HEAD:hello.txt"]);
    let oid = ObjectId::from_hex(&oid_hex).expect("blob oid");

    let grit_objects = git_dir.path().join(".git").join("objects");
    let odb = Odb::new(&grit_objects);
    let grit_obj = odb.read(&oid).expect("grit read git blob");
    let git_blob = git_bytes(git_dir.path(), &["cat-file", "blob", &oid_hex]);
    assert_eq!(grit_obj.kind, ObjectKind::Blob);
    assert_eq!(grit_obj.data, git_blob);
}

#[test]
fn corrupt_zlib_loose_object_returns_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = dir.path().join("objects");
    std::fs::create_dir_all(&objects).expect("objects");
    let odb = Odb::new(&objects);
    let oid = odb.write(ObjectKind::Blob, b"ok").expect("write");
    let path = odb.object_path(&oid);
    let mut bytes = std::fs::read(&path).expect("read loose");
    bytes.truncate(bytes.len() / 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod for test");
    }
    std::fs::write(&path, bytes).expect("truncate zlib");

    let err = odb.read(&oid).unwrap_err();
    assert!(
        matches!(err, grit_lib::error::Error::Zlib(_)),
        "expected Zlib error, got {err:?}"
    );
}
