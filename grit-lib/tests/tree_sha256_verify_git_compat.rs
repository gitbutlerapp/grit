//! tree-sha256 tag verification with system `git` and SSH signing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use grit_lib::command_runner::{CommandRunner, CommandSpec, RecordedResponse, RecordingRunner};
use grit_lib::commit::signing::GpgConfig;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{serialize_tag, set_extra_header, ObjectKind, TagData};
use grit_lib::porcelain::tag::{verify_tag_by_name, TagError};
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::{
    parse_tree_sha256_tag_headers, verify_tag, SignatureVerdict, TagVerifyOptions, TreeSha256,
    TREE_SHA256_EXCLUDED_HEADER, TREE_SHA256_HEADER,
};
use grit_test_support::git;

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

fn git_mktag(repo: &Path, body: &str) -> String {
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

fn setup_ssh_signing(root: &Path) -> std::path::PathBuf {
    let key = root.join("key");
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
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("read pub");
    let allowed = root.join("allowed_signers");
    std::fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed_signers");
    git(
        root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );
    key
}

fn gpg_config(repo: &Repository) -> GpgConfig {
    GpgConfig::from_config(repo.config().expect("config").as_ref())
        .expect("gpg config")
        .with_command_runner(repo.command_runner())
}

fn sign_payload_ssh(key: &Path, payload: &[u8]) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let payload_path = dir.path().join("payload.txt");
    std::fs::write(&payload_path, payload).expect("write payload");
    let status = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-n", "git", "-f"])
        .arg(key)
        .arg(&payload_path)
        .status()
        .expect("ssh-keygen sign");
    assert!(status.success(), "ssh-keygen sign failed");
    let sig = std::fs::read(payload_path.with_extension("txt.sig")).expect("read sig");
    let mut out = payload.to_vec();
    out.extend_from_slice(&sig);
    out
}

fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn copy_worktree_from(source: &Path, dest: &Path) {
    for entry in fs::read_dir(source).expect("read_dir") {
        let entry = entry.expect("entry");
        if entry.file_name() == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to).expect("copy dir");
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

fn install_submodule_checkout(super_dir: &Path, name: &str, source_repo: &Path, gitlink: &str) {
    let wt = super_dir.join(name);
    fs::create_dir_all(&wt).expect("mkdir submodule wt");
    copy_worktree_from(source_repo, &wt);
    let modules = super_dir.join(".git/modules").join(name);
    if modules.exists() {
        fs::remove_dir_all(&modules).ok();
    }
    copy_dir_all(&source_repo.join(".git"), &modules).expect("copy module git");
    grit_lib::config::gitmodules::gitdir::write_submodule_gitfile(&wt, &modules).expect("gitfile");
    grit_lib::config::gitmodules::gitdir::init_submodule_head_from_gitlink(&modules, gitlink)
        .expect("module head");
}

fn tree_sha256_digest(repo: &Repository, commit: grit_lib::objects::ObjectId) -> TreeSha256 {
    use grit_lib::tree_sha256::TreeSha256Options;
    repo.tree_sha256(commit, &TreeSha256Options::default())
        .expect("tree_sha256")
        .digest
}

#[test]
fn git_made_tag_with_header_verifies() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = setup_ssh_signing(root);
    std::fs::write(root.join("a"), b"a\n").expect("write");
    git(root, &["add", "a"]);
    git(root, &["commit", "-m", "base"]);
    let commit = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let digest = tree_sha256_digest(&repo, commit.parse().unwrap());

    let body = format!(
        "object {commit}\ntype commit\ntag signed-hdr\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_HEADER} {digest}\n\nrelease\n"
    );
    let tag_oid_hex = git_mktag(root, &body);
    let raw_unsigned = Command::new("git")
        .current_dir(root)
        .args(["cat-file", "-p", &tag_oid_hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("cat-file");
    assert!(raw_unsigned.status.success());
    let signed = sign_payload_ssh(&key, &raw_unsigned.stdout);
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &signed)
        .expect("write tag");
    git(
        root,
        &["update-ref", "refs/tags/signed-hdr", &tag_oid.to_hex()],
    );
    Command::new("git")
        .current_dir(root)
        .args(["verify-tag", "signed-hdr"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git verify-tag status");

    let opts = TagVerifyOptions {
        check_signature: true,
        gpg: gpg_config(&repo),
    };
    let report = repo.verify_tag(tag_oid, &opts).expect("verify");
    assert!(report.is_ok(), "{report:?}");
    assert!(matches!(report.signature, SignatureVerdict::Good { .. }));
}

#[test]
fn reference_tool_tag_fixture_verifies() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tree_sha256_tag");
    let tag_bytes = std::fs::read(fixture.join("tag-object.txt")).expect("tag bytes");
    let dot_git = fixture.join("dot-git");
    let work = tempfile::tempdir().expect("tempdir");
    std::process::Command::new("cp")
        .args([
            "-a",
            dot_git.to_str().unwrap(),
            work.path().join(".git").to_str().unwrap(),
        ])
        .status()
        .expect("cp fixture git dir");
    let allowed = fixture.join("allowed_signers");
    git(
        work.path(),
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );
    git(work.path(), &["config", "gpg.format", "ssh"]);
    let repo = Repository::discover(Some(work.path()), &RepositoryOptions::empty()).expect("open");
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &tag_bytes)
        .expect("install tag");
    let opts = TagVerifyOptions {
        check_signature: true,
        gpg: gpg_config(&repo),
    };
    let report = repo.verify_tag(tag_oid, &opts).expect("verify fixture");
    assert!(report.is_ok(), "{report:?}");
}

#[test]
fn edited_header_fails_signature() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = setup_ssh_signing(root);
    git(root, &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let digest = tree_sha256_digest(&repo, commit.parse().unwrap());
    let body = format!(
        "object {commit}\ntype commit\ntag t\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_HEADER} {digest}\n\nmsg\n"
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let mut tampered = signed;
    if let Some(pos) = tampered.windows(digest.to_hex().len()).position(|w| {
        std::str::from_utf8(w)
            .ok()
            .and_then(|s| s.parse::<TreeSha256>().ok())
            == Some(digest)
    }) {
        tampered[pos] = b'0';
    }
    let tag_oid = repo.odb().write(ObjectKind::Tag, &tampered).expect("write");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify");
    assert!(!report.is_ok());
    assert!(matches!(report.signature, SignatureVerdict::Bad));
}

#[test]
fn wrong_claim_fails_digest() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = setup_ssh_signing(root);
    git(root, &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let digest = tree_sha256_digest(&repo, commit.parse().unwrap());
    let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
    let body = format!(
        "object {commit}\ntype commit\ntag t\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_HEADER} {wrong}\n\nmsg\n"
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let tag_oid = repo.odb().write(ObjectKind::Tag, &signed).expect("write");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify");
    assert!(matches!(report.signature, SignatureVerdict::Good { .. }));
    assert!(!report.is_ok());
    let v = report.tree_sha256.expect("verdict");
    assert_ne!(v.claimed, v.computed);
    assert_eq!(v.computed, digest);
}

#[test]
fn bogus_excluded_path_fails() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = setup_ssh_signing(root);
    git(root, &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let digest = tree_sha256_digest(&repo, commit.parse().unwrap());
    let body = format!(
        "object {commit}\ntype commit\ntag t\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_EXCLUDED_HEADER} not-a-submodule\n\
{TREE_SHA256_HEADER} {digest}\n\nmsg\n"
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let tag_oid = repo.odb().write(ObjectKind::Tag, &signed).expect("write");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify");
    assert!(!report.is_ok());
    assert_eq!(
        report.tree_sha256.as_ref().unwrap().bogus_excluded,
        vec!["not-a-submodule".to_owned()]
    );
}

#[test]
fn changed_blob_detected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    std::fs::write(tmp.path().join("f"), b"one\n").expect("write");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-m", "one"]);
    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    let commit1 = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let digest1 = tree_sha256_digest(&repo, commit1);
    std::fs::write(tmp.path().join("f"), b"two\n").expect("change");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-m", "two"]);
    let commit2 = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let mut tag = TagData {
        object: commit2,
        object_type: "commit".into(),
        tag: "stale-digest".into(),
        tagger: Some("T <t@example.com> 1 +0000".into()),
        extra_headers: Vec::new(),
        message: "body\n".into(),
    };
    set_extra_header(
        &mut tag.extra_headers,
        TREE_SHA256_HEADER,
        format!("{TREE_SHA256_HEADER} {digest1}\n").as_bytes(),
    );
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &serialize_tag(&tag))
        .expect("write");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: false,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify");
    assert!(!report.is_ok());
    let v = report.tree_sha256.expect("verdict");
    assert_ne!(v.claimed, v.computed);
    assert_eq!(v.computed, tree_sha256_digest(&repo, commit2));
}

#[test]
fn unsigned_tag_with_header_reports_missing_signature() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    let digest = tree_sha256_digest(&repo, commit);
    let mut tag = TagData {
        object: commit,
        object_type: "commit".into(),
        tag: "unsigned".into(),
        tagger: Some("T <t@example.com> 1 +0000".into()),
        extra_headers: Vec::new(),
        message: "body\n".into(),
    };
    set_extra_header(
        &mut tag.extra_headers,
        TREE_SHA256_HEADER,
        format!("{TREE_SHA256_HEADER} {digest}\n").as_bytes(),
    );
    let bytes = serialize_tag(&tag);
    let tag_oid = repo.odb().write(ObjectKind::Tag, &bytes).expect("write");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify");
    assert!(matches!(report.signature, SignatureVerdict::Missing));
    assert!(!report.is_ok());
}

#[test]
fn signed_tag_without_header_verifies_signature_only() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = setup_ssh_signing(root);
    git(root, &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let body = format!(
        "object {commit}\ntype commit\ntag plain\ntagger T <t@example.com> 1 +0000\n\nmsg\n"
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let tag_oid = repo.odb().write(ObjectKind::Tag, &signed).expect("write");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify");
    assert!(report.is_ok());
    assert!(report.tree_sha256.is_none());
    assert!(matches!(report.signature, SignatureVerdict::Good { .. }));
}

#[test]
fn non_tag_object_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    let err = repo
        .verify_tag(
            commit,
            &TagVerifyOptions {
                check_signature: false,
                gpg: gpg_config(&repo),
            },
        )
        .unwrap_err();
    assert!(matches!(err, Error::TagVerifyNotTag { .. }));
}

#[test]
fn verify_tag_by_name_reads_git_written_loose_and_packed_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["commit", "--allow-empty", "-m", "base"]);
    git(tmp.path(), &["tag", "-a", "-m", "loose", "v-loose"]);
    git(tmp.path(), &["tag", "-a", "-m", "packed", "v-packed"]);
    git(tmp.path(), &["tag", "light"]);
    git(tmp.path(), &["pack-refs", "--all"]);
    git(tmp.path(), &["tag", "-a", "-m", "loose", "v-loose", "-f"]);
    let commit: grit_lib::objects::ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    let options = TagVerifyOptions {
        check_signature: false,
        gpg: gpg_config(&repo),
    };

    for name in ["v-loose", "v-packed"] {
        let tag_oid: grit_lib::objects::ObjectId =
            git(tmp.path(), &["rev-parse", &format!("refs/tags/{name}")])
                .trim()
                .parse()
                .unwrap();
        let verified = verify_tag_by_name(&repo, name, &options).expect("verify by name");
        assert_eq!(verified.name, name);
        assert_eq!(verified.oid, tag_oid);
        assert_eq!(verified.target, commit);
        assert_eq!(
            verified.verification.signature,
            SignatureVerdict::NotChecked
        );
        assert!(verified.verification.is_ok());
    }

    let err = verify_tag_by_name(&repo, "light", &options).unwrap_err();
    assert!(
        matches!(&err, Error::Tag(TagError::Lightweight { name }) if name == "light"),
        "{err:?}"
    );
}

