//! Single chain walk that maps library and wrapped failures to CLI exit codes,
//! human text, hints, and structured `--json` payloads.

use std::error::Error as StdError;

use anyhow::Error;
use grit_lib::command_runner::hooks::HookError;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ident_resolve::IdentityError;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error as LibError;
use grit_lib::ident::Signature;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::BranchError;
use grit_lib::porcelain::paths::BlockedRemoval;
use grit_lib::porcelain::pull::PullError;
use grit_lib::refs::store::RefStoreError;
use grit_lib::remote::clone::CloneError;
use grit_lib::remote::RemoteError;
use grit_lib::revwalk::rev_parse_error::{AmbiguousChangeIdHint, RevParseError};
use grit_lib::state::InProgressOperation;
use time::OffsetDateTime;

use crate::error_kind::ErrorKind;
use crate::exit_status::ExitStatus;
use crate::json_error::operation_in_progress_for_repo;
use crate::json_error::{
    hash_collision, lock_held, operation_in_progress, would_overwrite_error, StructuredCliError,
    StructuredJsonError,
};
use crate::ui::PathDisplayOptions;

/// Fully classified CLI failure (exit code, human text, optional JSON payload).
#[derive(Debug, Clone)]
pub struct ClassifiedError {
    /// Machine-readable category when this failure is structured for `--json`.
    #[allow(dead_code)]
    pub kind: Option<ErrorKind>,
    /// Process exit status for this failure.
    #[allow(dead_code)]
    pub exit: ExitStatus,
    /// Human-readable summary (also used as JSON `error` when structured).
    pub human: String,
    /// Next-step hint, when one is known.
    #[allow(dead_code)]
    pub hint: Option<String>,
    /// Structured `--json` fields, when this failure is typed for scripts.
    pub payload: Option<StructuredJsonError>,
}

impl ClassifiedError {
    /// Wrap as `anyhow` with [`StructuredCliError`] when a JSON payload is present.
    #[must_use]
    pub fn into_anyhow(self) -> Error {
        let Some(payload) = self.payload else {
            return Error::msg(self.human);
        };
        Error::new(StructuredCliError {
            payload,
            human: self.human,
        })
    }

    fn structured(
        kind: ErrorKind,
        human: String,
        hint: Option<String>,
        payload: StructuredJsonError,
    ) -> Self {
        Self {
            kind: Some(kind),
            exit: ExitStatus::for_structured_kind(kind),
            human: human.clone(),
            hint: hint.clone(),
            payload: Some(payload),
        }
    }

    fn from_structured_cli(err: &StructuredCliError) -> Self {
        let kind = err.payload.kind;
        Self {
            kind: Some(kind),
            exit: ExitStatus::for_structured_kind(kind),
            human: err.human.clone(),
            hint: err.payload.hint.clone(),
            payload: Some(err.payload.clone()),
        }
    }
}

fn operation_in_progress_human(action: &str, kind: InProgressOperation) -> String {
    let description = kind.description();
    let article = if description
        .chars()
        .next()
        .is_some_and(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'))
    {
        "an"
    } else {
        "a"
    };
    format!("cannot {action} while {article} {description} is in progress")
}

fn format_ambiguous_change_id(spec: &str, candidates: &[AmbiguousChangeIdHint]) -> String {
    let mut msg = format!("ambiguous change id '{spec}'");
    for hint in candidates {
        let short_id = hint.change_id.chars().take(8).collect::<String>();
        msg.push_str(&format!(
            "\n  {} {} {}",
            hint.oid_hex, short_id, hint.subject
        ));
    }
    msg
}

/// Human-readable text for a bare [`LibError`] (same rules as the legacy CLI mapper).
#[must_use]
pub fn human_message_for_lib(err: &LibError) -> String {
    match err {
        LibError::RevParse(RevParseError::AmbiguousChangeId { prefix, candidates }) => {
            format_ambiguous_change_id(prefix, candidates)
        }
        LibError::RevParse(RevParseError::InvalidObjectName { name }) => {
            format!("revision '{name}' not found")
        }
        LibError::RevParse(RevParseError::AmbiguousArgument { spec }) => {
            format!("revision '{spec}' not found")
        }
        LibError::RevParse(RevParseError::ReflogDateUnparseable { date }) => {
            format!("invalid reflog date '{date}'")
        }
        LibError::ObjectNotFound(spec) => format!("revision '{spec}' not found"),
        LibError::HashCollision { oid } => format!(
            "refusing to store content that triggers an SHA-1 hash collision (digest {})",
            oid.to_hex()
        ),
        LibError::LockHeld(lock) => lock_held(&lock.path, lock.holder).to_string(),
        LibError::RefStore(RefStoreError::NotFound { name }) => {
            match name.strip_prefix("refs/heads/") {
                Some(short) => format!("no branch named '{short}'"),
                None => format!("ref not found: {name}"),
            }
        }
        LibError::InvalidPathComponent { path, reason } => {
            format!("cannot use path '{path}': {reason}")
        }
        LibError::PathspecIgnored { paths } => pathspec_ignored_human(paths),
        LibError::UnmergedConflictMarkers { paths } => stage_conflict_markers_human(paths),
        LibError::Hook(hook) => {
            let mut message = hook.to_string();
            let mut cause = StdError::source(hook);
            while let Some(c) = cause {
                message.push_str(": ");
                message.push_str(&c.to_string());
                cause = c.source();
            }
            message
        }
        LibError::RemoveBlocked { paths } => remove_blocked_human(paths, false, None),
        LibError::OperationInProgress(kind) => operation_in_progress_human("proceed", *kind),
        other => other.git_stderr_message(),
    }
}

/// Format a failed `-c KEY=VALUE` parse for clap's value parser.
#[must_use]
pub fn format_config_override_parse_error(err: LibError) -> String {
    classify(&Error::new(err)).human
}

/// `grit config --unset` when the key is not present in the chosen config file.
#[must_use]
pub fn config_key_not_set_error(key: &str) -> Error {
    Error::msg(format!("key '{key}' is not set"))
}

/// Map unset failures from config write helpers.
#[must_use]
pub fn map_config_unset_error(key: &str, err: LibError) -> Error {
    match err {
        LibError::Config(grit_lib::error::ConfigError::KeyNotSet { .. }) => {
            config_key_not_set_error(key)
        }
        other => Error::new(other),
    }
}

/// Rebase upstream resolution when `@{upstream}` is used but the branch has no upstream.
#[must_use]
pub fn rebase_no_upstream_error(branch: &str) -> Error {
    Error::msg(format!(
        "no upstream configured for branch '{branch}'\n\
         Set one with `grit branch -u <remote>/<branch>`"
    ))
}

/// Resolve a revision for rebase upstream, mapping missing upstream when using the default spec.
pub fn resolve_rebase_upstream_revision(
    spec: &str,
    use_default_upstream: bool,
    resolve: Result<ObjectId, LibError>,
) -> Result<ObjectId, Error> {
    match resolve {
        Ok(oid) => Ok(oid),
        Err(LibError::RevParse(RevParseError::NoUpstream { branch })) if use_default_upstream => {
            Err(rebase_no_upstream_error(&branch))
        }
        Err(other) => {
            Err(Error::new(other).context(format!("could not resolve upstream '{spec}'")))
        }
    }
}

/// Map a library error to `anyhow`, using structured JSON for typed failures.
#[must_use]
pub fn map_lib_error(err: LibError) -> Error {
    match err {
        LibError::RevisionNotFound(spec) => crate::json_error::revision_not_found(&spec),
        LibError::UnbornHead => crate::unborn_errors::unborn_head_from_cwd()
            .unwrap_or_else(|| crate::unborn_errors::unborn_head_error(None)),
        other => {
            let wrapped = Error::new(other);
            let classified = classify(&wrapped);
            if classified.payload.is_some() {
                classified.into_anyhow()
            } else {
                wrapped
            }
        }
    }
}

const IDENTITY_SETUP_HINT: &str = "Tell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\"";

/// Map [`IdentityError`] to a structured CLI error (`IdentityUnknown`).
#[must_use]
pub fn identity_error(err: IdentityError) -> Error {
    let (short, extra) = match &err {
        IdentityError::MissingConfiguredEmail { role }
        | IdentityError::AutoDetectionDisabled { role } => (
            role.missing_email_hint().to_owned(),
            Some("\n\nunable to auto-detect email address".to_owned()),
        ),
        other => (other.to_string(), None),
    };
    let human = match extra {
        Some(extra) => format!("{short}{extra}\n\n{IDENTITY_SETUP_HINT}"),
        None => format!("{short}\n\n{IDENTITY_SETUP_HINT}"),
    };
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: Some(IDENTITY_SETUP_HINT.to_owned()),
            ..StructuredJsonError::new(short, ErrorKind::IdentityUnknown)
        },
        human,
    })
}

