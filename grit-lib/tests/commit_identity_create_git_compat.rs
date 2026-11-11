//! [`create_commit`] identities must pass `git fsck --strict`, and typed identities that cannot
//! be written are rejected before anything is recorded.

use grit_lib::error::ObjectError;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ident_resolve::{IdentRole, IdentityError};
use grit_lib::error::Error;
use grit_lib::ident::{Signature, SignatureError};
use grit_lib::porcelain::commit::CommitRequest;
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use time::OffsetDateTime;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_stdout(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init(root: &std::path::Path) -> Repository {
    Repository::init(
        root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init")
    .0
}

/// Reopen `root` with an explicit identity environment (no process environment reads).
fn open_with_env(root: &std::path::Path, vars: &[(&str, &str)]) -> Repository {
    let env = Environment::from_vars(
        vars.iter().map(|(k, v)| ((*k).into(), (*v).into())),
        root.to_path_buf(),
    );
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(root),
    )
    .expect("open")
}

#[test]
fn create_commit_signature_passes_git_fsck_strict() {
    if !git_available() {
        return;
    }

    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    let repo = init(root);
    std::fs::write(root.join("a.txt"), "hi\n").expect("file");
    &repo.index().stage_worktree_paths(&[]).expect("stage");

    let doc = Signature::new(
        "Doc",
        "doc@example.com",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("signature");
    let req = {
        let mut req = CommitRequest::new("first\n");
        req.author = Some(doc.clone());
        req.committer = Some(doc);
        req
    };
    repo.commits()
        .create_staged(
            &req,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("create_commit");

    git_stdout(root, &["fsck", "--strict"]);
    assert_eq!(
        git_stdout(root, &["log", "-1", "--format=%an <%ae> %at|%cn <%ce> %ct"]).trim(),
        "Doc <doc@example.com> 1700000000|Doc <doc@example.com> 1700000000"
    );
}

#[test]
fn create_commit_defaults_to_repository_identity() {
    if !git_available() {
        return;
    }

    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    init(root);
    let repo = open_with_env(
        root,
        &[
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("GIT_AUTHOR_EMAIL", "author@example.com"),
            ("GIT_AUTHOR_DATE", "@1112911993 -0700"),
            ("GIT_COMMITTER_NAME", "C O Mitter"),
            ("GIT_COMMITTER_EMAIL", "committer@example.com"),
            ("GIT_COMMITTER_DATE", "1112912053 -0700"),
        ],
    );
    assert_eq!(
        repo.identity(IdentRole::Author, repo.identity_now())
            .expect("author")
            .header_line(),
        "A U Thor <author@example.com> 1112911993 -0700"
    );
    assert_eq!(
        repo.identity(IdentRole::Committer, repo.identity_now())
            .expect("committer")
            .header_line(),
        "C O Mitter <committer@example.com> 1112912053 -0700"
    );

    std::fs::write(root.join("a.txt"), "hi\n").expect("file");
    &repo.index().stage_worktree_paths(&[]).expect("stage");
    let req = {
        let mut req = CommitRequest::new("first\n");
        req
    };
    repo.commits()
        .create_staged(
            &req,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("create_commit");

    git_stdout(root, &["fsck", "--strict"]);
    assert_eq!(
        git_stdout(root, &["log", "-1", "--format=%an <%ae> %at|%cn <%ce> %ct"]).trim(),
        "A U Thor <author@example.com> 1112911993|C O Mitter <committer@example.com> 1112912053"
    );
}

#[test]
fn repository_identity_reports_missing_configuration() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    init(root);
    let repo = open_with_env(root, &[("GIT_AUTHOR_NAME", "Nobody")]);
    let err = repo
        .identity(IdentRole::Author, repo.identity_now())
        .expect_err("no email configured");
    assert!(
        matches!(
            err,
            CommitError::Identity(IdentityError::MissingConfiguredEmail {
                role: IdentRole::Author
            })
        ),
        "unexpected error: {err}"
    );

    let req = {
        let mut req = CommitRequest::new("x\n");
        req.allow_empty = true;
        req
    };
    let err = repo
        .commits()
        .create_staged(
            &req,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("no identity");
    assert!(
        matches!(
            err,
            grit_lib::error::Error::Commit(CommitError::Identity(_))
        ),
        "unexpected error: {err}"
    );
}

#[test]
fn create_commit_rejects_unwritable_signatures() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    let repo = init(root);
    let time = OffsetDateTime::from_unix_timestamp(1).expect("time");
    // `Signature::new` rejects the injected email, so build a valid one and corrupt it.
    let mut injected = Signature::new("Name", "a@b", time).expect("valid signature");
    injected.email = "a@b>\nInjected: x".into();

    let bad = {
        let mut req = CommitRequest::new("x\n");
        req.author = Some(injected.clone());
        req.committer = Some(injected);
        req.allow_empty = true;
        req
    };
    let err = repo
        .commits()
        .create_staged(
            &bad,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("invalid identity");
    assert!(
        matches!(
            err,
            Error::Object(ObjectError::InvalidSignature(
                SignatureError::ForbiddenCharacter { .. }
            ))
        ),
        "unexpected error: {err}"
    );
    assert!(
        repo.references().resolve("refs/heads/main").is_err(),
        "nothing may be committed with an invalid identity"
    );
}