#[derive(Debug)]
struct GateSigningRunner {
    inner: Arc<RecordingRunner>,
    gate: Arc<std::sync::atomic::AtomicBool>,
    hash_done: Arc<std::sync::atomic::AtomicBool>,
}

impl CommandRunner for GateSigningRunner {
    fn spawn(
        &self,
        spec: &CommandSpec,
    ) -> std::io::Result<grit_lib::command_runner::RunningCommand> {
        while !self.hash_done.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.gate.store(true, std::sync::atomic::Ordering::Release);
        self.inner.spawn(spec)
    }
}

#[test]
fn signature_and_hash_run_concurrently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["commit", "--allow-empty", "-m", "base"]);
    let commit = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    let digest = tree_sha256_digest(&repo, commit.parse().unwrap());
    let mut body = format!(
        "object {commit}\ntype commit\ntag t\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_HEADER} {digest}\n\nmsg\n"
    );
    body.push_str("-----BEGIN PGP SIGNATURE-----\nAAAA\n-----END PGP SIGNATURE-----\n");
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, body.as_bytes())
        .expect("write tag");

    git(tmp.path(), &["config", "gpg.format", "openpgp"]);
    let recording = RecordingRunner::always_success();
    recording.push_response(RecordedResponse::Output {
        stdout: b"[GNUPG:] NEWSIG\n[GNUPG:] VALIDSIG abc 123 456 0 0\n[GNUPG:] GOODSIG user User <u@e.com>\n[GNUPG:] TRUST_FULLY 0 pgp\n".to_vec(),
        code: 0,
    });
    let hash_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sign_gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate_runner = Arc::new(GateSigningRunner {
        inner: recording.clone(),
        gate: sign_gate.clone(),
        hash_done: hash_done.clone(),
    });
    let gpg = GpgConfig::from_config(repo.config().expect("config").as_ref())
        .expect("gpg")
        .with_command_runner(gate_runner);

    let handle = std::thread::spawn({
        let gpg = gpg.clone();
        move || {
            verify_tag(
                &repo,
                tag_oid,
                &TagVerifyOptions {
                    check_signature: true,
                    gpg,
                },
            )
        }
    });
    for _ in 0..30 {
        assert!(
            !sign_gate.load(std::sync::atomic::Ordering::Acquire),
            "signing subprocess started while hash gate was closed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    hash_done.store(true, std::sync::atomic::Ordering::Release);
    let report = handle.join().expect("join").expect("verify");
    assert!(report.tree_sha256.is_some());
    assert!(matches!(report.signature, SignatureVerdict::Good { .. }));
    assert!(!recording.specs().is_empty());
}

#[test]
fn verify_tag_nested_exclusion_path_from_root() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    use std::fs;
    let tmp = tempfile::tempdir().expect("tempdir");
    let nest = tmp.path().join("nest-src");
    fs::create_dir(&nest).unwrap();
    git(&nest, &["init", "-q", "-b", "main"]);
    fs::write(nest.join("n.txt"), b"n\n").unwrap();
    git(&nest, &["add", "n.txt"]);
    git(&nest, &["commit", "-m", "nest"]);
    let nest_head = git(&nest, &["rev-parse", "HEAD"]).trim().to_string();

    let inner = tmp.path().join("inner-src");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(
        inner.join(".gitmodules"),
        format!(
            "[submodule \"nest\"]\n\tpath = nest\n\turl = file://{}\n",
            nest.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&inner)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{nest_head},nest"),
        ])
        .status()
        .unwrap();
    git(&inner, &["add", ".gitmodules"]);
    git(&inner, &["commit", "-m", "inner"]);
    let inner_head = git(&inner, &["rev-parse", "HEAD"]).trim().to_string();

    let root = tmp.path().join("root");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"inner\"]\n\tpath = inner\n\turl = file://{}\n",
            inner.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{inner_head},inner"),
        ])
        .status()
        .unwrap();
    git(&root, &["add", ".gitmodules"]);
    git(&root, &["commit", "-m", "root"]);
    install_submodule_checkout(&root, "inner", &inner, &inner_head);

    let key = tmp.path().join("sign-key");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "gpg.format", "ssh"]);
    git(
        &root,
        &[
            "config",
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ],
    );
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("read pub");
    let allowed = root.join("allowed_signers");
    std::fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed_signers");
    git(
        &root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );

    let repo = Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("open");
    let head: grit_lib::objects::ObjectId =
        git(&root, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let mut opts = grit_lib::tree_sha256::TreeSha256Options::default();
    opts.exclude_exact = vec!["inner/nest".into()];
    let digest = repo
        .tree_sha256(head, &opts)
        .expect("hash with exclusion")
        .digest;

    let key = key;
    let commit = head.to_hex();
    let body = format!(
        "object {commit}\ntype commit\ntag nested-excl\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_EXCLUDED_HEADER} inner/nest\n\
{TREE_SHA256_HEADER} {digest}\n\nnested exclusion tag\n"
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &signed)
        .expect("write tag");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify nested exclusion");
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        report.tree_sha256.as_ref().unwrap().excluded,
        vec![("inner/nest".to_owned(), nest_head.parse().unwrap())]
    );
}

