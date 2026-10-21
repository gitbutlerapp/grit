//! Commit object bytes match `git commit-tree` when identity and time come from [`Environment`].

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use grit_lib::config::environment::{offset_from_epoch_and_tz, Environment, RepositoryOptions};
use grit_lib::config::ident_resolve::{
    identity_email_is_configured_with, resolve_email_lenient_with, resolve_email_with,
    resolve_identity_with, IdentRole, IdentityError,
};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};

const FIXED_EPOCH: i64 = 1_112_911_993;

#[test]
fn strict_identity_refuses_synthetic_email_without_config() {
    let vars = [("USER", "alice"), ("HOSTNAME", "host.example")]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
    let env = Environment::from_vars(vars, PathBuf::new());
    let config = ConfigSet::default();
    assert!(!identity_email_is_configured_with(
        &env,
        &config,
        IdentRole::Author
    ));
    let err = resolve_email_with(&env, &config, IdentRole::Author).expect_err("strict email");
    assert_eq!(
        err,
        IdentityError::MissingConfiguredEmail {
            role: IdentRole::Author
        }
    );
    let lenient = resolve_email_lenient_with(&env, &config, IdentRole::Author);
    assert_eq!(lenient, "alice@host.example");
}

#[test]
fn create_commit_bytes_match_git_with_explicit_environment_identity() {
    if !Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return;
    }

    let saved_tz = std::env::var("TZ").ok();
    std::env::set_var("TZ", "UTC");
    grit_lib::config::git_date::tm::refresh_process_tz();

    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");

    let disable_change_ids = Command::new("git")
        .current_dir(root)
        .args(["config", "grit.changeIds", "false"])
        .output()
        .expect("git config grit.changeIds");
    assert!(
        disable_change_ids.status.success(),
        "git config grit.changeIds: {}",
        String::from_utf8_lossy(&disable_change_ids.stderr)
    );

    let vars = [
        ("TZ", "America/New_York"),
        ("GIT_AUTHOR_NAME", "A U Thor"),
        ("GIT_AUTHOR_EMAIL", "author@example.com"),
        ("GIT_COMMITTER_NAME", "C O Mmitter"),
        ("GIT_COMMITTER_EMAIL", "committer@example.com"),
        ("GIT_AUTHOR_DATE", "@1112911993"),
        ("GIT_COMMITTER_DATE", "@1112911993"),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)));

    let mut env = Environment::from_vars(vars, PathBuf::from(root));
    env.git_config_nosystem = Some("true".into());

    let mut options = RepositoryOptions::with_environment(env.clone());
    options.reference_unix_time = Some(FIXED_EPOCH);

    let repo = Repository::discover(Some(root), &options).expect("discover with environment");
    fs::write(root.join("f"), b"x\n").expect("worktree file");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec![".".to_owned()],
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage");
    let config = repo.config().expect("config");
    let now = repo.identity_now();
    assert_eq!(
        now,
        offset_from_epoch_and_tz(FIXED_EPOCH, env.tz.as_deref()),
        "identity_now follows the handle's reference time and TZ"
    );
    let author = resolve_identity_with(&env, config.as_ref(), IdentRole::Author, now)
        .expect("author identity");
    let committer = resolve_identity_with(&env, config.as_ref(), IdentRole::Committer, now)
        .expect("committer identity");

    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message: "subject".to_owned(),
            author,
            committer,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
        },
        &mut NullProgress,
    )
    .expect("create_commit");
    let grit_oid = outcome.oid;

    let commit_obj = repo.odb().read(&grit_oid).expect("read commit");
    assert_eq!(commit_obj.kind, ObjectKind::Commit);
    let parsed = parse_commit(&commit_obj.data).expect("parse");
    let tree_hex = parsed.tree.to_hex();

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(root)
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed: stdout={} stderr={}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );

    let git_out = Command::new("git")
        .args(["commit-tree", &tree_hex, "-m", "subject"])
        .env("TZ", "America/New_York")
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "C O Mmitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_AUTHOR_DATE", "@1112911993")
        .env("GIT_COMMITTER_DATE", "@1112911993")
        .current_dir(root)
        .output()
        .expect("git commit-tree");
    assert!(
        git_out.status.success(),
        "git commit-tree: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_oid = String::from_utf8_lossy(&git_out.stdout).trim().to_owned();
    assert_eq!(git_oid, grit_oid.to_hex());

    match saved_tz {
        Some(v) => std::env::set_var("TZ", v),
        None => std::env::remove_var("TZ"),
    }
    grit_lib::config::git_date::tm::refresh_process_tz();
}
