//! Commits and annotated tags whose header block ends at EOF (no blank line) parse like Git.

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::fsck::{fsck_object, FsckObjectOptions};
use grit_lib::maintenance::repository_fsck::{self as repo_fsck, FsckOptions};
use grit_lib::objects::{parse_commit, parse_tag, HashAlgo, ObjectId, ObjectKind};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;
use grit_test_support::objects::{HashAlgo as FixtureAlgo, RepoFixture};
use grit_test_support::{git_fsck, git_hash_object_literally};

fn git_mktag(repo: &std::path::Path, body: &str) -> ObjectId {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["mktag"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git mktag");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .expect("stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git mktag: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("tag oid")
}

/// Reproduction from gitbutlerapp/grit#2320.
#[test]
fn headers_only_commit_and_tag_match_git() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let root = fixture.path();
    let tree = fixture
        .git(&["hash-object", "-t", "tree", "-w", "/dev/null"])
        .stdout
        .trim()
        .to_string();

    let commit_body = format!(
        "tree {tree}\nauthor A <a@b> 1700000000 +0000\ncommitter A <a@b> 1700000000 +0000\n"
    );
    let commit_oid_hex =
        git_hash_object_literally(root, FixtureAlgo::Sha1, "commit", commit_body.as_bytes())
            .expect("literally commit");
    let commit_oid: ObjectId = commit_oid_hex.parse().expect("hex");

    let child_oid: ObjectId = fixture
        .git(&["commit-tree", &tree, "-p", &commit_oid_hex, "-m", "child"])
        .stdout
        .trim()
        .parse()
        .expect("child");
    assert!(
        fixture
            .git(&["update-ref", "refs/heads/main", &child_oid.to_hex()])
            .ok
    );

    let tag_body =
        format!("object {child_oid}\ntype commit\ntag nomsg\ntagger A <a@b> 1700000000 +0000\n");
    let tag_oid = git_mktag(root, &tag_body);
    assert!(
        fixture
            .git(&["update-ref", "refs/tags/nomsg", &tag_oid.to_hex()])
            .ok
    );

    let git_outcome = git_fsck(root, true);
    assert!(
        git_outcome.ok,
        "git fsck --strict: {:?}",
        git_outcome.msg_ids
    );

    let commit_raw = fixture.git(&["cat-file", "-p", &commit_oid_hex]).stdout;
    let parsed_commit = parse_commit(commit_raw.as_bytes()).expect("parse commit");
    assert_eq!(parsed_commit.message, "");
    assert_eq!(parsed_commit.parents.len(), 0);

    let tag_raw = fixture.git(&["cat-file", "-p", &tag_oid.to_hex()]).stdout;
    let parsed_tag = parse_tag(tag_raw.as_bytes()).expect("parse tag");
    assert_eq!(parsed_tag.message, "");
    assert_eq!(parsed_tag.tag, "nomsg");

    let repo =
        Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover grit");
    let from_odb = repo.odb().read_commit(&commit_oid).expect("read_commit");
    assert_eq!(from_odb.message, "");

    let oids = repo
        .rev_walk()
        .commits(&["main"], &RevWalkListOptions::default())
        .expect("rev walk");
    assert_eq!(oids.len(), 2);
    assert!(oids.contains(&commit_oid));

    let tag_obj = repo.odb().read(&tag_oid).expect("read tag object");
    assert_eq!(tag_obj.kind, ObjectKind::Tag);
    let tag_via_odb = parse_tag(&tag_obj.data).expect("parse tag from odb");
    assert_eq!(tag_via_odb.message, "");
}

#[test]
fn commit_headers_without_trailing_newline_parses_fsck_unterminated_header() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let body =
        format!("tree {tree}\nauthor A <a@b> 1700000000 +0000\ncommitter A <a@b> 1700000000 +0000");
    assert!(parse_commit(body.as_bytes()).is_ok());
    let err = fsck_object(
        ObjectKind::Commit,
        body.as_bytes(),
        FsckObjectOptions::new(HashAlgo::Sha1),
    )
    .expect_err("fsck");
    assert_eq!(err.id, "unterminatedHeader");
}

#[test]
fn repository_fsck_reports_nul_in_commit_message() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let tree = fixture
        .git(&["hash-object", "-t", "tree", "-w", "/dev/null"])
        .stdout
        .trim()
        .to_string();
    let mut body = format!(
        "tree {tree}\nauthor A <a@b> 1700000000 +0000\ncommitter A <a@b> 1700000000 +0000\n\n"
    )
    .into_bytes();
    body.push(0);
    let oid_hex =
        git_hash_object_literally(fixture.path(), FixtureAlgo::Sha1, "commit", &body).expect("oid");
    assert!(
        fixture
            .git(&["update-ref", "refs/heads/probe", &oid_hex])
            .ok
    );

    let repo =
        Repository::discover(Some(fixture.path()), &RepositoryOptions::empty()).expect("discover");
    let report = repo_fsck::fsck(
        &repo,
        &FsckOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("fsck");
    assert!(
        report
            .issues
            .iter()
            .any(|i| repo_fsck::issue_msg_id(i) == "nulInCommit"),
        "expected nulInCommit in {:?}",
        report.issues
    );
}
