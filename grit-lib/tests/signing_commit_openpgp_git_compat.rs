//! OpenPGP commit signing without `user.signingkey` (committer default key).

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::signing::{verify_commit, GpgConfig};
use grit_test_support::git;

fn gpg_available() -> bool {
    Command::new("gpg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

fn git_out(dir: &Path, gnupg_home: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GNUPGHOME", gnupg_home)
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

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: test-local env override; restored on drop.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[test]
fn create_commit_openpgp_uses_committer_default_without_signingkey() {
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
    git(root, &["config", "commit.gpgsign", "true"]);
    git(root, &["config", "gpg.format", "openpgp"]);

    std::fs::write(root.join("a"), b"a\n").expect("write");
    let repo = Repository::discover(Some(root)).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");

    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    let gnupg_path = gnupg_home.to_string_lossy().into_owned();
    let _gnupg = EnvVarGuard::set("GNUPGHOME", &gnupg_path);
    create_commit(
        &repo,
        &CommitRequest {
            message: "openpgp default key".to_owned(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
        },
        &mut NullProgress,
    )
    .expect("create_commit");

    let head_hex = git_out(root, gnupg_home, &["rev-parse", "HEAD"]);
    let raw = repo
        .odb
        .read(&head_hex.parse::<ObjectId>().expect("oid"))
        .expect("read")
        .data;
    let config = repo.config().expect("config");
    assert!(
        config.as_ref().get("user.signingkey").is_none(),
        "test must not set user.signingkey"
    );
    let gpg_cfg = GpgConfig::from_config(config.as_ref())
        .expect("gpg config")
        .with_command_runner(repo.command_runner());
    let check = verify_commit(&gpg_cfg, &raw).expect("verify_commit");
    assert!(
        check.is_good(),
        "OpenPGP signature should verify (grit verify_commit)"
    );

    git_out(root, gnupg_home, &["fsck", "--strict"]);
}