/// Human text for [`LibError::PathspecIgnored`] (explicit `grit add` of ignored paths).
#[must_use]
pub fn pathspec_ignored_human(paths: &[String]) -> String {
    let mut msg = String::from("the following paths are ignored by one of your ignore files:\n");
    for path in paths {
        msg.push_str("    ");
        msg.push_str(path);
        msg.push('\n');
    }
    msg.push_str("Use --force if you really want to add them.");
    msg
}

/// Human text when staging refuses paths that still contain conflict markers.
#[must_use]
pub fn stage_conflict_markers_human(paths: &[String]) -> String {
    let mut msg = String::new();
    for path in paths {
        msg.push_str(path);
        msg.push_str(": conflict markers remain; not staged\n");
    }
    if !msg.is_empty() {
        msg.pop();
    }
    msg
}

/// Map [`RevParseError`] from `grit resolve` ref resolution.
#[must_use]
pub fn map_rev_parse_ref_error(spec: &str, err: RevParseError) -> Error {
    if matches!(
        err,
        RevParseError::AmbiguousChangeId { .. }
            | RevParseError::InvalidObjectName { .. }
            | RevParseError::AmbiguousArgument { .. }
            | RevParseError::ReflogDateUnparseable { .. }
    ) {
        Error::msg(human_message_for_lib(&LibError::RevParse(err)))
    } else {
        Error::msg(format!("could not resolve ref '{spec}': {err}"))
    }
}

/// Map an error from expanding commit arguments (`grit pick` / `grit revert`).
#[must_use]
pub fn map_commit_spec_error(err: LibError) -> Error {
    match err {
        LibError::RevParse(RevParseError::InvalidObjectName { name }) => {
            crate::json_error::structured_revision_not_found(format!("revision '{name}' not found"))
        }
        LibError::RevParse(RevParseError::AmbiguousArgument { spec }) => {
            crate::json_error::structured_revision_not_found(format!("revision '{spec}' not found"))
        }
        LibError::ObjectNotFound(spec) => {
            crate::json_error::structured_revision_not_found(format!("revision '{spec}' not found"))
        }
        LibError::RevParse(RevParseError::AmbiguousChangeId { prefix, candidates }) => {
            Error::msg(format_ambiguous_change_id(&prefix, &candidates))
        }
        LibError::RevParse(RevParseError::ReflogDateUnparseable { date }) => {
            Error::msg(format!("invalid reflog date '{date}'"))
        }
        LibError::InvalidRef(msg) => Error::msg(msg),
        other => map_lib_error(other),
    }
}

/// Map a library error from `grit switch` (includes in-progress operation hints from repo state).
#[must_use]
pub fn map_switch_lib_error(repo: &grit_lib::repo::Repository, err: LibError) -> Error {
    if let LibError::OperationInProgress(kind) = err {
        return operation_in_progress_for_repo(repo, "switch branches", kind);
    }
    map_lib_error(err)
}

/// Map [`PullError::Repository`] failures during `grit pull`.
#[must_use]
pub fn map_pull_repository_error(repo: &grit_lib::repo::Repository, err: PullError) -> Error {
    match err {
        PullError::Remote(remote) => map_remote_error(remote),
        PullError::Repository(hook @ LibError::Hook(_)) => {
            if repo.git_dir().join("MERGE_HEAD").is_file() {
                crate::json_error::hook_failed_with_hint(
                    hook,
                    "the merge is still in progress: run `grit merge --continue` to commit it, or `grit merge --abort` to cancel; autostashed edits come back either way",
                )
            } else {
                Error::new(hook)
            }
        }
        PullError::Repository(LibError::OperationInProgress(kind)) => {
            operation_in_progress_for_repo(repo, "pull", kind)
        }
        PullError::Repository(lib) => map_lib_error(lib),
        rest => Error::new(rest),
    }
}

/// Map errors while building [`PullOptions`] for `grit pull`.
#[must_use]
pub fn map_pull_options_error(repo: &grit_lib::repo::Repository, err: PullError) -> Error {
    match err {
        PullError::Repository(LibError::OperationInProgress(kind)) => {
            operation_in_progress_for_repo(repo, "pull", kind)
        }
        other => Error::new(other),
    }
}

/// Map [`remove_paths`](grit_lib::porcelain::paths::remove_paths) failures for `grit rm`.
#[must_use]
pub fn map_remove_paths_error(
    err: LibError,
    cached: bool,
    path_display: Option<&PathDisplayOptions>,
) -> Error {
    match err {
        LibError::RemoveBlocked { paths } => {
            Error::msg(remove_blocked_human(&paths, cached, path_display))
        }
        other => map_lib_error(other),
    }
}

fn remove_blocked_human(
    paths: &[BlockedRemoval],
    cached: bool,
    path_display: Option<&PathDisplayOptions>,
) -> String {
    let format_path = |p: &str| {
        path_display
            .map(|opts| opts.format_path_for_human(p))
            .unwrap_or_else(|| p.to_owned())
    };
    let mut msg = String::from("not removed, these files have content that would be lost:\n");
    for blocked in paths {
        msg.push_str(&format!(
            "    {} ({})\n",
            format_path(&blocked.path),
            blocked.reason
        ));
    }
    let cached_would_help = !cached
        && paths
            .iter()
            .all(|blocked| blocked.reason.allowed_with_cached());
    msg.push_str(if cached_would_help {
        "(use --cached to stop tracking them and keep the files, or --force to remove them anyway)"
    } else {
        "(use --force to remove them anyway)"
    });
    msg
}

/// Append would-overwrite path names when reporting integrate follow-up failures.
#[must_use]
pub fn integrate_failure_text(err: &Error) -> String {
    let text = format!("{err:#}");
    let paths = err.chain().find_map(|cause| {
        let lib = match cause.downcast_ref::<PullError>() {
            Some(PullError::Repository(lib)) => Some(lib),
            _ => cause.downcast_ref::<LibError>(),
        };
        match lib {
            Some(LibError::WouldOverwrite { paths, .. }) if !paths.is_empty() => {
                Some(paths.join(", "))
            }
            _ => None,
        }
    });
    match paths {
        Some(paths) => format!("{text}: {paths}"),
        None => text,
    }
}

/// Map repository identity resolution failures to structured CLI errors.
#[must_use]
pub fn map_repo_identity_error(err: LibError) -> Error {
    match err {
        LibError::Identity(identity) => identity_error(identity),
        other => Error::new(other),
    }
}

