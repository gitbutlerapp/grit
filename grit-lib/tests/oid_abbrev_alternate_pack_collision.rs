//! Abbreviation must disambiguate across primary and alternate object stores.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_parse::abbreviate_object_id;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
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

fn two_blobs_sharing_seven_hex_prefix() -> (ObjectId, ObjectId, Vec<u8>, Vec<u8>) {
    let mut first_by_prefix: HashMap<String, (ObjectId, Vec<u8>)> = HashMap::new();
    for n in 0_u64..2_000_000 {
        let data = n.to_string().into_bytes();
        let oid = HashAlgo::Sha1
            .hash_object(ObjectKind::Blob, &data)
            .expect("hash");
        let prefix = oid.to_hex()[..7].to_string();
        if let Some((other, _)) = first_by_prefix.get(&prefix) {
            if *other != oid {
                let (first_oid, first_data) = first_by_prefix.remove(&prefix).expect("prefix");
                return (first_oid, oid, first_data, data);
            }
        } else {
            first_by_prefix.insert(prefix, (oid, data));
        }
    }
    panic!("failed to find two blobs sharing a 7-hex prefix");
}

fn pack_loose_object(dir: &Path, oid: &ObjectId) {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", "-q", ".git/objects/pack/pack"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("pack-objects");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{}\n", oid.to_hex()).as_bytes())
        .expect("write oid");
    let out = child.wait_with_output().expect("wait pack-objects");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(dir, &["prune-packed"]);
}

#[test]
fn abbrev_extends_prefix_when_collision_lives_in_alternate_pack() {
    let (first_oid, second_oid, first_data, second_data) = two_blobs_sharing_seven_hex_prefix();
    assert_eq!(first_oid.to_hex()[..7], second_oid.to_hex()[..7]);

    let root = TempDir::new().expect("tempdir");

    let alt_work = root.path().join("alt");
    fs::create_dir_all(&alt_work).expect("alt dir");
    git(&alt_work, &["init", "-q", "-b", "main"]);
    let alt_repo = Repository::init(&alt_work, &InitOptions::default()).expect("alt init");
    assert_eq!(
        alt_repo
            .odb()
            .write(ObjectKind::Blob, &first_data)
            .expect("write"),
        first_oid
    );
    pack_loose_object(&alt_work, &first_oid);

    let primary_work = root.path().join("primary");
    fs::create_dir_all(&primary_work).expect("primary dir");
    git(&primary_work, &["init", "-q", "-b", "main"]);
    let primary_repo =
        Repository::init(&primary_work, &InitOptions::default()).expect("primary init");
    assert_eq!(
        primary_repo
            .odb()
            .write(ObjectKind::Blob, &second_data)
            .expect("write"),
        second_oid
    );
    pack_loose_object(&primary_work, &second_oid);

    let alt_objects = fs::canonicalize(alt_work.join(".git/objects")).expect("alt objects");
    fs::create_dir_all(primary_work.join(".git/objects/info")).expect("info");
    fs::write(
        primary_work.join(".git/objects/info/alternates"),
        format!("{}\n", alt_objects.display()),
    )
    .expect("alternates");

    let local_packs = std::fs::read_dir(primary_work.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
        .count();
    assert_eq!(local_packs, 1, "primary should have exactly one local pack");

    let repo =
        Repository::discover(Some(&primary_work), &RepositoryOptions::empty()).expect("open");

    let git_short = Command::new("git")
        .current_dir(&primary_work)
        .args(["rev-parse", "--short=7", &second_oid.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-parse");
    assert!(git_short.status.success());
    let git_short = String::from_utf8_lossy(&git_short.stdout).trim().to_owned();

    let grit_short = abbreviate_object_id(&repo, second_oid, 7).expect("abbrev");
    assert_eq!(
        grit_short, git_short,
        "grit must match git when a 7-hex collision spans primary and alternate packs"
    );
    assert!(
        grit_short.len() > 7,
        "expected extended abbreviation, got {grit_short}"
    );
}
