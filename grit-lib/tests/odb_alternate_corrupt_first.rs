//! Alternate ordering when an earlier store holds corrupt bytes for an OID (system git oracle).

use grit_lib::error::Error;
use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_corruption_err<T>(result: Result<T, Error>) {
    match result {
        Err(Error::Zlib(_)) | Err(Error::CorruptObject(_)) => {}
        Err(other) => panic!("expected zlib/corrupt error, got {other:?}"),
        Ok(_) => panic!("expected zlib/corrupt error, got Ok"),
    }
}

#[test]
fn corrupt_first_alternate_propagates_for_read_read_info_open_stream() {
    let layout = TempDir::new().expect("tempdir");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = layout.path().join("primary.git/objects");

    let good_objects = layout.path().join("good/objects");
    let bad_objects = layout.path().join("bad/objects");
    fs::create_dir_all(&good_objects).expect("good");
    fs::create_dir_all(&bad_objects).expect("bad");

    let payload = b"alternate corruption ordering payload";
    let good_odb = Odb::new(&good_objects);
    let oid = good_odb
        .write(ObjectKind::Blob, payload)
        .expect("write good");

    let bad_odb = Odb::new(&bad_objects);
    let bad_oid = bad_odb
        .write(ObjectKind::Blob, payload)
        .expect("write bad copy");
    assert_eq!(oid, bad_oid);
    let bad_path = bad_odb.object_path(&oid);
    fs::set_permissions(&bad_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    fs::write(&bad_path, b"not-valid-zlib").expect("corrupt");

    let bad_abs = fs::canonicalize(&bad_objects).expect("bad canon");
    let good_abs = fs::canonicalize(&good_objects).expect("good canon");
    fs::create_dir_all(primary_objects.join("info")).expect("info");
    fs::write(
        primary_objects.join("info/alternates"),
        format!("{}\n{}\n", bad_abs.display(), good_abs.display()),
    )
    .expect("alternates");

    let odb = Odb::new(&primary_objects);
    assert_corruption_err(odb.read(&oid).map(|_| ()));
    assert_corruption_err(odb.read_info(&oid).map(|_| ()));
    assert_corruption_err(odb.open_stream(&oid).map(|_| ()));

    let git_dir = layout.path().join("primary.git");
    let cat = Command::new("git")
        .args(["cat-file", "blob", &oid.to_hex()])
        .env("GIT_DIR", &git_dir)
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git cat-file");
    assert!(
        !cat.status.success(),
        "git must fail on corrupt-first alternate, stdout={}",
        String::from_utf8_lossy(&cat.stdout)
    );
    let stderr = String::from_utf8_lossy(&cat.stderr).to_lowercase();
    assert!(
        stderr.contains("inflate") || stderr.contains("corrupt") || stderr.contains("bad"),
        "git stderr should mention corruption: {stderr}"
    );
}
