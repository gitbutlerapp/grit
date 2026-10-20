//! Shared repository helpers used across `grit` commands.

use anyhow::{bail, Context, Result};
use grit_lib::config::environment::Environment;
use grit_lib::config::ident_resolve::{
    resolve_email_with, resolve_loose_committer_parts_with, resolve_name_with, IdentRole,
    IdentityError,
};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::refs;
use grit_lib::repo::Repository;
use std::path::PathBuf;
use std::sync::OnceLock;
use time::OffsetDateTime;

static THREAD_POLICY: OnceLock<ThreadPolicy> = OnceLock::new();

/// Apply the global `--threads` flag for subsequent repository opens in this process.
pub fn set_thread_policy(threads: Option<usize>) {
    let _ = THREAD_POLICY.set(ThreadPolicy::with_threads_override(threads));
}

/// Thread policy for [`grit_lib::config::environment::RepositoryOptions`] (global `--threads` when set).
pub fn thread_policy_for_repo() -> ThreadPolicy {
    THREAD_POLICY.get().copied().unwrap_or_default()
}

use crate::diagnostics;

/// Build the process [`Environment`] for repository discovery and config loading.
pub fn environment() -> Environment {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Environment::from_vars(std::env::vars_os(), cwd)
}

/// Wall-clock instant for commit/reflog identity, with [`Environment::tz`] applied.
pub fn wall_clock_now(env: &Environment) -> OffsetDateTime {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    grit_lib::config::environment::offset_from_epoch_and_tz(epoch, env.tz.as_deref())
}

/// A resolved "target" branch (the trunk `grit` measures the current branch against).
#[derive(Debug, Clone)]
pub struct TargetBranch {
    pub display_name: String,
    pub oid: ObjectId,
}

/// A one-line summary of a commit, for shortlog-style output.
#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub oid: ObjectId,
    pub subject: String,
    /// Author display name: the identity name when present, otherwise the email
    /// local-part (e.g. `schacon` from `schacon@gmail.com`).
    pub author: String,
    /// Raw `author` line from the commit object (`Name <email> <epoch> <tz>`).
    pub author_ident: String,
    /// Author date as a Unix timestamp (for relative-date rendering).
    pub timestamp: i64,
}

/// Discover the repository containing the current directory.
pub fn discover() -> Result<Repository> {
    let (options, _sink) = diagnostics::repository_options(false);
    Repository::discover(None, &options).context("not in a repository")
}

/// Like [`discover`], but returns the diagnostic sink used for warnings.
pub fn discover_with_warnings(
) -> Result<(Repository, std::sync::Arc<diagnostics::CliDiagnosticSink>)> {
    let (options, sink) = diagnostics::repository_options(false);
    let repo = Repository::discover(None, &options).context("not in a repository")?;
    Ok((repo, sink))
}

/// Find the branch `grit` should measure the current branch against, trying
/// `target.branch` from config first, then the usual trunk names.
pub fn find_target_branch(repo: &Repository) -> Result<Option<TargetBranch>> {
    for candidate in target_branch_candidates(repo)? {
        if let Some(oid) = resolve_branch_candidate(repo, &candidate) {
            return Ok(Some(TargetBranch {
                display_name: candidate,
                oid,
            }));
        }
    }
    Ok(None)
}

fn target_branch_candidates(repo: &Repository) -> Result<Vec<String>> {
    let config = repo.config().context("could not load config")?;
    let mut candidates = Vec::new();
    if let Some(target) = config.as_ref().get("target.branch") {
        let trimmed = target.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_owned());
        }
    }
    candidates.extend([
        "origin/master".to_owned(),
        "origin/main".to_owned(),
        "master".to_owned(),
        "main".to_owned(),
    ]);
    Ok(candidates)
}

fn resolve_branch_candidate(repo: &Repository, candidate: &str) -> Option<ObjectId> {
    for refname in candidate_refnames(candidate) {
        if let Ok(oid) = refs::resolve_ref(repo.git_dir(), &refname) {
            return Some(oid);
        }
    }
    None
}

fn candidate_refnames(candidate: &str) -> Vec<String> {
    if candidate.starts_with("refs/") || candidate == "HEAD" {
        return vec![candidate.to_owned()];
    }

    if let Some(remote_branch) = candidate.strip_prefix("origin/") {
        return vec![
            format!("refs/remotes/origin/{remote_branch}"),
            format!("refs/heads/{candidate}"),
        ];
    }

    vec![
        format!("refs/heads/{candidate}"),
        format!("refs/remotes/{candidate}"),
    ]
}

