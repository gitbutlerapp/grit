//! `index` line OID abbreviations must match system `git` when `core.abbrev` collides in the ODB.

use std::io::Cursor;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{write_commit_log_patch, DiffOptions, LogPatchCommitHeader, LogPatchOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn two_blobs_sharing_seven_hex_prefix() -> (ObjectId, ObjectId, Vec<u8>, Vec<u8>) {
    use std::collections::HashMap;

    use grit_lib::objects::{HashAlgo, ObjectKind};

    let mut first_by_prefix: HashMap<String, (ObjectId, Vec<u8>)> = HashMap::new();
    for n in 0_u64..2_000_000 {
        let data = n.to_string().into_bytes();
        let oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, &data);
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

fn index_line_from_patch(patch: &[u8]) -> Option<String> {
    String::from_utf8_lossy(patch)
        .lines()
        .find(|l| l.starts_with("index "))
        .map(str::to_owned)
}

#[test]
fn index_line_abbrev_for_colliding_blobs_matches_git_show() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let odb = repo.odb();
    let (old_oid, new_oid, old_data, new_data) = two_blobs_sharing_seven_hex_prefix();
    assert_eq!(old_oid.to_hex()[..7], new_oid.to_hex()[..7]);

    std::fs::write(dir.path().join("file.txt"), &old_data).unwrap();
    git_in(dir.path(), &["add", "file.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "old"]);

    std::fs::write(dir.path().join("file.txt"), &new_data).unwrap();
    git_in(dir.path(), &["add", "file.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "new"]);

    let git_patch = git_in(
        dir.path(),
        &["show", "-p", "--no-color", "--format=%H", "HEAD"],
    );
    let git_index = index_line_from_patch(&git_patch).expect("git index line");

    let head_raw = odb
        .read(
            &grit_lib::objects::ObjectId::from_hex(
                String::from_utf8_lossy(&git_in(dir.path(), &["rev-parse", "HEAD"])).trim(),
            )
            .unwrap(),
        )
        .unwrap();
    let head_commit = grit_lib::objects::parse_commit(&head_raw.data).unwrap();
    let parent_raw = odb.read(&head_commit.parents[0]).unwrap();
    let parent_commit = grit_lib::objects::parse_commit(&parent_raw.data).unwrap();
    let entries =
        grit_lib::diff::diff_trees(odb, Some(&parent_commit.tree), Some(&head_commit.tree), "")
            .expect("tree diff");
    let file = entries
        .iter()
        .find(|e| e.new_path.as_deref() == Some("file.txt"))
        .expect("file.txt diff");
    assert_ne!(
        file.old_oid,
        file.new_oid,
        "modified file must have distinct blob oids (old={}, new={})",
        file.old_oid.to_hex(),
        file.new_oid.to_hex()
    );
    let head = git_in(dir.path(), &["rev-parse", "HEAD"]);
    let head_hex = String::from_utf8_lossy(&head);
    let head_oid = grit_lib::objects::ObjectId::from_hex(head_hex.trim()).expect("parse HEAD oid");

    let patch_opts = LogPatchOptions {
        diff: DiffOptions::default(),
        abbrev_len: 7,
        commit_header: LogPatchCommitHeader::FormatPercentH,
    };
    let mut out = Cursor::new(Vec::new());
    repo.odb()
        .with_pack_read_context(|| write_commit_log_patch(&repo, &mut out, &head_oid, &patch_opts))
        .expect("grit patch");
    let grit_index = index_line_from_patch(&out.into_inner()).expect("grit index line");

    assert_eq!(
        grit_index,
        git_index,
        "index abbrev mismatch for colliding blob pair {} vs {}",
        old_oid.to_hex(),
        new_oid.to_hex()
    );
    assert!(
        !git_index.contains("..9419d65.."),
        "ambiguous truncated prefix must not appear: {git_index}"
    );
}