/// Committer for reflog entries when configured identity is missing.
pub fn committer_reflog_signature(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    now: OffsetDateTime,
) -> Result<Signature, Error> {
    use grit_lib::config::ident_resolve::resolve_loose_committer_identity_with;

    match repo.identity(IdentRole::Committer, now) {
        Ok(sig) => Ok(sig),
        Err(LibError::Identity(_)) => {
            let line = resolve_loose_committer_identity_with(repo.environment(), config, now);
            line.parse()
                .map_err(|err| Error::new(LibError::InvalidSignature(err)))
        }
        Err(other) => Err(Error::new(other)),
    }
}

/// Map library errors from [`grit_lib::porcelain::add::stage`] to structured CLI errors.
#[must_use]
pub fn map_stage_lib_error(err: LibError) -> Error {
    match err {
        LibError::EmbeddedRepoWithoutCommit { path } => {
            let human =
                format!("cannot stage '{path}': embedded git repository has no commit checked out");
            Error::new(StructuredCliError {
                payload: StructuredJsonError {
                    paths: Some(vec![path.clone()]),
                    ..StructuredJsonError::new(human.clone(), ErrorKind::EmbeddedRepoWithoutCommit)
                },
                human,
            })
        }
        other => crate::json_error::map_lib_error(other),
    }
}

/// Walk `err`'s chain once and classify it for exit codes and output.
#[must_use]
pub fn classify(err: &Error) -> ClassifiedError {
    if let Some(structured) = err.downcast_ref::<StructuredCliError>() {
        return ClassifiedError::from_structured_cli(structured);
    }

    let mut first_guided: Option<Error> = None;
    let mut first_lock: Option<ClassifiedError> = None;
    let mut first_hook: Option<StructuredJsonError> = None;
    let mut first_hash: Option<ObjectId> = None;
    let mut first_overwrite: Option<ClassifiedError> = None;
    let mut first_revision: Option<StructuredJsonError> = None;
    let mut remote_unreadable = false;
    let mut first_lib: Option<&LibError> = None;

    for cause in err.chain() {
        if let Some(structured) = cause.downcast_ref::<StructuredCliError>() {
            return ClassifiedError::from_structured_cli(structured);
        }
        if first_lib.is_none() {
            first_lib = cause.downcast_ref::<LibError>();
        }
        if first_guided.is_none() {
            first_guided = guide_cause(cause);
        }
        if first_lock.is_none() {
            first_lock = lock_classified_from_cause(cause);
        }
        if first_hook.is_none() {
            first_hook = hook_payload_from_cause(cause);
        }
        if first_hash.is_none() {
            first_hash = hash_oid_from_cause(cause);
        }
        if first_overwrite.is_none() {
            first_overwrite = would_overwrite_classified_from_cause(cause);
        }
        if first_revision.is_none() {
            first_revision = revision_payload_from_cause(cause);
        }
        if !remote_unreadable {
            remote_unreadable = cause_is_remote_unreadable(cause);
        }
    }

    if let Some(guided) = first_guided {
        return classified_from_structured_error(&guided);
    }
    if let Some(lock) = first_lock {
        return lock;
    }
    if let Some(payload) = first_hook {
        let human = payload.error.clone();
        return ClassifiedError::structured(payload.kind, human, payload.hint.clone(), payload);
    }
    if let Some(oid) = first_hash {
        return classified_from_structured_error(&hash_collision(oid));
    }
    if let Some(overwrite) = first_overwrite {
        return overwrite;
    }
    if let Some(payload) = first_revision {
        let human = payload.error.clone();
        return ClassifiedError::structured(payload.kind, human, payload.hint.clone(), payload);
    }
    if remote_unreadable {
        let human = first_lib
            .map(human_message_for_lib)
            .unwrap_or_else(|| format!("{err:#}"));
        let payload = StructuredJsonError::new(human.clone(), ErrorKind::RemoteUnreadable);
        return ClassifiedError::structured(ErrorKind::RemoteUnreadable, human, None, payload);
    }

    ClassifiedError {
        kind: None,
        exit: ExitStatus::Failure,
        human: first_lib
            .map(human_message_for_lib)
            .unwrap_or_else(|| format!("{err:#}")),
        hint: None,
        payload: None,
    }
}

fn classified_from_structured_error(err: &Error) -> ClassifiedError {
    err.downcast_ref::<StructuredCliError>()
        .map(ClassifiedError::from_structured_cli)
        .unwrap_or_else(|| ClassifiedError {
            kind: None,
            exit: ExitStatus::Failure,
            human: format!("{err:#}"),
            hint: None,
            payload: None,
        })
}

