//! Annotated tag extra headers round-trip with the system `git` binary.

use std::io::Write;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
use grit_lib::objects::{
    extra_header_values, parse_tag, serialize_tag, set_extra_header, ObjectKind, TagData,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_mktag(repo: &std::path::Path, body: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["-c", "fsck.extraHeaderEntry=ignore", "mktag"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .spawn()
        .expect("spawn git mktag");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body.as_bytes())
        .expect("write mktag body");
    let out = child.wait_with_output().expect("wait mktag");
    assert!(
        out.status.success(),
        "git mktag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_cat_tag(repo: &std::path::Path, oid: &str) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-p", oid])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("cat-file");
    assert!(
        out.status.success(),
        "cat-file tag: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn tag_post_tagger_duplicate_object_header_roundtrips_byte_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let null = "0000000000000000000000000000000000000000";
    let body = format!(
        "object {commit}\ntype commit\ntag dup-object-extra\ntagger T <t@example.com> 1 +0000\n\
object {null}\n extra continuation\n\nmessage\n"
    );
    let oid_hex = git_mktag(dir.path(), &body);
    let raw = git_cat_tag(dir.path(), &oid_hex);
    assert_eq!(raw, body.as_bytes());

    let parsed = parse_tag(&raw).expect("parse");
    assert_eq!(parsed.object.to_hex(), commit);
    assert!(
        parsed.extra_headers.windows(7).any(|w| w == b"object "),
        "post-tagger object line must stay in extra_headers"
    );
    assert!(
        String::from_utf8_lossy(&parsed.extra_headers).contains(" extra continuation"),
        "continuation line must stay in extra_headers: {:?}",
        String::from_utf8_lossy(&parsed.extra_headers)
    );
    assert_eq!(serialize_tag(&parsed), raw);
}

#[test]
fn tag_with_unknown_headers_roundtrips_byte_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let body = format!(
        "object {commit}\ntype commit\ntag extra-hdr\ntagger T <t@example.com> 1 +0000\n\
tree-sha256 {digest}\ncustom-header value one\n\nannotated with extra headers\n"
    );
    let oid_hex = git_mktag(dir.path(), &body);
    let raw = git_cat_tag(dir.path(), &oid_hex);
    assert_eq!(
        raw,
        body.as_bytes(),
        "git cat-file should match written bytes"
    );

    let parsed = parse_tag(&raw).expect("parse git tag");
    assert_eq!(
        extra_header_values(&parsed.extra_headers, "tree-sha256"),
        vec![digest.as_bytes()]
    );
    let round = serialize_tag(&parsed);
    assert_eq!(round, raw);
    let mut hash = Command::new("git")
        .current_dir(dir.path())
        .args(["hash-object", "-t", "tag", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .spawn()
        .expect("hash-object");
    hash.stdin
        .take()
        .expect("stdin")
        .write_all(&round)
        .expect("write");
    let hash_out = hash.wait_with_output().expect("wait hash-object");
    assert!(hash_out.status.success());
    let round_oid = String::from_utf8_lossy(&hash_out.stdout).trim().to_string();
    assert_eq!(round_oid, oid_hex);
}

#[test]
fn grit_written_tag_extra_headers_pass_git_fsck_strict() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "base"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let commit = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");

    let mut tag = TagData {
        object: commit,
        object_type: "commit".into(),
        tag: "with-extra".into(),
        tagger: Some("T <t@example.com> 1 +0000".into()),
        extra_headers: Vec::new(),
        message: "tag body\n".into(),
    };
    set_extra_header(
        &mut tag.extra_headers,
        "tree-sha256",
        b"tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
    );
    let bytes = serialize_tag(&tag);
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &bytes)
        .expect("write tag");
    git(
        dir.path(),
        &["update-ref", "refs/tags/with-extra", &tag_oid.to_hex()],
    );
    git(dir.path(), &["fsck", "--strict"]);
}

#[test]
fn grit_fsck_ignores_tag_extra_headers() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "base"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let commit = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");

    let tag = TagData {
        object: commit,
        object_type: "commit".into(),
        tag: "fsck-extra".into(),
        tagger: Some("T <t@example.com> 1 +0000".into()),
        extra_headers:
            b"tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n"
                .to_vec(),
        message: "body\n".into(),
    };
    let bytes = serialize_tag(&tag);
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &bytes)
        .expect("write tag");
    git(
        dir.path(),
        &["update-ref", "refs/tags/fsck-extra", &tag_oid.to_hex()],
    );

    let opts = FsckOptions::full_repository(false).with_check_dangling(false);
    let report = fsck(&repo, &opts, &mut NullProgress, &NeverCancel).expect("grit fsck");
    let config = repo.config().expect("config");
    assert!(
        report.ok(&config, &opts),
        "expected fsck ok; issues: {:?}",
        report.issues
    );
}