#[test]
fn verify_tag_deep_nested_exclusion_path_from_root() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let leaf = tmp.path().join("leaf-src");
    fs::create_dir(&leaf).unwrap();
    git(&leaf, &["init", "-q", "-b", "main"]);
    fs::write(leaf.join("x.txt"), b"x\n").unwrap();
    git(&leaf, &["add", "x.txt"]);
    git(&leaf, &["commit", "-m", "leaf"]);
    let leaf_head = git(&leaf, &["rev-parse", "HEAD"]).trim().to_string();

    let mid = tmp.path().join("mid-src");
    fs::create_dir(&mid).unwrap();
    git(&mid, &["init", "-q", "-b", "main"]);
    fs::write(
        mid.join(".gitmodules"),
        format!(
            "[submodule \"leaf\"]\n\tpath = leaf\n\turl = file://{}\n",
            leaf.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&mid)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{leaf_head},leaf"),
        ])
        .status()
        .unwrap();
    git(&mid, &["add", ".gitmodules"]);
    git(&mid, &["commit", "-m", "mid"]);
    let mid_head = git(&mid, &["rev-parse", "HEAD"]).trim().to_string();

    let inner = tmp.path().join("inner-src");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(
        inner.join(".gitmodules"),
        format!(
            "[submodule \"mid\"]\n\tpath = mid\n\turl = file://{}\n",
            mid.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&inner)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{mid_head},mid"),
        ])
        .status()
        .unwrap();
    git(&inner, &["add", ".gitmodules"]);
    git(&inner, &["commit", "-m", "inner"]);
    let inner_head = git(&inner, &["rev-parse", "HEAD"]).trim().to_string();

    let root = tmp.path().join("root");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"inner\"]\n\tpath = inner\n\turl = file://{}\n",
            inner.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{inner_head},inner"),
        ])
        .status()
        .unwrap();
    git(&root, &["add", ".gitmodules"]);
    git(&root, &["commit", "-m", "root"]);
    install_submodule_checkout(&root, "inner", &inner, &inner_head);
    git(
        &root.join("inner"),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "mid",
        ],
    );

    let key = tmp.path().join("sign-key");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "gpg.format", "ssh"]);
    git(
        &root,
        &[
            "config",
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ],
    );
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("read pub");
    let allowed = root.join("allowed_signers");
    fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed_signers");
    git(
        &root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );

    let repo = Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("open");
    let head: grit_lib::objects::ObjectId =
        git(&root, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let mut opts = grit_lib::tree_sha256::TreeSha256Options::default();
    opts.exclude_exact = vec!["inner/mid/leaf".into()];
    let digest = repo.tree_sha256(head, &opts).expect("hash").digest;

    let body = format!(
        "object {}\ntype commit\ntag deep-excl\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_EXCLUDED_HEADER} inner/mid/leaf\n\
{TREE_SHA256_HEADER} {digest}\n\ndeep exclusion tag\n",
        head.to_hex()
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &signed)
        .expect("write tag");
    let report = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .expect("verify deep nested exclusion");
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        report.tree_sha256.as_ref().unwrap().excluded,
        vec![("inner/mid/leaf".to_owned(), leaf_head.parse().unwrap())]
    );
}