/// Summary of how far `head` is ahead of `target` for status/shortlog.
#[derive(Debug, Clone)]
pub struct AheadOfTarget {
    /// Total commits reachable from `head` but not from `target`.
    pub total: usize,
    /// Up to `summary_limit` newest such commits (newest first).
    pub commits: Vec<CommitSummary>,
}

/// Commits on `head` not in `target`'s history: full ahead count plus bounded summaries.
///
/// Only the newest `summary_limit` commits are read and parsed; `total` reflects the
/// full ahead count. JSON/Markdown status uses this so default `grit status` does not
/// materialize unbounded ahead lists.
pub fn commits_ahead_of(
    repo: &Repository,
    head: ObjectId,
    target: ObjectId,
    summary_limit: usize,
) -> Result<AheadOfTarget> {
    let (total, oids) =
        grit_lib::revwalk::merge_base::ahead_of_target_commits(repo, head, target, summary_limit)?;
    let mut commits = Vec::with_capacity(oids.len());
    for oid in oids {
        let commit = read_commit(repo, &oid)?;
        let (author, timestamp) = author_and_time(&commit.author);
        commits.push(CommitSummary {
            oid,
            subject: subject_line(&commit.message),
            author,
            author_ident: commit.author.clone(),
            timestamp,
        });
    }

    Ok(AheadOfTarget { total, commits })
}

/// Ahead/behind vs `target` plus bounded newest-ahead commit summaries for status.
#[derive(Debug, Clone)]
pub struct AheadBehindOfTarget {
    pub ahead: usize,
    pub behind: usize,
    pub commits: Vec<CommitSummary>,
}

/// Symmetric ahead/behind between `head` and `target`, with a bounded ahead shortlog.
///
/// Uses a streaming rev walk (no full ancestor closures). When one tip is an ancestor of
/// the other, the unnecessary direction is skipped.
pub fn commits_ahead_behind_of(
    repo: &Repository,
    head: ObjectId,
    target: ObjectId,
    summary_limit: usize,
) -> Result<AheadBehindOfTarget> {
    let (ahead, behind, oids) = grit_lib::revwalk::merge_base::ahead_behind_of_target_commits(
        repo,
        head,
        target,
        summary_limit,
    )?;
    let mut commits = Vec::with_capacity(oids.len());
    for oid in oids {
        let commit = read_commit(repo, &oid)?;
        let (author, timestamp) = author_and_time(&commit.author);
        commits.push(CommitSummary {
            oid,
            subject: subject_line(&commit.message),
            author,
            author_ident: commit.author.clone(),
            timestamp,
        });
    }
    Ok(AheadBehindOfTarget {
        ahead,
        behind,
        commits,
    })
}

/// Parsed fields from a Git commit identity line (`Name <email> <epoch> <tz>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedIdentityLine {
    pub name: String,
    pub email: String,
    /// Timestamp and timezone suffix after the closing `>` (e.g. `1700000000 +0000`).
    pub when_suffix: String,
}

/// Split a raw identity line into name, email, and the date suffix.
pub(crate) fn parse_identity_line(ident: &str) -> ParsedIdentityLine {
    let (name, rest) = ident.split_once(" <").map_or((ident, ""), |(n, r)| (n, r));
    let (email, when) = rest.split_once("> ").map_or((rest, ""), |(e, w)| (e, w));
    ParsedIdentityLine {
        name: name.trim().to_owned(),
        email: email.trim_end_matches('>').trim().to_owned(),
        when_suffix: when.to_owned(),
    }
}

/// Display author for log-style output: prefer the identity name, fall back to
/// the email local-part when the name is empty.
pub(crate) fn author_display_name(name: &str, email: &str) -> String {
    if !name.trim().is_empty() {
        name.trim().to_owned()
    } else {
        email.split('@').next().unwrap_or("").trim().to_owned()
    }
}

