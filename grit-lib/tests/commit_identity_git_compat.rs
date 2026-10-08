//! Commit object bytes match `git commit-tree` when identity and time come from [`Environment`].

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use grit_lib::commit::assemble_identity;
use grit_lib::config::ConfigSet;
use grit_lib::environment::{offset_from_epoch_and_tz, Environment, RepositoryOptions};
use grit_lib::ident_resolve::{
    resolve_email_lenient_with, resolve_name_with, IdentRole, IdentityError,
};
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use time::OffsetDateTime;

const FIXED_EPOCH: i64 = 1_112_911_993;

fn identity_from_environment(
    env: &Environment,
    config: &ConfigSet,
    role: IdentRole,
    now: OffsetDateTime,
) -> Result<String, IdentityError> {
    let date_key = match role {
        IdentRole::Author => "GIT_AUTHOR_DATE",
        IdentRole::Committer => "GIT_COMMITTER_DATE",
    };
    let date_override = env.var(date_key).filter(|d| !d.trim().is_empty());
    let name = resolve_name_with(env, config, role)?;
    let email = resolve_email_lenient_with(env, config, role);
    Ok(assemble_identity(
        &name,
        &email,
        date_override.as_deref(),
        now,
    ))
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
    grit_lib::git_date::tm::refresh_process_tz();

    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    init_repository(root, false, "main", None, "files").expect("init");

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

    let repo = Repository::discover_with(&options, Some(root)).expect("discover with environment");
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
    let now = offset_from_epoch_and_tz(FIXED_EPOCH, env.tz.as_deref());
    let author = identity_from_environment(&env, config.as_ref(), IdentRole::Author, now)
        .expect("author identity");
    let committer = identity_from_environment(&env, config.as_ref(), IdentRole::Committer, now)
        .expect("committer identity");

    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message: "subject".to_owned(),
            author,
            committer,
            allow_empty: false,
            sign_override: None,
        },
        &mut NullProgress,
    )
    .expect("create_commit");
    let grit_oid = outcome.oid;

    let commit_obj = repo.odb.read(&grit_oid).expect("read commit");
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
    grit_lib::git_date::tm::refresh_process_tz();
}