#[test]
fn verify_tag_deep_nested_missing_submodule_path_from_root() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let leaf = tmp.path().join("leaf-src");
    fs::create_dir(&leaf).unwrap();
    git(&leaf, &["init", "-q", "-b", "main"]);
    fs::write(leaf.join("x.txt"), b"x\n").unwrap();
    git(&leaf, &["add", "x.txt"]);
    git(&leaf, &["commit", "-m", "leaf"]);
    let leaf_head = git(&leaf, &["rev-parse", "HEAD"]).trim().to_string();

    let mid = tmp.path().join("mid-src");
    fs::create_dir(&mid).unwrap();
    git(&mid, &["init", "-q", "-b", "main"]);
    fs::write(
        mid.join(".gitmodules"),
        format!(
            "[submodule \"leaf\"]\n\tpath = leaf\n\turl = file://{}\n",
            leaf.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&mid)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{leaf_head},leaf"),
        ])
        .status()
        .unwrap();
    git(&mid, &["add", ".gitmodules"]);
    git(&mid, &["commit", "-m", "mid"]);
    let mid_head = git(&mid, &["rev-parse", "HEAD"]).trim().to_string();

    let inner = tmp.path().join("inner-src");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(
        inner.join(".gitmodules"),
        format!(
            "[submodule \"mid\"]\n\tpath = mid\n\turl = file://{}\n",
            mid.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&inner)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{mid_head},mid"),
        ])
        .status()
        .unwrap();
    git(&inner, &["add", ".gitmodules"]);
    git(&inner, &["commit", "-m", "inner"]);
    let inner_head = git(&inner, &["rev-parse", "HEAD"]).trim().to_string();

    let root = tmp.path().join("root");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"inner\"]\n\tpath = inner\n\turl = file://{}\n",
            inner.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{inner_head},inner"),
        ])
        .status()
        .unwrap();
    git(&root, &["add", ".gitmodules"]);
    git(&root, &["commit", "-m", "root"]);
    install_submodule_checkout(&root, "inner", &inner, &inner_head);
    git(
        &root.join("inner"),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "mid",
        ],
    );
    git(
        &root.join("inner/mid"),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "leaf",
        ],
    );

    let key = tmp.path().join("sign-key");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "gpg.format", "ssh"]);
    git(
        &root,
        &[
            "config",
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ],
    );
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("read pub");
    let allowed = root.join("allowed_signers");
    fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed_signers");
    git(
        &root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );

    let repo = Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("open");
    let head: grit_lib::objects::ObjectId =
        git(&root, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let digest = repo
        .tree_sha256(head, &grit_lib::tree_sha256::TreeSha256Options::default())
        .expect("hash with leaf present")
        .digest;

    let body = format!(
        "object {}\ntype commit\ntag deep-miss\ntagger T <t@example.com> 1 +0000\n\
{TREE_SHA256_HEADER} {digest}\n\ndeep missing leaf tag\n",
        head.to_hex()
    );
    let signed = sign_payload_ssh(&key, body.as_bytes());
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &signed)
        .expect("write tag");

    fs::remove_dir_all(root.join("inner/mid/leaf")).expect("remove leaf checkout");

    let err = repo
        .verify_tag(
            tag_oid,
            &TagVerifyOptions {
                check_signature: true,
                gpg: gpg_config(&repo),
            },
        )
        .unwrap_err();
    match err {
        Error::TreeSha256MissingSubmodules(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].path, "inner/mid/leaf");
        }
        other => panic!("expected missing submodule error, got {other:?}"),
    }
}

#[test]
fn parse_tree_sha256_tag_headers_reads_excluded() {
    let data = b"object 0000000000000000000000000000000000000001\n\
type commit\ntag t\ntagger T <t@e.com> 1 +0000\n\
tree-sha256-excluded sub/mod\n\
tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\nm\n";
    let h = parse_tree_sha256_tag_headers(data).expect("parse");
    assert_eq!(h.excluded, vec!["sub/mod".to_owned()]);
    assert!(h.claimed.is_some());
}
