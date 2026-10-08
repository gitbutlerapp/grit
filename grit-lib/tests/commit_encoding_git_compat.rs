//! Commits with non-UTF-8 encodings round-trip with system `git log` / `git fsck`.

use std::process::Command;

use grit_lib::commit_encoding;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use grit_test_support::git;

fn ident() -> String {
    "Enc Test <enc@example.com> 1700000000 +0000".to_owned()
}

fn git_fsck(root: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(root)
        .args(["fsck"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_log_body_utf8(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(["log", "-1", "--encoding=UTF-8", "--format=%B"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git log");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit_write_encoded_commit(
    repo: &Repository,
    encoding: &str,
    message_unicode: &str,
    parent: Option<ObjectId>,
    tree: ObjectId,
) -> ObjectId {
    let (message, enc_label, raw_body) =
        commit_encoding::finalize_stored_commit_message(message_unicode.to_owned(), Some(encoding));
    assert!(
        enc_label.is_some(),
        "encoding {encoding} should produce header"
    );
    assert!(
        raw_body.is_some(),
        "encoding {encoding} should produce raw body"
    );
    let id = ident();
    let data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: id.clone(),
        committer: id,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: enc_label,
        message,
        raw_message: raw_body,
        extra_headers: Vec::new(),
    };
    repo.odb
        .write(ObjectKind::Commit, &serialize_commit(&data))
        .expect("write commit")
}

fn init_repo_with_empty_commit(root: &std::path::Path) -> (Repository, ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(
        root,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "base",
            "--author",
            "Enc Test <enc@example.com>",
        ],
    );
    let repo = Repository::discover(Some(root)).expect("open grit repo");
    let parent = match resolve_head(&repo.git_dir).expect("head") {
        grit_lib::state::HeadState::Branch { oid: Some(oid), .. } => oid,
        grit_lib::state::HeadState::Detached { oid } => oid,
        _ => panic!("expected base commit"),
    };
    let tree = parse_commit(&repo.odb.read(&parent).expect("read parent").data)
        .expect("parse parent")
        .tree;
    (repo, parent, tree)
}

#[test]
fn iso8859_1_grit_commit_validated_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (repo, parent, tree) = init_repo_with_empty_commit(root);
    let oid = grit_write_encoded_commit(&repo, "ISO-8859-1", "caf\u{00e9}\n", Some(parent), tree);
    git(root, &["update-ref", "HEAD", &oid.to_hex()]);
    git_fsck(root);
    let body = git_log_body_utf8(root);
    assert!(body.contains('é'), "log body: {body}");
}

#[test]
fn shift_jis_grit_commit_validated_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (repo, parent, tree) = init_repo_with_empty_commit(root);
    let oid = grit_write_encoded_commit(&repo, "Shift_JIS", "日本語\n", Some(parent), tree);
    git(root, &["update-ref", "HEAD", &oid.to_hex()]);
    git_fsck(root);
    let body = git_log_body_utf8(root);
    assert!(body.contains('語'), "log body: {body}");
}

#[test]
fn euc_jp_grit_commit_validated_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (repo, parent, tree) = init_repo_with_empty_commit(root);
    let oid = grit_write_encoded_commit(&repo, "EUC-JP", "日本語\n", Some(parent), tree);
    git(root, &["update-ref", "HEAD", &oid.to_hex()]);
    git_fsck(root);
    let body = git_log_body_utf8(root);
    assert!(body.contains('語'), "log body: {body}");
}

#[test]
fn utf16_grit_commit_validated_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (repo, parent, tree) = init_repo_with_empty_commit(root);
    let oid = grit_write_encoded_commit(&repo, "UTF-16", "wide\n", Some(parent), tree);
    git(root, &["update-ref", "HEAD", &oid.to_hex()]);
    git_fsck(root);
    assert_eq!(git_log_body_utf8(root).trim(), "wide");
}

#[test]
fn git_encoded_commit_readable_by_grit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "i18n.commitEncoding", "ISO-8859-1"]);
    std::fs::write(root.join("f"), b"x").unwrap();
    git(root, &["add", "f"]);
    git(
        root,
        &["commit", "-m", "caf\u{00e9}", "--author", "T <t@e.com>"],
    );
    let repo = Repository::discover(Some(root)).expect("open");
    let head = match resolve_head(&repo.git_dir).expect("head") {
        grit_lib::state::HeadState::Branch { oid: Some(oid), .. } => oid,
        grit_lib::state::HeadState::Detached { oid } => oid,
        _ => panic!("expected commit at HEAD"),
    };
    let obj = repo.odb.read(&head).expect("read");
    let commit = parse_commit(&obj.data).expect("parse");
    assert!(commit.message.contains('é') || commit.encoding.is_some());
}
