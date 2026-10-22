//! Signed tags with tree-sha256 claims interoperate with the system `git` binary.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{extra_header_values, parse_tag, ObjectId};
use grit_lib::porcelain::tag::{create_tag, list_tags, CreateTagRequest};
use grit_lib::refs::listing::RefQuery;
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::{TreeSha256Claim, TreeSha256Options, TREE_SHA256_EXCLUDED_HEADER};
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

fn signed_tag_with_tree_sha256(
    repo: &Repository,
    name: &str,
    head: ObjectId,
    opts: TreeSha256Options,
) -> grit_lib::porcelain::tag::CreateTagOutcome {
    create_tag(
        repo,
        &CreateTagRequest {
            name: name.to_owned(),
            target: head,
            message: Some("release".to_owned()),
            tagger: Some("T <t@example.com> 1700000000 +0000".to_owned()),
            sign: Some(true),
            signing_key: None,
            tree_sha256: Some(opts),
            force: false,
            dry_run: false,
        },
    )
    .expect("create signed tag with tree-sha256")
}

#[test]
fn signed_tag_with_tree_sha256_verifies_with_git() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let outcome = signed_tag_with_tree_sha256(&repo, "v1", head, TreeSha256Options::default());

    git_out(root, &["verify-tag", "v1"]);
    let cat = git_out(root, &["cat-file", "-p", &outcome.oid.to_hex()]);
    assert!(
        cat.contains("tree-sha256 "),
        "expected tree-sha256 header in tag:\n{cat}"
    );
}

#[test]
fn header_digest_equals_recomputed() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let outcome = signed_tag_with_tree_sha256(&repo, "v1", head, TreeSha256Options::default());
    let ts = outcome.tree_sha256.expect("tree_sha256 outcome");
    let raw = repo.odb().read(&outcome.oid).expect("read tag").data;
    let tag = parse_tag(&raw).expect("parse tag");
    let claim = TreeSha256Claim::from_tag(&tag)
        .expect("from_tag")
        .expect("claim");
    assert_eq!(claim.digest, ts.claim.digest);

    let recomputed = repo
        .tree_sha256(head, &TreeSha256Options::default())
        .expect("recompute");
    assert_eq!(claim.digest, recomputed.digest);
}

#[test]
fn excluded_submodule_writes_excluded_header() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);

    let sub_dir = root.join("vendor-src");
    std::fs::create_dir(&sub_dir).unwrap();
    git(&sub_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(sub_dir.join("s"), b"s\n").unwrap();
    git(&sub_dir, &["add", "s"]);
    git(&sub_dir, &["commit", "-m", "sub"]);
    let sub_head = git(&sub_dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"vendor\"]\n\tpath = vendor\n\turl = file://{}\n",
            sub_dir.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},vendor"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("update-index");
    git(root, &["add", ".gitmodules"]);
    git(root, &["commit", "-m", "gitlink"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let mut opts = TreeSha256Options::default();
    opts.exclude_globs = vec!["vendor".into()];
    let outcome = signed_tag_with_tree_sha256(&repo, "rel", head, opts);
    let raw = repo.odb().read(&outcome.oid).expect("read tag").data;
    let tag = parse_tag(&raw).expect("parse");
    let excluded = extra_header_values(&tag.extra_headers, TREE_SHA256_EXCLUDED_HEADER);
    assert_eq!(excluded, vec![b"vendor".as_slice()]);
    let ts = outcome.tree_sha256.expect("outcome");
    assert_eq!(ts.report.excluded.len(), 1);
    assert_eq!(ts.report.excluded[0].1.to_hex(), sub_head);
    git_out(root, &["verify-tag", "rel"]);
}

#[test]
fn missing_submodule_fails_before_writing_tag() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);

    let sub = tempfile::tempdir().expect("sub");
    git(sub.path(), &["init", "-q", "-b", "main", "."]);
    git(sub.path(), &["commit", "--allow-empty", "-m", "sub"]);
    let sub_head = git(sub.path(), &["rev-parse", "HEAD"]).trim().to_string();

    std::fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"vendor\"]\n\tpath = vendor\n\turl = file://{}\n",
            sub.path().display()
        ),
    )
    .unwrap();
    git(root, &["add", ".gitmodules"]);
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &sub_head,
            "vendor",
        ],
    );
    git(root, &["commit", "-m", "gitlink without checkout"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let err = create_tag(
        &repo,
        &CreateTagRequest {
            name: "bad".to_owned(),
            target: head,
            message: Some("x".into()),
            tagger: Some("T <t@example.com> 1700000000 +0000".into()),
            sign: Some(true),
            signing_key: None,
            tree_sha256: Some(TreeSha256Options::default()),
            force: false,
            dry_run: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::TreeSha256MissingSubmodules(_)),
        "{err:?}"
    );
    assert!(list_tags(&repo, &RefQuery::default())
        .expect("list")
        .is_empty());
}

#[test]
fn tree_sha256_with_no_sign_is_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["commit", "--allow-empty", "-m", "base"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let err = create_tag(
        &repo,
        &CreateTagRequest {
            name: "nope".to_owned(),
            target: head,
            message: Some("x".into()),
            tagger: Some("T <t@example.com> 1700000000 +0000".into()),
            sign: Some(false),
            signing_key: None,
            tree_sha256: Some(TreeSha256Options::default()),
            force: false,
            dry_run: false,
        },
    )
    .unwrap_err();
    assert!(matches!(err, Error::TreeSha256RequiresSigning));
}

#[test]
fn signed_tag_tree_sha256_overhead_within_bound() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    setup_ssh_signing_repo(root);
    for i in 0..64 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "many files"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let tagger = "T <t@example.com> 1700000000 +0000".to_owned();

    let sign_only = std::time::Instant::now();
    create_tag(
        &repo,
        &CreateTagRequest {
            name: "sign-only".to_owned(),
            target: head,
            message: Some("m".into()),
            tagger: Some(tagger.clone()),
            sign: Some(true),
            signing_key: None,
            tree_sha256: None,
            force: false,
            dry_run: false,
        },
    )
    .expect("sign only");
    let sign_only_elapsed = sign_only.elapsed();

    let with_ts = std::time::Instant::now();
    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "with-ts".to_owned(),
            target: head,
            message: Some("m".into()),
            tagger: Some(tagger),
            sign: Some(true),
            signing_key: None,
            tree_sha256: Some(TreeSha256Options::default()),
            force: false,
            dry_run: false,
        },
    )
    .expect("sign with tree-sha256");
    let with_ts_elapsed = with_ts.elapsed();
    let hash_ms = outcome.tree_sha256.expect("ts").hash_elapsed.as_secs_f64() * 1000.0;

    let overhead_ms =
        with_ts_elapsed.as_secs_f64() * 1000.0 - sign_only_elapsed.as_secs_f64() * 1000.0;
    assert!(
        overhead_ms <= hash_ms + 10.0,
        "tree-sha256 tag overhead {overhead_ms:.2}ms exceeds hash time {hash_ms:.2}ms + 10ms (sign-only {sign_only_elapsed:?}, with-ts {with_ts_elapsed:?})"
    );
}
