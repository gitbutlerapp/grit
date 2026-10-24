//! Change-id commit headers interoperate with the system `git` binary.

use std::path::Path;
use std::process::Command;

use grit_lib::change_id::{set_change_id, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::commit::prepare_commit_bytes;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed (status {:?}): stdout={} stderr={}",
        args,
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

#[test]
fn change_id_header_passes_git_and_grit_fsck() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "base",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let head = repo
        .odb()
        .read(&{
            let hex = git_out(dir.path(), &["rev-parse", "HEAD"]);
            hex.parse().expect("head oid")
        })
        .expect("read head");
    let tree = parse_commit(&head.data).expect("parse head").tree;
    let mut data = CommitData {
        tree,
        parents: vec![],
        author: "T <t@example.com> 1 +0000".into(),
        committer: "T <t@example.com> 1 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "with change id\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    set_change_id(
        &mut data,
        &ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("id"),
    );
    let bytes = serialize_commit(&data);
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("write commit");
    git_out(dir.path(), &["cat-file", "-e", &oid.to_hex()]);
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &oid.to_hex()],
    );

    assert_eq!(git_out(dir.path(), &["rev-parse", "HEAD"]), oid.to_hex());
    let head_raw = git_out(dir.path(), &["cat-file", "-p", "HEAD"]);
    assert!(
        head_raw.contains("change-id"),
        "HEAD commit should include change-id: {head_raw}"
    );
    git_out(dir.path(), &["fsck", "--strict"]);
    let cat = git_out(dir.path(), &["cat-file", "-p", "HEAD"]);
    assert!(
        cat.lines().any(|l| l.starts_with("change-id ")),
        "git cat-file should show change-id header"
    );

    let opts = FsckOptions::full_repository(true).with_check_dangling(false);
    let report = fsck(&repo, &opts, &mut NullProgress, &NeverCancel).expect("grit fsck");
    let config = repo.config().expect("config");
    assert!(report.ok(&config, &opts), "{:?}", report.issues);
    let parsed = parse_commit(&repo.odb().read(&oid).expect("read").data).expect("parse");
    assert!(grit_lib::change_id::change_id(&parsed).is_some());
}

#[test]
fn signed_commit_keeps_change_id_before_gpgsig() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = root.join("key");
    let allowed = root.join("allowed_signers");

    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");

    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "gpg.format", "ssh"]);
    git(
        root,
        &[
            "config",
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ],
    );
    git(root, &["config", "commit.gpgsign", "true"]);
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("read pub");
    std::fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed_signers");
    git(
        root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let tree = repo.odb().write(ObjectKind::Tree, b"").expect("empty tree");
    let mut data = CommitData {
        tree,
        parents: vec![],
        author: "T <t@example.com> 1700000000 +0000".into(),
        committer: "T <t@example.com> 1700000000 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "signed change id\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    set_change_id(&mut data, &ChangeId::from_bytes(&[0x11; 16]));
    let config = repo.config().expect("config");
    let signed = prepare_commit_bytes(&repo, config.as_ref(), &data, None).expect("sign");
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &signed)
        .expect("write");
    git(root, &["update-ref", "refs/heads/main", &oid.to_hex()]);

    assert_eq!(git_out(root, &["log", "-1", "--format=%G?"]), "G");
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    let change_pos = cat.find("change-id ").expect("change-id header");
    let sig_pos = cat.find("gpgsig ").expect("gpgsig header");
    assert!(change_pos < sig_pos, "change-id must precede gpgsig");
}
