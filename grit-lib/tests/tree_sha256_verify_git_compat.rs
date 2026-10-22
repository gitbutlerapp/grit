//! tree-sha256 tag verification with system `git` and SSH signing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::command_runner::{CommandRunner, CommandSpec, RecordedResponse, RecordingRunner};
use grit_lib::commit::signing::GpgConfig;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{serialize_tag, set_extra_header, ObjectKind, TagData};
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
    let unsigned = body.as_bytes();
    let signed = sign_payload_ssh(&key, unsigned);
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, &signed)
        .expect("write tag");

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

#[derive(Debug)]
struct DelaySigningRunner {
    inner: Arc<RecordingRunner>,
    delay: Duration,
}

impl CommandRunner for DelaySigningRunner {
    fn spawn(
        &self,
        spec: &CommandSpec,
    ) -> std::io::Result<grit_lib::command_runner::RunningCommand> {
        std::thread::sleep(self.delay);
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
    let delay_runner = Arc::new(DelaySigningRunner {
        inner: recording.clone(),
        delay: Duration::from_millis(250),
    });
    let gpg = GpgConfig::from_config(repo.config().expect("config").as_ref())
        .expect("gpg")
        .with_command_runner(delay_runner);

    let started = Instant::now();
    let report = verify_tag(
        &repo,
        tag_oid,
        &TagVerifyOptions {
            check_signature: true,
            gpg,
        },
    )
    .expect("verify");
    let elapsed = started.elapsed();
    assert!(report.tree_sha256.is_some());
    assert!(matches!(report.signature, SignatureVerdict::Good { .. }));
    assert!(
        !recording.specs().is_empty(),
        "signing subprocess should run"
    );
    assert!(
        elapsed < Duration::from_millis(400),
        "hash and signature should overlap; sequential would exceed 400ms, took {elapsed:?}"
    );
    assert!(elapsed >= Duration::from_millis(200));
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
