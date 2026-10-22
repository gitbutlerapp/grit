//! SSH and OpenPGP tag signing round-trip with the system `git` binary.

use std::path::Path;
use std::process::Command;

use grit_lib::commit::signing::{verify_tag, GpgConfig};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::tag::{create_tag, CreateTagRequest};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

fn gpg_available() -> bool {
    Command::new("gpg").arg("--version").output().is_ok()
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

fn setup_ssh_signing_repo(root: &Path) -> std::path::PathBuf {
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
    git(root, &["add", "a"]);
    git(root, &["commit", "-m", "base"]);

    key
}

#[test]
fn create_tag_ssh_signed_verifies_with_git_verify_tag() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let tagger = "T <t@example.com> 1700000000 +0000".to_owned();

    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".to_owned(),
            target: head,
            message: Some("signed release".to_owned()),
            tagger: Some(tagger),
            sign: Some(true),
            signing_key: None,
            force: false,
            dry_run: false,
            tree_sha256: None,
        },
    )
    .expect("create signed tag");
    assert!(outcome.signed);

    git_out(root, &["verify-tag", "v1"]);
    git_out(root, &["tag", "-v", "v1"]);

    let raw = repo.odb().read(&outcome.oid).expect("read tag").data;
    let gpg = GpgConfig::from_config(repo.config().expect("config").as_ref())
        .expect("gpg config")
        .with_command_runner(repo.command_runner());
    let check = verify_tag(&gpg, &raw).expect("verify_tag");
    assert!(
        check.is_good(),
        "grit verify_tag should accept the signature"
    );
}

#[test]
fn create_tag_honors_tag_gpgsign_config() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);
    git(root, &["config", "tag.gpgSign", "true"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();

    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "v2".to_owned(),
            target: head,
            message: None,
            tagger: Some("T <t@example.com> 1700000000 +0000".to_owned()),
            sign: None,
            signing_key: None,
            force: false,
            dry_run: false,
            tree_sha256: None,
        },
    )
    .expect("create tag with tag.gpgSign");
    assert!(outcome.signed);
    git_out(root, &["verify-tag", "v2"]);
}

#[test]
fn signed_tag_passes_git_fsck_strict() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    create_tag(
        &repo,
        &CreateTagRequest {
            name: "rel".to_owned(),
            target: head,
            message: Some("x".to_owned()),
            tagger: Some("T <t@example.com> 1700000000 +0000".to_owned()),
            sign: Some(true),
            signing_key: None,
            force: false,
            dry_run: false,
            tree_sha256: None,
        },
    )
    .expect("signed tag");

    git_out(root, &["fsck", "--strict"]);
}

#[test]
fn grit_verify_tag_accepts_git_signed_tag() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);
    git(root, &["config", "tag.gpgSign", "true"]);
    git_out(root, &["tag", "-s", "git-v1", "-m", "from git"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let tag_oid: ObjectId = git_out(root, &["rev-parse", "git-v1"])
        .parse()
        .expect("parse oid");
    let raw = repo.odb().read(&tag_oid).expect("read").data;
    let gpg = GpgConfig::from_config(repo.config().expect("config").as_ref())
        .expect("gpg config")
        .with_command_runner(repo.command_runner());
    let check = verify_tag(&gpg, &raw).expect("verify_tag");
    assert!(
        check.is_good(),
        "grit should verify a tag signed by git: {:?}",
        check.output
    );
}

struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: test-only single-threaded mutation.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn init_ephemeral_signing_key(gnupg_home: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(gnupg_home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(gnupg_home, std::fs::Permissions::from_mode(0o700))?;
    }
    let spec = gnupg_home.join("keyspec");
    std::fs::write(
        &spec,
        "%no-protection
Key-Type: EDDSA
Key-Curve: Ed25519
Key-Usage: sign
Name-Real: T
Name-Email: t@example.com
Expire-Date: 0
",
    )?;
    let status = Command::new("gpg")
        .args(["--batch", "--generate-key", spec.to_str().unwrap()])
        .env("GNUPGHOME", gnupg_home)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("gpg --generate-key failed"));
    }
    Ok(())
}

#[test]
fn create_tag_openpgp_signed_verifies_with_git() {
    if !gpg_available() {
        eprintln!("skipping: gpg not available");
        return;
    }

    let _git_config = EnvVarGuard::set("GIT_CONFIG_GLOBAL", "/dev/null");
    let _git_nosystem = EnvVarGuard::set("GIT_CONFIG_NOSYSTEM", "1");

    let repo_dir = tempfile::tempdir().expect("repo");
    let gnupg_dir = tempfile::tempdir().expect("gnupg");
    let root = repo_dir.path();
    let gnupg_home = gnupg_dir.path();
    if init_ephemeral_signing_key(gnupg_home).is_err() {
        eprintln!("skipping: could not generate ephemeral OpenPGP key");
        return;
    }

    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "gpg.format", "openpgp"]);
    git(root, &["config", "tag.gpgSign", "true"]);

    std::fs::write(root.join("a"), b"a\n").expect("write");
    git(root, &["add", "a"]);
    git(root, &["commit", "-m", "base"]);

    let gnupg_path = gnupg_home.to_string_lossy().into_owned();
    let _gnupg = EnvVarGuard::set("GNUPGHOME", &gnupg_path);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "openpgp-v1".to_owned(),
            target: head,
            message: Some("openpgp tag".to_owned()),
            tagger: Some("T <t@example.com> 1700000000 +0000".to_owned()),
            sign: None,
            signing_key: None,
            force: false,
            dry_run: false,
            tree_sha256: None,
        },
    )
    .expect("signed openpgp tag");
    assert!(outcome.signed);

    let verify = Command::new("git")
        .current_dir(root)
        .args(["verify-tag", "openpgp-v1"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GNUPGHOME", gnupg_home)
        .output()
        .expect("git verify-tag");
    assert!(
        verify.status.success(),
        "git verify-tag failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
}