fn hook_payload_from_cause(cause: &(dyn StdError + 'static)) -> Option<StructuredJsonError> {
    let hook = cause
        .downcast_ref::<LibError>()
        .and_then(|lib| match lib {
            LibError::Hook(h) => Some(h),
            _ => None,
        })
        .or_else(|| cause.downcast_ref::<HookError>())?;
    let HookError::Failed {
        hook_name,
        status,
        output,
    } = hook
    else {
        return None;
    };
    Some(StructuredJsonError {
        hook: Some(crate::json_error::HookFailureJson {
            name: hook_name.clone(),
            status: *status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        ..StructuredJsonError::new(hook.to_string(), ErrorKind::HookFailed)
    })
}

fn lock_classified_from_cause(cause: &(dyn StdError + 'static)) -> Option<ClassifiedError> {
    match cause.downcast_ref::<LibError>()? {
        LibError::LockHeld(lock) => Some(classified_from_structured_error(&lock_held(
            &lock.path,
            lock.holder,
        ))),
        _ => None,
    }
}

fn hash_oid_from_cause(cause: &(dyn StdError + 'static)) -> Option<ObjectId> {
    match cause.downcast_ref::<LibError>()? {
        LibError::HashCollision { oid } => Some(*oid),
        _ => None,
    }
}

fn would_overwrite_classified_from_cause(
    cause: &(dyn StdError + 'static),
) -> Option<ClassifiedError> {
    if let Some(PullError::Repository(LibError::WouldOverwrite {
        reason,
        paths,
        message,
    })) = cause.downcast_ref::<PullError>()
    {
        return Some(classified_from_structured_error(&would_overwrite_error(
            *reason,
            paths.clone(),
            message.clone(),
        )));
    }
    match cause.downcast_ref::<LibError>()? {
        LibError::WouldOverwrite {
            reason,
            paths,
            message,
        } => Some(classified_from_structured_error(&would_overwrite_error(
            *reason,
            paths.clone(),
            message.clone(),
        ))),
        _ => None,
    }
}

fn revision_payload_from_cause(cause: &(dyn StdError + 'static)) -> Option<StructuredJsonError> {
    let lib = cause.downcast_ref::<LibError>()?;
    match lib {
        LibError::RevParse(RevParseError::InvalidObjectName { name }) => {
            Some(StructuredJsonError::new(
                format!("revision '{name}' not found"),
                ErrorKind::RevisionNotFound,
            ))
        }
        LibError::RevParse(RevParseError::AmbiguousArgument { spec }) => {
            Some(StructuredJsonError::new(
                format!("revision '{spec}' not found"),
                ErrorKind::RevisionNotFound,
            ))
        }
        LibError::RevParse(
            RevParseError::PathNotInRevision { .. }
            | RevParseError::PathMissingFromWorktreeAndIndex { .. },
        ) => Some(StructuredJsonError::new(
            lib.to_string(),
            ErrorKind::PathNotFound,
        )),
        _ => None,
    }
}

fn cause_is_remote_unreadable(cause: &(dyn StdError + 'static)) -> bool {
    cause
        .downcast_ref::<LibError>()
        .is_some_and(LibError::is_remote_unreadable)
        || cause
            .downcast_ref::<RemoteError>()
            .is_some_and(RemoteError::is_remote_unreadable)
        || cause
            .downcast_ref::<CloneError>()
            .is_some_and(CloneError::is_remote_unreadable)
        || cause
            .downcast_ref::<PullError>()
            .is_some_and(PullError::is_remote_unreadable)
}

/// Map a [`RemoteError`] for fetch, push, pull, and remote commands.
pub fn map_remote_error(err: RemoteError) -> Error {
    if let Some(guided) = guide_remote(&err) {
        return guided;
    }
    match err {
        RemoteError::Library(LibError::BundleMissingPrerequisites) => {
            Error::new(crate::commands::bundle::MissingPrerequisites {
                commits: Vec::new(),
            })
        }
        RemoteError::Library(LibError::HashCollision { oid }) => {
            crate::json_error::map_lib_error(LibError::HashCollision { oid })
        }
        RemoteError::Library(hook @ LibError::Hook(_)) => Error::new(hook),
        RemoteError::Library(lib) if lib.is_cancelled() => Error::new(lib),
        RemoteError::Library(lib) if lib.is_remote_unreadable() => Error::new(lib),
        RemoteError::Library(lib) => Error::msg(RemoteError::Library(lib).to_string()),
        other => Error::msg(other.to_string()),
    }
}

fn branch_error_message(err: &BranchError) -> String {
    match err {
        BranchError::NotFullyMerged { name, .. } => format!(
            "{err}\nIf you are sure you want to delete it, run 'grit branch --force-delete {name}'"
        ),
        BranchError::AlreadyExists { name } => {
            format!("branch '{name}' already exists (use --force to replace its tip)")
        }
        other => other.to_string(),
    }
}

/// Map a library [`LibError`] from branch commands to a structured CLI error.
pub fn map_branch_error(err: LibError) -> Error {
    match err {
        LibError::Branch(branch_err) => {
            let human = branch_error_message(&branch_err);
            Error::new(StructuredCliError {
                payload: StructuredJsonError::new(
                    human.clone(),
                    ErrorKind::from_branch_error(&branch_err),
                ),
                human,
            })
        }
        LibError::BranchCheckedOutInWorktree { worktree_path, .. } => {
            let worktree = grit_lib::config::git_path::path_for_error_message(
                std::path::Path::new(&worktree_path),
            );
            let human = format!("branch is checked out in '{worktree}'");
            Error::new(StructuredCliError {
                payload: StructuredJsonError::new(human.clone(), ErrorKind::BranchCheckedOut),
                human,
            })
        }
        other => crate::json_error::map_lib_error(other),
    }
}

use grit_lib::error::{AuthFailure, OverwriteReason, RemoteUnreadableReason};
use grit_lib::porcelain::merge::ContinueMergeError;
use grit_lib::remote::github_auth::is_github_https_url;
use grit_lib::remote::push_plan::PushPlanError;
use grit_lib::remote::DEFAULT_REMOTE;
use grit_lib::revwalk::history::RebaseSessionError;

use crate::usage_hints;

/// Paths shown in human messages and hints before the rest are summarized as a count.
const MAX_LISTED_PATHS: usize = 10;

/// A structured error with a hint for the first typed failure in `err`'s chain that has a
/// known next step, or `None` when there is nothing to add.
#[allow(dead_code)]
pub fn guided_error(err: &Error) -> Option<Error> {
    err.chain().find_map(guide_cause)
}

/// [`guided_error`] for a [`RemoteError`] that has not been wrapped in `anyhow` yet.
pub fn guide_remote(err: &RemoteError) -> Option<Error> {
    match err {
        RemoteError::UnknownRemote(name) => Some(remote_not_found(name)),
        RemoteError::NoUrl { remote: Some(name) } => Some(guided(
            ErrorKind::RemoteNoUrl,
            err.to_string(),
            Some(format!("set it with `grit remote set-url {name} <url>`")),
            None,
        )),
        RemoteError::Library(lib) => guide_lib(lib),
        _ => None,
    }
}

/// The hint for an [`LibError::WouldOverwrite`] refusal.
pub fn would_overwrite_hint(reason: OverwriteReason) -> &'static str {
    match reason {
        OverwriteReason::LocalChanges => {
            "commit them with `grit commit`, or set them aside with `grit stash`, then try again"
        }
        OverwriteReason::Untracked => "move or remove those files, then try again",
    }
}

/// `paths` as indented lines (`\n  a\n  b`), capped at [`MAX_LISTED_PATHS`].
pub fn indented_paths(paths: &[String]) -> String {
    let mut out: String = paths
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(|path| format!("\n  {path}"))
        .collect();
    if let Some(rest) = paths.len().checked_sub(MAX_LISTED_PATHS).filter(|n| *n > 0) {
        out.push_str(&format!("\n  … and {rest} more"));
    }
    out
}

fn guide_cause(cause: &(dyn StdError + 'static)) -> Option<Error> {
    if let Some(err) = cause.downcast_ref::<PushPlanError>() {
        return guide_push_plan(err);
    }
    if let Some(err) = cause.downcast_ref::<RemoteError>() {
        return guide_remote(err);
    }
    if let Some(err) = cause.downcast_ref::<PullError>() {
        return guide_pull(err);
    }
    if let Some(err) = cause.downcast_ref::<ContinueMergeError>() {
        return guide_continue_merge(err);
    }
    if let Some(err) = cause.downcast_ref::<RebaseSessionError>() {
        return guide_rebase_session(err);
    }
    if let Some(err) = cause.downcast_ref::<CloneError>() {
        return match err {
            CloneError::Library(lib) => guide_lib(lib),
            CloneError::Remote(remote) => guide_remote(remote),
            CloneError::CheckoutFailed { .. } => Some(clone_checkout_failed(err)),
            _ => None,
        };
    }
    cause.downcast_ref::<LibError>().and_then(guide_lib)
}

fn guide_push_plan(err: &PushPlanError) -> Option<Error> {
    match err {
        PushPlanError::NoRemote => Some(guided(
            ErrorKind::NoRemote,
            err.to_string(),
            Some(format!(
                "add one with `grit remote add {DEFAULT_REMOTE} <url>`, or push to a URL with `grit push <url> <branch>`"
            )),
            None,
        )),
        PushPlanError::UnknownRemote(name) => Some(remote_not_found(name)),
        _ => None,
    }
}

fn guide_pull(err: &PullError) -> Option<Error> {
    match err {
        PullError::NoUpstream { branch } => Some(guided(
            ErrorKind::BranchNoUpstream,
            err.to_string(),
            Some(format!(
                "pull a branch by name with `grit pull {DEFAULT_REMOTE} {branch}`, or publish this one and track it with `grit push -u {DEFAULT_REMOTE} {branch}`"
            )),
            None,
        )),
        PullError::Remote(remote) => guide_remote(remote),
        PullError::Repository(lib) => guide_lib(lib),
        _ => None,
    }
}

fn guide_continue_merge(err: &ContinueMergeError) -> Option<Error> {
    match err {
        ContinueMergeError::NoMergeInProgress => Some(no_operation_in_progress(err.to_string())),
        ContinueMergeError::UnmergedPaths { paths } => Some(guided(
            ErrorKind::UnmergedPaths,
            err.to_string(),
            Some(format!(
                "resolve the conflict markers in {}, then run `grit merge --continue` (or `grit merge --abort` to start over)",
                listed_paths(paths)
            )),
            Some(paths.clone()),
        )),
        ContinueMergeError::ResolvedNotStaged { paths } => Some(guided(
            ErrorKind::UnmergedPaths,
            err.to_string(),
            Some(format!(
                "stage them with `grit add {}`, then run `grit merge --continue`",
                shell_words(paths)
            )),
            Some(paths.clone()),
        )),
        ContinueMergeError::Repository(lib) => guide_lib(lib),
        _ => None,
    }
}

fn guide_rebase_session(err: &RebaseSessionError) -> Option<Error> {
    match err {
        RebaseSessionError::NoRebaseInProgress => {
            Some(no_operation_in_progress(err.to_string()))
        }
        RebaseSessionError::UnmergedPaths => Some(guided(
            ErrorKind::UnmergedPaths,
            err.to_string(),
            Some(
                "resolve conflicts and stage with `grit add`, then run `grit rebase --continue` (or `grit rebase --abort` to start over)".to_owned(),
            ),
            None,
        )),
        _ => None,
    }
}

fn guide_lib(err: &LibError) -> Option<Error> {
    match err {
        LibError::NotARepository(_) => Some(guided(
            ErrorKind::NotARepository,
            err.to_string(),
            None,
            None,
        )),
        LibError::PathspecNoMatch { .. } => Some(guided(
            ErrorKind::PathspecNoMatch,
            err.to_string(),
            None,
            None,
        )),
        LibError::EmptyCommitMessage => Some(guided(
            ErrorKind::EmptyCommitMessage,
            err.to_string(),
            None,
            None,
        )),
        LibError::Identity(identity_err) => Some(identity_error(identity_err.clone())),
        LibError::Tag(tag_err) => match tag_err {
            grit_lib::porcelain::tag::TagError::AlreadyExists { name } => {
                let human = format!("tag '{name}' already exists (use --force to replace)");
                Some(guided(ErrorKind::TagAlreadyExists, human, None, None))
            }
            grit_lib::porcelain::tag::TagError::NotFound { name } => {
                let human = format!("no tag named '{name}'");
                Some(guided(ErrorKind::TagNotFound, human, None, None))
            }
            _ => None,
        },
        LibError::EmbeddedRepoWithoutCommit { .. } => Some(guided(
            ErrorKind::EmbeddedRepoWithoutCommit,
            err.to_string(),
            None,
            None,
        )),
        LibError::IndexUnmerged => Some(guided(
            ErrorKind::UnmergedPaths,
            "cannot commit: unresolved merge conflicts remain — resolve each conflicted path (grit add or grit rm), then try again".to_owned(),
            Some(
                "resolve conflicts with `grit add` or `grit rm`, then commit, or finish with `grit merge --continue` / `grit merge --abort`".to_owned(),
            ),
            None,
        )),
        LibError::NoMergeInProgress | LibError::NoReplayInProgress => {
            Some(no_operation_in_progress(err.to_string()))
        }
        LibError::OperationInProgress(kind) => {
            Some(operation_in_progress("proceed", *kind, None))
        }
        LibError::RemoveBlocked { paths } => {
            Some(Error::msg(remove_blocked_human(paths, false, None)))
        }
        LibError::StashNotFound { .. } => Some(stash_not_found(err)),
        LibError::RevParse(
            RevParseError::PathNotInRevision { .. }
            | RevParseError::PathMissingFromWorktreeAndIndex { .. },
        ) => Some(guided(
            ErrorKind::PathNotFound,
            err.to_string(),
            None,
            None,
        )),
        LibError::Auth { url, reason } => Some(auth_error(err, url, reason)),
        LibError::RemoteUnreadable {
            reason:
                RemoteUnreadableReason::RepositoryNotFound
                | RemoteUnreadableReason::HttpStatus { status: 404 }
                | RemoteUnreadableReason::NotGitServer { .. },
            ..
        } => Some(guided(
            ErrorKind::RemoteUnreadable,
            err.to_string(),
            Some("check the URL, and that you have access to the repository".to_owned()),
            None,
        )),
        LibError::WouldOverwrite {
            reason,
            paths,
            message,
        } => Some(would_overwrite_error(*reason, paths.clone(), message.clone())),
        LibError::DirtyWorktree { paths, .. } => Some(Error::new(StructuredCliError {
            human: format!("{err}{}", indented_paths(paths)),
            payload: StructuredJsonError {
                hint: Some(would_overwrite_hint(OverwriteReason::LocalChanges).to_owned()),
                paths: Some(paths.clone()),
                ..StructuredJsonError::new(err.to_string(), ErrorKind::DirtyWorktree)
            },
        })),
        LibError::RestorePathUnmerged { paths } => {
            let words = shell_words(paths);
            Some(guided(
                ErrorKind::UnmergedPaths,
                err.to_string(),
                Some(format!(
                    "take one side with `grit restore --ours {words}` or `grit restore --theirs {words}`, bring the conflict markers back with `grit restore --merge {words}`, or edit out the markers and run `grit add {words}`"
                )),
                Some(paths.clone()),
            ))
        }
        LibError::UnmergedConflictMarkers { paths } => Some(Error::new(StructuredCliError {
            human: stage_conflict_markers_human(paths),
            payload: StructuredJsonError {
                hint: Some(format!(
                    "edit out the conflict markers in {}, then try again",
                    listed_paths(paths)
                )),
                paths: Some(paths.clone()),
                ..StructuredJsonError::new(err.to_string(), ErrorKind::UnmergedPaths)
            },
        })),
        LibError::WorktreePathIo { path, .. } => Some(guided(
            ErrorKind::WorktreePathIo,
            err.to_string(),
            err.is_worktree_file_in_use()
                .then(|| "close programs that may have the file open, then try again".to_owned()),
            Some(vec![path.clone()]),
        )),
        LibError::CheckoutRollbackIncomplete { unrestored, .. } => Some(Error::new(
            StructuredCliError {
                human: format!("{err}{}", indented_paths(unrestored)),
                payload: StructuredJsonError {
                    hint: Some(
                        "HEAD and the index did not move; `grit status` shows the listed paths, and `grit reset --hard` plus `grit clean -f` put them back".to_owned(),
                    ),
                    paths: Some(unrestored.clone()),
                    ..StructuredJsonError::new(err.to_string(), ErrorKind::CheckoutIncomplete)
                },
            },
        )),
        LibError::GitDirPathIo { path, .. } => Some(guided(
            ErrorKind::GitDirPathIo,
            err.to_string(),
            err.is_git_dir_file_in_use()
                .then(|| {
                    "another thread or process is updating the same git metadata; retry shortly"
                        .to_owned()
                })
                .or_else(|| Some(git_dir_path_io_hint(path))),
            Some(vec![path.clone()]),
        )),
        LibError::UnbornHead => Some(
            crate::unborn_errors::unborn_head_from_cwd()
                .unwrap_or_else(|| crate::unborn_errors::unborn_head_error(None)),
        ),
        LibError::Branch(branch_err) => guide_branch(branch_err),
        LibError::MoveDestinationExists { destination, .. } => Some(guided(
            ErrorKind::BranchError,
            err.to_string(),
            Some(format!(
                "remove or rename '{destination}' first, or run `grit mv --force` to replace it"
            )),
            Some(vec![destination.clone()]),
        )),
        LibError::MoveSourceNotTracked { src } => Some(guided(
            ErrorKind::BranchError,
            err.to_string(),
            Some(format!("track '{src}' with `grit add` before moving it")),
            Some(vec![src.clone()]),
        )),
        _ => None,
    }
}

/// A clone kept after its checkout failed: say so, and how to finish the checkout.
fn clone_checkout_failed(err: &CloneError) -> Error {
    let CloneError::CheckoutFailed {
        path,
        commit,
        source,
    } = err
    else {
        return guided(ErrorKind::CloneCheckoutFailed, err.to_string(), None, None);
    };
    let dir = path.display();
    let human = format!("the clone into '{dir}' succeeded, but its checkout failed: {source}");
    let hint = format!(
        "the repository was kept; run `grit reset --hard` in '{dir}' to finish the checkout{}",
        if err.missing_promised_object().is_some() {
            " once its promisor remotes can supply the missing objects"
        } else {
            ""
        }
    );
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: Some(hint),
            clone_checkout: Some(crate::json_error::CloneCheckoutJson {
                path: path.display().to_string(),
                commit: commit.to_hex(),
                missing_object: err.missing_promised_object().map(|oid| oid.to_hex()),
                failed_remotes: err
                    .failed_promisor_remotes()
                    .iter()
                    .map(|(remote, e)| crate::json_error::PromisorRemoteFailureJson {
                        remote: remote.clone(),
                        error: e.to_string(),
                    })
                    .collect(),
            }),
            ..StructuredJsonError::new(human.clone(), ErrorKind::CloneCheckoutFailed)
        },
        human,
    })
}

fn guide_branch(err: &BranchError) -> Option<Error> {
    match err {
        BranchError::NoPreviousBranch => Some(guided(
            ErrorKind::NoPreviousBranch,
            err.to_string(),
            None,
            None,
        )),
        BranchError::NotFound { name } => {
            if let Some(hint) = usage_hints::checkout_restore_hint(name) {
                Some(guided(
                    ErrorKind::BranchNotFound,
                    format!("no branch named '{name}'"),
                    Some(hint),
                    None,
                ))
            } else {
                let hint = (!looks_like_rev_syntax(name)).then(|| {
                    format!(
                        "`grit branch` lists your branches; create this one with `grit switch -c {name}`"
                    )
                });
                Some(guided(
                    ErrorKind::BranchNotFound,
                    format!("no branch named '{name}'"),
                    hint,
                    None,
                ))
            }
        }
        BranchError::AlreadyExists { .. } => Some(guided(
            ErrorKind::BranchAlreadyExists,
            err.to_string(),
            None,
            None,
        )),
        BranchError::InvalidName { .. } => Some(guided(
            ErrorKind::BranchInvalidName,
            err.to_string(),
            None,
            None,
        )),
        BranchError::NoCommits => Some(guided(
            ErrorKind::BranchNoCommits,
            err.to_string(),
            None,
            None,
        )),
        _ => None,
    }
}

/// True when `name` is revision syntax, not a literal branch name (do not suggest `switch -c`).
fn looks_like_rev_syntax(name: &str) -> bool {
    name.starts_with("@{")
        || name.contains('^')
        || name.contains('~')
        || name.contains(':')
        || name.contains('*')
        || name.contains('?')
        || (name.starts_with('-') && name != "-")
}

fn auth_error(err: &LibError, url: &str, reason: &AuthFailure) -> Error {
    let github = is_github_https_url(url);
    let store = format!("`grit auth {url} --username <user> --token <token>`");
    let (kind, hint) = match reason {
        AuthFailure::Rejected if github => (
            ErrorKind::AuthRejected,
            Some("your saved GitHub token may have expired; sign in again with `grit auth`".to_owned()),
        ),
        AuthFailure::Rejected => (
            ErrorKind::AuthRejected,
            Some(format!(
                "update the credentials your credential helper stores for this host (for example with {store}), then try again"
            )),
        ),
        AuthFailure::UnsupportedScheme { .. } | AuthFailure::UrlNotDescribable => {
            (ErrorKind::AuthRequired, None)
        }
        _ if github => (
            ErrorKind::AuthRequired,
            Some("sign in to GitHub with `grit auth` and try again; if it still fails, check the URL".to_owned()),
        ),
        // Helper entries are commands that can embed secrets, so they are counted, never shown.
        AuthFailure::CredentialsUnavailable { helpers, .. } if !helpers.is_empty() => (
            ErrorKind::AuthRequired,
            Some(format!(
                "{} has no credentials for this host; save them with {store}, or point GIT_ASKPASS at a program that prints them, then try again",
                if helpers.len() == 1 {
                    "your credential helper".to_owned()
                } else {
                    format!("none of your {} credential helpers", helpers.len())
                }
            )),
        ),
        _ => (
            ErrorKind::AuthRequired,
            Some(format!(
                "save a user name and token for this host with {store} (it sets up a credential helper), or point GIT_ASKPASS at a program that prints them, then try again"
            )),
        ),
    };
    guided(kind, err.to_string(), hint, None)
}

fn remote_not_found(name: &str) -> Error {
    guided(
        ErrorKind::RemoteNotFound,
        format!("unknown remote '{name}'"),
        Some(format!(
            "add it with `grit remote add {name} <url>`, or pass a URL or path instead; `grit remote` lists the configured remotes"
        )),
        None,
    )
}

fn git_dir_path_io_hint(path: &str) -> String {
    if path.starts_with(".git/") || path == ".git" {
        "check permissions and ownership under .git (for example `ls -l .git`); \
         after running as root, fix ownership with `chown -R` for your user"
            .to_owned()
    } else {
        "check permissions and ownership in the repository directory (for example `ls -l` in the repo root); \
         after running as root, fix ownership with `chown -R` for your user"
            .to_owned()
    }
}

fn no_operation_in_progress(human: String) -> Error {
    guided(ErrorKind::NoOperationInProgress, human, None, None)
}

fn stash_not_found(err: &LibError) -> Error {
    let LibError::StashNotFound { n } = err else {
        unreachable!("stash_not_found called for {:?}", err);
    };
    let (human, hint) = if *n == 0 {
        (
            "no stash entries".to_owned(),
            Some("save local changes with `grit stash push`".to_owned()),
        )
    } else {
        (
            format!("no stash entry at index {n}"),
            Some("run `grit stash list` to see valid indexes".to_owned()),
        )
    };
    guided(ErrorKind::StashNotFound, human, hint, None)
}

fn guided(
    kind: ErrorKind,
    human: String,
    hint: Option<String>,
    paths: Option<Vec<String>>,
) -> Error {
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint,
            paths,
            ..StructuredJsonError::new(human.clone(), kind)
        },
        human,
    })
}

/// `a, b and c`, capped at [`MAX_LISTED_PATHS`].
fn listed_paths(paths: &[String]) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(String::as_str)
        .collect();
    let rest = paths.len() - shown.len();
    match (shown.split_last(), rest) {
        (None, _) => String::new(),
        (Some((last, [])), 0) => (*last).to_owned(),
        (Some((last, init)), 0) => format!("{} and {last}", init.join(", ")),
        (Some(_), rest) => format!("{} and {rest} more", shown.join(", ")),
    }
}

/// `paths` as shell words, single-quoting any that need it.
fn shell_words(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| {
            let plain = !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-/+@%,=:".contains(c));
            if plain {
                path.clone()
            } else {
                format!("'{}'", path.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::json_error::structured_json_payload;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn payload(err: Error) -> StructuredJsonError {
        let guided = guided_error(&err).expect("guided");
        structured_json_payload(&guided).expect("structured")
    }

    #[test]
    fn listed_paths_reads_as_prose() {
        assert_eq!(listed_paths(&strings(&["a"])), "a");
        assert_eq!(listed_paths(&strings(&["a", "b", "c"])), "a, b and c");
        let many: Vec<String> = (0..12).map(|i| format!("f{i}")).collect();
        assert!(listed_paths(&many).ends_with("f9 and 2 more"));
    }

    #[test]
    fn shell_words_quote_only_when_needed() {
        assert_eq!(
            shell_words(&strings(&["src/a.rs", "with space", "it's"])),
            r"src/a.rs 'with space' 'it'\''s'"
        );
    }

    #[test]
    fn indented_paths_caps_the_list() {
        assert_eq!(indented_paths(&strings(&["a", "b"])), "\n  a\n  b");
        let many: Vec<String> = (0..11).map(|i| format!("f{i}")).collect();
        assert!(indented_paths(&many).ends_with("\n  f9\n  … and 1 more"));
    }

    #[test]
    fn push_without_remote_suggests_remote_add() {
        let p = payload(Error::new(PushPlanError::NoRemote));
        assert_eq!(p.kind, ErrorKind::NoRemote);
        assert!(p
            .hint
            .expect("hint")
            .contains("grit remote add origin <url>"));
    }

    #[test]
    fn unknown_and_url_less_remotes_name_the_remote() {
        let p = payload(Error::new(RemoteError::UnknownRemote("up".into())));
        assert_eq!(p.kind, ErrorKind::RemoteNotFound);
        assert!(p.hint.expect("hint").contains("grit remote add up <url>"));
        let p = payload(Error::new(RemoteError::NoUrl {
            remote: Some("half".into()),
        }));
        assert_eq!(p.kind, ErrorKind::RemoteNoUrl);
        assert!(p
            .hint
            .expect("hint")
            .contains("grit remote set-url half <url>"));
        assert!(guided_error(&Error::new(RemoteError::NoUrl { remote: None })).is_none());
    }

    #[test]
    fn worktree_path_io_suggests_closing_programs_only_when_in_use() {
        let in_use = payload(Error::new(LibError::WorktreePathIo {
            path: "a.txt".into(),
            operation: grit_lib::error::WorktreePathOperation::Replace,
            source: std::io::Error::from_raw_os_error(libc::EBUSY),
        }));
        assert_eq!(in_use.kind, ErrorKind::WorktreePathIo);
        assert!(in_use.hint.expect("hint").contains("close programs"));
        let denied = payload(Error::new(LibError::WorktreePathIo {
            path: "a.txt".into(),
            operation: grit_lib::error::WorktreePathOperation::Metadata,
            source: std::io::Error::from_raw_os_error(libc::EACCES),
        }));
        assert_eq!(denied.paths, Some(vec!["a.txt".to_owned()]));
        assert!(denied.hint.is_none());
    }

    #[test]
    fn git_dir_path_io_suggests_retry_when_in_use() {
        let in_use = payload(Error::new(LibError::GitDirPathIo {
            path: ".git/index".into(),
            operation: grit_lib::error::GitDirPathOperation::WriteIndex,
            source: std::io::Error::from_raw_os_error(libc::EBUSY as i32),
        }));
        assert!(in_use.hint.expect("hint").contains("retry shortly"));
    }

    #[test]
    fn git_dir_path_io_hint_matches_layout() {
        let worktree = payload(Error::new(LibError::GitDirPathIo {
            path: ".git/objects/pack/tmp".into(),
            operation: grit_lib::error::GitDirPathOperation::GcPack,
            source: std::io::Error::from_raw_os_error(libc::EACCES as i32),
        }));
        assert!(worktree.hint.expect("hint").contains("ls -l .git"));
        let bare = payload(Error::new(LibError::GitDirPathIo {
            path: "objects/pack/tmp_repack_1".into(),
            operation: grit_lib::error::GitDirPathOperation::GcPack,
            source: std::io::Error::from_raw_os_error(libc::EACCES as i32),
        }));
        let bare_hint = bare.hint.expect("hint");
        assert!(
            !bare_hint.contains("ls -l .git"),
            "bare hint should not reference .git"
        );
        assert!(bare_hint.contains("repo root"));
    }

    #[test]
    fn auth_hints_depend_on_host_and_failure() {
        let auth = |url: &str, reason| {
            payload(Error::new(LibError::Auth {
                url: url.to_owned(),
                reason,
            }))
        };
        let unavailable = || AuthFailure::CredentialsUnavailable {
            detail: "none".into(),
            helpers: Vec::new(),
        };
        let p = auth("https://github.com/o/r.git", unavailable());
        assert_eq!(p.kind, ErrorKind::AuthRequired);
        assert!(p.hint.expect("hint").contains("`grit auth`"));
        assert!(!p.error.contains("info/refs"));
        let p = auth("https://git.example.com/o/r.git", unavailable());
        let hint = p.hint.expect("hint");
        assert!(
            hint.contains(
                "`grit auth https://git.example.com/o/r.git --username <user> --token <token>`"
            ),
            "{hint}"
        );
        assert!(hint.contains("GIT_ASKPASS"), "{hint}");
        let configured = |helpers: &[&str]| {
            auth(
                "https://git.example.com/o/r.git",
                AuthFailure::CredentialsUnavailable {
                    detail: "none".into(),
                    helpers: helpers.iter().map(|h| (*h).to_owned()).collect(),
                },
            )
        };
        let secret = "!echo password=literal-secret";
        let two = configured(&["store", secret]).hint.expect("hint");
        assert!(two.contains("none of your 2 credential helpers"), "{two}");
        assert!(
            !two.contains("literal-secret") && !two.contains("store"),
            "{two}"
        );
        let p = configured(&[secret]);
        assert!(!p.error.contains("literal-secret"), "{}", p.error);
        let hint = p.hint.expect("hint");
        assert!(
            hint.contains("your credential helper has no credentials"),
            "{hint}"
        );
        assert!(!hint.contains("literal-secret"), "{hint}");
        assert!(!hint.contains("configure a credential helper"), "{hint}");
        assert!(hint.contains("--username <user> --token <token>"), "{hint}");
        let p = auth("https://git.example.com/o/r.git", AuthFailure::Rejected);
        assert_eq!(p.kind, ErrorKind::AuthRejected);
        let hint = p.hint.expect("hint");
        assert!(
            hint.contains("update the credentials your credential helper stores"),
            "{hint}"
        );
    }

    #[test]
    fn pull_without_upstream_names_the_branch() {
        let p = payload(Error::new(PullError::NoUpstream {
            branch: "main".into(),
        }));
        assert_eq!(p.kind, ErrorKind::BranchNoUpstream);
        let hint = p.hint.expect("hint");
        assert!(hint.contains("grit pull origin main"));
        assert!(hint.contains("grit push -u origin main"));
    }

    #[test]
    fn unmerged_paths_carry_paths_and_next_commands() {
        let p = payload(Error::new(ContinueMergeError::UnmergedPaths {
            paths: strings(&["f.txt"]),
        }));
        assert_eq!(p.kind, ErrorKind::UnmergedPaths);
        assert_eq!(p.paths, Some(strings(&["f.txt"])));
        assert_eq!(p.error, "you still have unmerged paths: f.txt");
        let hint = p.hint.expect("hint");
        assert!(hint.contains("f.txt") && hint.contains("grit merge --continue"));
        let p = payload(Error::new(ContinueMergeError::ResolvedNotStaged {
            paths: strings(&["a b"]),
        }));
        assert!(p.hint.expect("hint").contains("grit add 'a b'"));
    }

    #[test]
    fn restore_of_unmerged_paths_names_each_side() {
        let p = payload(Error::new(LibError::RestorePathUnmerged {
            paths: strings(&["a b"]),
        }));
        assert_eq!(p.kind, ErrorKind::UnmergedPaths);
        assert_eq!(p.paths, Some(strings(&["a b"])));
        assert_eq!(p.error, "path 'a b' is unmerged");
        let hint = p.hint.expect("hint");
        for side in ["--ours", "--theirs", "--merge"] {
            assert!(
                hint.contains(&format!("grit restore {side} 'a b'")),
                "{hint}"
            );
        }
        assert!(hint.contains("grit add 'a b'"), "{hint}");
    }

    #[test]
    fn conflict_markers_refusal_names_paths() {
        let p = payload(Error::new(LibError::UnmergedConflictMarkers {
            paths: strings(&["f"]),
        }));
        assert_eq!(p.kind, ErrorKind::UnmergedPaths);
        assert_eq!(p.paths, Some(strings(&["f"])));
        assert_eq!(p.error, "unmerged paths still have conflict markers");
        assert!(p.hint.expect("hint").contains("conflict markers in f"));
    }

    #[test]
    fn would_overwrite_from_pull_repository_is_guided() {
        let err = Error::new(PullError::Repository(LibError::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths: strings(&["f"]),
            message: "local changes would be overwritten by merge".into(),
        }));
        let p = payload(err);
        assert_eq!(p.kind, ErrorKind::WouldOverwrite);
        assert_eq!(
            p.would_overwrite.as_ref().expect("wo").paths,
            strings(&["f"])
        );
        assert!(p.hint.expect("hint").contains("grit stash"));
    }

    #[test]
    fn dirty_worktree_lists_paths_and_suggests_stash() {
        let err = Error::new(LibError::dirty_worktree(
            grit_lib::error::WorktreeOperation::Switch,
            strings(&["b", "a"]),
        ));
        let guided = guided_error(&err).expect("guided");
        assert!(guided.to_string().ends_with("\n  a\n  b"));
        let p = structured_json_payload(&guided).expect("structured");
        assert_eq!(p.kind, ErrorKind::DirtyWorktree);
        assert_eq!(p.paths, Some(strings(&["a", "b"])));
        assert!(p.hint.expect("hint").contains("grit stash"));
    }

    #[test]
    fn not_a_repository_has_kind() {
        use grit_lib::error::NotARepositoryReason;
        let p = payload(Error::new(LibError::NotARepository(
            NotARepositoryReason::NoRepositoryAt {
                path: "/tmp".into(),
            },
        )));
        assert_eq!(p.kind, ErrorKind::NotARepository);
    }

    #[test]
    fn pathspec_no_match_has_kind() {
        let p = payload(Error::new(LibError::PathspecNoMatch {
            spec: "nosuch".into(),
        }));
        assert_eq!(p.kind, ErrorKind::PathspecNoMatch);
    }

    #[test]
    fn no_operation_in_progress_for_merge_abort() {
        let p = payload(Error::new(LibError::NoMergeInProgress));
        assert_eq!(p.kind, ErrorKind::NoOperationInProgress);
    }

    #[test]
    fn errors_inside_wrappers_are_found() {
        let wrapped = Error::new(CloneError::Remote(RemoteError::Library(
            LibError::remote_unreadable(
                "https://example.com/x.git",
                RemoteUnreadableReason::HttpStatus { status: 404 },
            ),
        )))
        .context("clone failed");
        let p = payload(wrapped);
        assert_eq!(p.kind, ErrorKind::RemoteUnreadable);
        assert!(p.hint.expect("hint").contains("check the URL"));
        let unreachable = Error::new(LibError::remote_unreadable(
            "https://example.com/x.git",
            RemoteUnreadableReason::Transport {
                detail: "refused".into(),
            },
        ));
        assert!(guided_error(&unreachable).is_none());
    }

    #[test]
    fn error_map_snapshot() {
        crate::error_map_contract::assert_error_map_contract();
    }

    #[test]
    fn lib_errors_rendered_in_one_place() {
        crate::error_map_contract::assert_lib_error_match_ratchet();
    }

    #[test]
    fn library_object_not_found_keeps_remote_display() {
        use grit_lib::remote::RemoteError;

        let err = super::map_remote_error(RemoteError::Library(LibError::ObjectNotFound(
            "deadbeef".into(),
        )));
        assert!(err.downcast_ref::<LibError>().is_none());
        assert_eq!(
            crate::cli_messages::human_error_message(&err),
            "object not found: deadbeef",
            "must not remap to revision-not-found wording"
        );
        assert!(crate::json_error::structured_json_payload(&err).is_none());
    }

    #[test]
    fn library_hash_collision_stays_typed_for_json() {
        use grit_lib::remote::RemoteError;

        let oid = ObjectId::from_hex("f92d74e3874587aaf443d1db961d4e26dde13e9c").expect("oid");
        let err = super::map_remote_error(RemoteError::Library(LibError::HashCollision { oid }));
        let payload = crate::json_error::structured_json_payload(&err).expect("structured payload");
        assert_eq!(payload.kind, ErrorKind::HashCollision);
        assert_eq!(payload.oid.as_deref(), Some(oid.to_hex().as_str()));
    }

    /// Library error carried as the `source()` of another error type, so `anyhow` downcasts
    /// cannot see it and only a chain walk finds it.
    #[derive(Debug)]
    struct WrappedLib(LibError);

    impl std::fmt::Display for WrappedLib {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("wrapped operation failed")
        }
    }

    impl std::error::Error for WrappedLib {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    fn wrapped_lock_held(holder: Option<grit_lib::lockfile::LockHolder>) -> (Error, PathBuf) {
        let path = PathBuf::from("/repo/.git/index.lock");
        let err = Error::new(WrappedLib(LibError::LockHeld {
            path: path.clone(),
            holder,
        }))
        .context("staging files");
        (err, path)
    }

    #[test]
    fn nested_lock_held_gets_lock_prose_in_human_message() {
        let (err, path) = wrapped_lock_held(Some(grit_lib::lockfile::LockHolder {
            pid: 42,
            running: false,
        }));
        assert!(err.downcast_ref::<LibError>().is_none());
        let human = crate::cli_messages::human_error_message(&err);
        assert!(
            human.contains(&grit_lib::config::git_path::user_display_path(&path)),
            "{human}"
        );
        assert!(human.contains("stale lock"), "{human}");
        assert!(human.contains("process 42"), "{human}");
    }

    #[test]
    fn context_wrapped_config_error_keeps_config_message() {
        use grit_lib::error::ConfigError;

        let err = Error::new(LibError::Config(ConfigError::BadConfigLineInFile {
            file: ".git/config".into(),
            line: 6,
        }))
        .context("not in a repository");
        let human = crate::cli_messages::human_error_message(&err);
        assert!(human.contains("bad config line"), "{human}");
        assert!(!human.contains("not in a repository"), "{human}");
        assert!(human.starts_with("fatal:"), "{human}");
    }

    #[test]
    fn ensure_structured_rewraps_nested_lock_held() {
        use crate::json_error::{ensure_structured, structured_json_payload, StructuredCliError};

        let (err, path) = wrapped_lock_held(None);
        let structured = ensure_structured(err);
        assert!(structured.downcast_ref::<StructuredCliError>().is_some());
        let human = crate::cli_messages::human_error_message(&structured);
        assert!(human.contains("another process holds"), "{human}");
        let payload = structured_json_payload(&structured).expect("payload");
        assert_eq!(payload.kind, ErrorKind::LockHeld);
        assert_eq!(payload.error, human);
        assert_eq!(
            payload.lock.expect("lock").path,
            grit_lib::config::git_path::user_display_path(&path)
        );
    }
}
