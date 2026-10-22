//! tree-sha256 tag claim headers interoperate with the system `git` binary.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_tag, serialize_tag, ObjectKind, TagData};
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::{TreeSha256, TreeSha256Claim};
use grit_test_support::git;
use std::str::FromStr;

#[test]
fn claim_headers_survive_git_cat_file_and_fsck_strict() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");

    let claim = TreeSha256Claim {
        digest: TreeSha256::from_str(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .expect("digest"),
        excluded: vec!["vendor/mod".into(), "alpha/mod".into()],
    };
    let mut tag = TagData {
        object: commit,
        object_type: "commit".into(),
        tag: "tree-sha256-claim".into(),
        tagger: Some("T <t@example.com> 1 +0000".into()),
        extra_headers: Vec::new(),
        message: "annotated\n".into(),
    };
    claim.apply_to_tag(&mut tag);
    let bytes = serialize_tag(&tag);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let tag_oid = repo.odb().write(ObjectKind::Tag, &bytes).expect("write");
    git(
        dir.path(),
        &[
            "update-ref",
            "refs/tags/tree-sha256-claim",
            &tag_oid.to_hex(),
        ],
    );

    let cat = Command::new("git")
        .current_dir(dir.path())
        .args(["cat-file", "-p", &tag_oid.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("cat-file");
    assert!(
        cat.status.success(),
        "git cat-file: {}",
        String::from_utf8_lossy(&cat.stderr)
    );
    assert_eq!(cat.stdout, bytes);

    let parsed = parse_tag(&cat.stdout).expect("parse git output");
    let round = TreeSha256Claim::from_tag(&parsed)
        .expect("from_tag")
        .expect("claim present");
    assert_eq!(round.digest, claim.digest);
    let mut got = round.excluded;
    let mut want = claim.excluded;
    got.sort();
    want.sort();
    assert_eq!(got, want);

    git(dir.path(), &["fsck", "--strict"]);
}
