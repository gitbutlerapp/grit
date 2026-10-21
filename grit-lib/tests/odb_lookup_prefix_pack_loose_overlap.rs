//! Prefix lookup must see collisions across pack and loose layers (Git disambiguate parity).

use grit_lib::object_inspect::abbreviate_for_display;
use grit_lib::objects::odb::BULK_NEW_LOOSE_WRITE;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::Repository;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn loose_object_path(objects: &Path, oid: &grit_lib::objects::ObjectId) -> std::path::PathBuf {
    let hex = oid.to_hex();
    objects.join(&hex[..2]).join(&hex[2..])
}

fn assert_loose_object_exists(objects: &Path, oid: &grit_lib::objects::ObjectId) {
    assert!(
        loose_object_path(objects, oid).is_file(),
        "expected loose object for {}",
        oid.to_hex()
    );
}

fn two_blobs_sharing_seven_hex_prefix() -> (
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
    Vec<u8>,
    Vec<u8>,
) {
    let mut first_by_prefix: HashMap<String, (grit_lib::objects::ObjectId, Vec<u8>)> =
        HashMap::new();
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

#[test]
fn lookup_prefix_finds_loose_collision_when_pack_object_is_also_loose() {
    let (packed_oid, collision_oid, packed_data, collision_data) =
        two_blobs_sharing_seven_hex_prefix();
    assert_eq!(packed_oid.to_hex()[..7], collision_oid.to_hex()[..7]);

    let root = TempDir::new().expect("tempdir");
    let dir = root.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("blob-a"), &packed_data).expect("write");
    git(dir, &["add", "blob-a"]);
    git(dir, &["commit", "-qm", "packed"]);
    git(dir, &["repack", "-a", "-d"]);

    let git_dir = dir.join(".git");
    let objects = git_dir.join("objects");
    assert!(
        !loose_object_path(&objects, &packed_oid).is_file(),
        "packed blob should not be loose before materialization"
    );

    let repo = Repository::open(
        git_dir.as_path(),
        &grit_lib::config::environment::RepositoryOptions::empty().with_work_tree(dir),
    )
    .expect("open grit");

    assert_eq!(
        repo.odb()
            .write_with_options(ObjectKind::Blob, &packed_data, BULK_NEW_LOOSE_WRITE)
            .expect("materialize packed blob loose"),
        packed_oid
    );
    assert_eq!(
        repo.odb()
            .write_with_options(ObjectKind::Blob, &collision_data, BULK_NEW_LOOSE_WRITE)
            .expect("write collision loose"),
        collision_oid
    );
    assert_loose_object_exists(&objects, &packed_oid);
    assert_loose_object_exists(&objects, &collision_oid);

    let prefix = &packed_oid.to_hex()[..7];
    let mut grit_matches = Vec::new();
    repo.odb()
        .lookup_prefix(prefix, 0, &mut grit_matches)
        .expect("lookup");
    grit_matches.sort_by_key(|o| o.to_hex());

    let git_spec = format!("--disambiguate={prefix}");
    let git_out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", &git_spec])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git disambiguate");
    assert!(git_out.status.success());
    let mut git_matches: Vec<String> = String::from_utf8_lossy(&git_out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    git_matches.sort();

    let grit_hex: Vec<String> = grit_matches.iter().map(|o| o.to_hex()).collect();
    assert_eq!(grit_hex, git_matches, "prefix `{prefix}` disambiguate set");

    let grit_abbrev = abbreviate_for_display(&repo, packed_oid, true).expect("abbrev");
    let git_abbrev = git(dir, &["rev-parse", "--short=7", &packed_oid.to_hex()]);
    assert_eq!(
        grit_abbrev, git_abbrev,
        "abbrev must extend past ambiguous 7-hex prefix"
    );
    assert!(grit_abbrev.len() >= 8);
    assert_ne!(collision_oid, packed_oid);

    let mut limited = Vec::new();
    repo.odb()
        .lookup_prefix(prefix, 2, &mut limited)
        .expect("limited lookup");
    assert_eq!(limited.len(), 2, "limit=2 must return both colliding oids");
    let limited_hex: HashSet<_> = limited.iter().map(|o| o.to_hex()).collect();
    assert!(limited_hex.contains(&packed_oid.to_hex()));
    assert!(limited_hex.contains(&collision_oid.to_hex()));
}
