//! `grit tag --sign` JSON reports `signed: true`.

use std::process::Command;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

fn git(root: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn tag_sign_json_reports_signed_true() {
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
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("pub");
    std::fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed");
    git(
        root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );

    std::fs::write(root.join("f"), b"x\n").expect("write");
    git(root, &["add", "f"]);
    git(root, &["commit", "-m", "init"]);

    let out = Command::new(GS)
        .current_dir(root)
        .args(["tag", "v1", "--sign", "-m", "x", "--json"])
        .output()
        .expect("grit tag");
    assert!(
        out.status.success(),
        "grit tag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("json from grit tag --json");
    assert_eq!(v["action"], "create");
    assert_eq!(v["signed"], true);
    assert_eq!(v["kind"], "annotated");
}
