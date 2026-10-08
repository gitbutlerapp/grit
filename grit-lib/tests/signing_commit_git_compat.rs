//! SSH commit signing round-trip with the system `git` binary.

use std::path::Path;
use std::process::Command;

use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn create_commit_ssh_signs_when_gpgsign_true() {
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
    git(root, &["config", "core.logAllRefUpdates", "true"]);
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

    std::fs::write(root.join("a"), b"a\n").expect("write");
    let repo = Repository::discover(Some(root)).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");

    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    create_commit(
        &repo,
        &CommitRequest {
            message: "grit signed".to_owned(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
        },
        &mut NullProgress,
    )
    .expect("create_commit");

    let sig_status = git_out(root, &["log", "-1", "--format=%G?"]);
    assert_eq!(
        sig_status, "G",
        "git should report a good signature (%G? = G)"
    );

    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    assert!(
        cat.contains("gpgsig "),
        "commit object should carry a gpgsig header"
    );

    git_out(root, &["fsck", "--strict"]);
}