/// Parse a `Name <email> <epoch> <tz>` identity into a display author and Unix timestamp.
pub fn author_and_time(ident: &str) -> (String, i64) {
    let parsed = parse_identity_line(ident);
    let author = author_display_name(&parsed.name, &parsed.email);
    let timestamp = parsed
        .when_suffix
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    (author, timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_display_prefers_name_over_email_local_part() {
        let ident = "Zed Quartz <zed@q.test> 1700000000 +0000";
        let (author, ts) = author_and_time(ident);
        assert_eq!(author, "Zed Quartz");
        assert_eq!(ts, 1_700_000_000);
    }

    #[test]
    fn author_display_falls_back_to_email_local_part_when_name_empty() {
        let ident = " <nobody@example.com> 1 +0000";
        let (author, _) = author_and_time(ident);
        assert_eq!(author, "nobody");
    }

    #[test]
    fn author_display_uses_name_when_no_email_angle_brackets() {
        let ident = "Ada Lovelace";
        let (author, ts) = author_and_time(ident);
        assert_eq!(author, "Ada Lovelace");
        assert_eq!(ts, 0);
    }
}

/// A human "N units ago" string for a Unix `timestamp`, relative to `now`
/// (taking `now` explicitly so a list of rows shares one clock).
#[must_use]
pub fn relative_date_from(timestamp: i64, now: i64) -> String {
    let secs = now - timestamp;
    if secs < 0 {
        return "in the future".to_owned();
    }
    if secs < 60 {
        return "just now".to_owned();
    }
    let (n, unit) = if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else if secs < 86_400 * 14 {
        (secs / 86_400, "day")
    } else if secs < 86_400 * 70 {
        (secs / (86_400 * 7), "week")
    } else if secs < 86_400 * 365 {
        (secs / (86_400 * 30), "month")
    } else {
        (secs / (86_400 * 365), "year")
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// The tree OID recorded by a commit.
pub fn commit_tree(repo: &Repository, oid: &ObjectId) -> Result<ObjectId> {
    grit_lib::diff::tree_oid_of_commit_in_repo(repo, oid)
        .with_context(|| format!("could not read tree for commit {oid}"))
}

/// Read and parse a commit object.
pub fn read_commit(repo: &Repository, oid: &ObjectId) -> Result<CommitData> {
    let object = repo
        .odb()
        .read(oid)
        .with_context(|| format!("could not read commit {oid}"))?;
    if object.kind != ObjectKind::Commit {
        bail!("object {oid} is a {}, not a commit", object.kind);
    }
    parse_commit(&object.data).with_context(|| format!("could not parse commit {oid}"))
}

/// The first non-blank line of a commit message.
pub fn subject_line(message: &str) -> String {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("(no subject)")
        .to_owned()
}

/// Resolve a strict commit identity line (`Name <email> <epoch> <offset>`) for a
/// role, honoring the matching `GIT_*_DATE` override. Errors if no identity is
/// configured — used when creating a commit.
pub fn identity(
    env: &Environment,
    config: &ConfigSet,
    role: IdentRole,
    date_var: &str,
    now: OffsetDateTime,
) -> Result<String> {
    let name = resolve_name_with(env, config, role).map_err(identity_error)?;
    let email = resolve_email_with(env, config, role).map_err(identity_error)?;
    let date = env.var(date_var);
    Ok(grit_lib::commit::assemble_identity(
        &name,
        &email,
        date.as_deref(),
        now,
    ))
}

/// Reflog identity for the committer (`Name <email>`), failing when unset like [`identity`].
pub fn committer_reflog_identity(env: &Environment, config: &ConfigSet) -> Result<String> {
    let name = resolve_name_with(env, config, IdentRole::Committer).map_err(identity_error)?;
    let email = resolve_email_with(env, config, IdentRole::Committer).map_err(identity_error)?;
    Ok(format!("{name} <{email}>"))
}

/// A best-effort committer identity for reflog entries: never fails, even when
/// no identity is configured.
pub fn reflog_identity(env: &Environment, config: &ConfigSet, now: OffsetDateTime) -> String {
    let (name, email) = resolve_loose_committer_parts_with(env, config);
    grit_lib::commit::assemble_identity(&name, &email, None, now)
}

fn identity_error(err: IdentityError) -> anyhow::Error {
    let setup = "\n\nTell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\"";
    match err {
        IdentityError::MissingConfiguredEmail { role }
        | IdentityError::AutoDetectionDisabled { role } => anyhow::anyhow!(
            "{}\n\nunable to auto-detect email address{setup}",
            role.missing_email_hint()
        ),
        other => anyhow::anyhow!("{other}{setup}"),
    }
}
