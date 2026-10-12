//! Reflog entries for branch checkout and branch creation (Git-compatible messages).

use crate::commit::{assemble_identity, identity_now_from_epoch};
use crate::config::ConfigSet;
use crate::diff::zero_oid;
use crate::error::Result;
use crate::ident_resolve::resolve_loose_committer_parts_with;
use crate::objects::ObjectId;
use crate::refs::{self, append_reflog};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

/// Committer identity line for reflog updates (`Name <email> epoch tz`).
pub fn checkout_reflog_identity(repo: &Repository) -> String {
    let env = repo.environment();
    let config = ConfigSet::load(env, Some(&repo.git_dir), true).unwrap_or_default();
    let now = identity_now_from_epoch(
        env.var("SOURCE_DATE_EPOCH")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| time::OffsetDateTime::now_utc().unix_timestamp()),
        env.var("TZ").as_deref(),
    );
    let (name, email) = resolve_loose_committer_parts_with(env, &config);
    assemble_identity(&name, &email, None, now)
}

/// `from` side of `checkout: moving from … to …` for the current `HEAD` state.
#[must_use]
pub fn checkout_reflog_from_label(head: &HeadState) -> Option<String> {
    match head {
        HeadState::Branch {
            short_name,
            oid: Some(_),
            ..
        } => Some(short_name.clone()),
        HeadState::Detached { oid } => Some(oid.to_hex()),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => None,
    }
}

/// Source label for `branch: Created from …` after `grit branch` (implicit start at `HEAD`).
#[must_use]
pub fn branch_create_reflog_source_label(head: &HeadState) -> String {
    match head {
        HeadState::Branch {
            short_name,
            oid: Some(_),
            ..
        } => short_name.clone(),
        HeadState::Detached { .. } => "HEAD".to_owned(),
        HeadState::Branch { .. } | HeadState::Invalid => "HEAD".to_owned(),
    }
}

/// Source label for `branch: Created from …` when `grit switch -c` supplies an explicit start point.
#[must_use]
pub fn switch_create_reflog_source_label(start_point: Option<&str>) -> String {
    match start_point {
        None => "HEAD".to_owned(),
        Some(spec) => spec.to_owned(),
    }
}

/// Append `checkout: moving from … to …` on `HEAD` when reflog autocreate is enabled.
pub fn append_head_checkout_reflog(
    repo: &Repository,
    old_oid: ObjectId,
    new_oid: ObjectId,
    from_label: &str,
    to_label: &str,
) -> Result<()> {
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    if !refs::should_autocreate_reflog_with_config(&config, &repo.git_dir, "HEAD") {
        return Ok(());
    }
    let identity = checkout_reflog_identity(repo);
    let message = format!("checkout: moving from {from_label} to {to_label}");
    append_reflog(
        &repo.git_dir,
        "HEAD",
        &old_oid,
        &new_oid,
        &identity,
        &message,
        false,
    )
}

/// Append `branch: Created from …` on a new branch ref.
pub fn append_branch_created_reflog(
    repo: &Repository,
    branch_ref: &str,
    start_oid: ObjectId,
    source_label: &str,
) -> Result<()> {
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    if !refs::should_autocreate_reflog_with_config(&config, &repo.git_dir, branch_ref) {
        return Ok(());
    }
    let identity = checkout_reflog_identity(repo);
    let message = format!("branch: Created from {source_label}");
    append_reflog(
        &repo.git_dir,
        branch_ref,
        &zero_oid(),
        &start_oid,
        &identity,
        &message,
        false,
    )
}

/// Whether `HEAD` already points at this attached branch tip (no checkout reflog).
pub fn is_already_on_branch(
    repo: &Repository,
    branch_ref: &str,
    target_oid: ObjectId,
) -> Result<bool> {
    let head = resolve_head(&repo.git_dir)?;
    Ok(matches!(
        head,
        HeadState::Branch {
            ref refname,
            oid: Some(oid),
            ..
        } if refname == branch_ref && oid == target_oid
    ))
}

/// Whether detached `HEAD` already points at `target_oid`.
pub fn is_already_detached_at(repo: &Repository, target_oid: ObjectId) -> Result<bool> {
    let head = resolve_head(&repo.git_dir)?;
    Ok(matches!(
        head,
        HeadState::Detached { oid } if oid == target_oid
    ))
}
