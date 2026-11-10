//! Shared error types for grit-lib.
//!
//! Every fallible operation returns [`enum@Error`]. It has one variant per functional area
//! (repository, objects, index, refs, revisions, config, work tree, diff, commit, merge,
//! history, stash, branches, tags, notes, remotes, hooks) plus the cross-cutting
//! [`Error::Io`], [`Error::LockHeld`] and [`Error::Cancelled`]. Each area enum lists that
//! area's failures with structured fields, so callers match on the area and then on the
//! specific failure. The binary wraps these with `anyhow` for human-readable top-level
//! reporting.
//!
//! Library guide: [typed errors](https://grit-scm.com/docs/library/errors) and [embedding](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::error::{Error, RefError};
//!
//! fn describe(err: &Error) -> &'static str {
//!     match err {
//!         Error::Ref(RefError::UnbornHead) => "no commits yet",
//!         Error::Cancelled => "cancelled",
//!         other => other.area(),
//!     }
//! }
//! assert_eq!(describe(&Error::Ref(RefError::UnbornHead)), "no commits yet");
//! ```

use std::path::PathBuf;

use thiserror::Error;

mod diff;
mod notes;
mod object;
mod refs;
mod revision;
mod summary;

pub use diff::DiffError;
pub use notes::NotesError;
pub use object::{ObjectError, PromisorFetchFailure};
pub use refs::RefError;
pub use revision::RevisionError;
pub use summary::ErrorSummary;

pub use crate::git_dir::GitDirPathOperation;
pub use crate::midx_error::MidxError;
pub use crate::repo::RepositoryError;
pub use crate::worktree::io_error::WorktreePathOperation;

/// Lock file could not be created because another process holds it (or left a stale lock).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "lock file {} already exists",
    crate::git_path::user_display_path(.path.as_path())
)]
pub struct LockHeldError {
    /// Lock file that could not be created.
    pub path: PathBuf,
    /// Owner recorded in the lock's pid sidecar (`core.lockfilePid`), when one was written.
    pub holder: Option<crate::lockfile::LockHolder>,
}

/// Operation stopped because [`CancelToken`](crate::progress::CancelToken) fired before completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation cancelled")
    }
}

/// Which filesystem area a [`PathIoError`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathIoOperation {
    /// Path under the git directory (objects, refs, index, …).
    GitDir(GitDirPathOperation),
    /// Repository-relative worktree path.
    Worktree(WorktreePathOperation),
}

/// Structured path I/O failure shared by worktree and git-directory operations.
#[derive(Debug)]
pub struct PathIoError {
    /// What the operation was doing when I/O failed.
    pub operation: PathIoOperation,
    /// Path that failed, when known.
    pub path: Option<PathBuf>,
    /// Underlying OS error.
    pub source: std::io::Error,
}

/// Copy an [`std::io::Error`] without dropping [`raw_os_error`](std::io::Error::raw_os_error).
fn preserve_io_error(source: &std::io::Error) -> std::io::Error {
    if let Some(raw) = source.raw_os_error() {
        std::io::Error::from_raw_os_error(raw)
    } else {
        std::io::Error::new(source.kind(), source.to_string())
    }
}

impl std::fmt::Display for PathIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PathIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl PathIoError {
    /// Build a worktree-path failure (path is the index- or worktree-relative display path).
    #[must_use]
    pub fn worktree(
        path: impl Into<String>,
        operation: WorktreePathOperation,
        source: std::io::Error,
    ) -> Self {
        Self {
            operation: PathIoOperation::Worktree(operation),
            path: Some(PathBuf::from(path.into())),
            source,
        }
    }

    /// Build a git-directory path failure (`path` is already a display path such as `.git/objects`).
    #[must_use]
    pub fn git_dir(
        path: impl Into<String>,
        operation: GitDirPathOperation,
        source: std::io::Error,
    ) -> Self {
        Self {
            operation: PathIoOperation::GitDir(operation),
            path: Some(PathBuf::from(path.into())),
            source,
        }
    }

    /// Human-readable summary (the display text of the worktree or git-dir path I/O failure).
    #[must_use]
    pub fn message(&self) -> String {
        let path = self
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        match self.operation {
            PathIoOperation::Worktree(op) => {
                crate::worktree::io_error::format_worktree_path_io_message(&path, op, &self.source)
            }
            PathIoOperation::GitDir(op) => {
                crate::git_dir::io_error::format_git_dir_io_message(&path, op, &self.source)
            }
        }
    }
}

/// Why a Git config numeric value was rejected (`git config int` strict parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadNumericSource {
    /// Suffix unit letter is invalid or missing where required.
    InvalidUnit,
    /// Parsed value or scaled product does not fit.
    OutOfRange,
}

impl BadNumericSource {
    fn detail(self) -> &'static str {
        match self {
            Self::InvalidUnit => "invalid unit",
            Self::OutOfRange => "out of range",
        }
    }
}

impl std::fmt::Display for BadNumericSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

/// Why a tree transition would overwrite existing worktree content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverwriteReason {
    /// Staged or unstaged edits on paths that differ between trees.
    LocalChanges,
    /// Untracked paths that a checkout would replace.
    Untracked,
}

impl std::fmt::Display for OverwriteReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalChanges => f.write_str("local_changes"),
            Self::Untracked => f.write_str("untracked"),
        }
    }
}

/// Why a remote could not be read or written ([`RemoteError::Unreadable`](crate::remote::RemoteError::Unreadable)).
///
/// Every reason is a failure to talk to the remote at all, as opposed to the remote refusing
/// an update. [`Self::Transport`] is the one usually worth retrying on a flaky network.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteUnreadableReason {
    /// A local path or `file://` URL does not name a Git directory (valid `HEAD`, plus
    /// `objects/` and `refs/` in the common directory).
    RepositoryNotFound,
    /// The connection failed or broke: name resolution, refused or reset connection, TLS, or
    /// timeouts.
    Transport {
        /// Transport-level description of the failure, with URL credentials redacted.
        detail: String,
    },
    /// A smart-HTTP server answered with an error status other than an authentication
    /// challenge (those are [`RemoteError::Auth`](crate::remote::RemoteError::Auth)).
    HttpStatus {
        /// HTTP status code (`>= 400`).
        status: u16,
    },
    /// An HTTP response looked successful but was not a Git smart-HTTP ref advertisement
    /// (for example `200 text/html` from a parked or mistyped host).
    NotGitServer {
        /// HTTP status code (often `200`).
        status: u16,
        /// Response `Content-Type` when present (media type only, without parameters).
        content_type: Option<String>,
    },
    /// The transport ran, but the remote never completed the Git protocol handshake (for
    /// example `ssh` exited, or a `git://` peer closed, before any ref advertisement).
    NoHandshake,
    /// The remote ended a fetch response before sending the pack it was asked for (the
    /// stream closed before the `packfile` section).
    ResponseTruncated,
}

impl RemoteUnreadableReason {
    /// Stable snake-case name for machine-readable output (`repository_not_found`,
    /// `transport`, `http_status`, `not_git_server`, `no_handshake`, `response_truncated`).
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            Self::RepositoryNotFound => "repository_not_found",
            Self::Transport { .. } => "transport",
            Self::HttpStatus { .. } => "http_status",
            Self::NotGitServer { .. } => "not_git_server",
            Self::NoHandshake => "no_handshake",
            Self::ResponseTruncated => "response_truncated",
        }
    }
}

pub(crate) fn remote_unreadable_message(
    url: Option<&str>,
    reason: &RemoteUnreadableReason,
) -> String {
    let target = url.map_or_else(|| "the remote".to_owned(), |u| format!("'{u}'"));
    match reason {
        RemoteUnreadableReason::RepositoryNotFound => {
            format!("could not find repository at {target}")
        }
        RemoteUnreadableReason::Transport { detail } => {
            format!("could not reach {target}: {detail}")
        }
        RemoteUnreadableReason::HttpStatus { status } => match url {
            Some(u) => format!("HTTP {status} from {u}"),
            None => format!("HTTP {status} from the remote"),
        },
        RemoteUnreadableReason::NotGitServer {
            status,
            content_type,
        } => {
            let got = match content_type
                .as_deref()
                .map(|ct| ct.split(';').next().unwrap_or(ct).trim())
                .filter(|ct| !ct.is_empty())
            {
                Some(ct) => format!("{status} {ct}"),
                None => status.to_string(),
            };
            format!("{target} did not answer like a Git server (got {got})")
        }
        RemoteUnreadableReason::NoHandshake => {
            format!("could not read from {target}: no Git protocol response")
        }
        RemoteUnreadableReason::ResponseTruncated => {
            format!("{target} closed the connection before sending the pack")
        }
    }
}

/// Display text for [`RemoteError::Transport`].
pub(crate) fn remote_transport_message(url: Option<&str>, detail: &str) -> String {
    match url {
        Some(u) => format!("could not reach {u}: {detail}"),
        None => format!("transport failure: {detail}"),
    }
}

/// Display text for [`RemoteError::HttpStatus`].
pub(crate) fn remote_http_status_message(url: Option<&str>, status: u16) -> String {
    match url {
        Some(u) => format!("HTTP {status} from {u}"),
        None => format!("HTTP {status} from the remote"),
    }
}

/// Why an HTTP remote refused access ([`RemoteError::Auth`](crate::remote::RemoteError::Auth)).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum AuthFailure {
    /// The server asked for credentials (`401`) and no credential provider is wired.
    #[error("no credential provider is configured")]
    NoCredentialProvider,
    /// The credential provider could not supply credentials (no helper has them, and
    /// prompting is not possible).
    #[error("{detail}")]
    CredentialsUnavailable {
        /// The provider's explanation (for example that no helper has credentials and
        /// prompting is disabled).
        detail: String,
        /// The `credential.helper` programs the provider asked, when it reported them
        /// (see [`RemoteError::CredentialsUnavailable`](crate::remote::RemoteError::CredentialsUnavailable)); empty when none is configured or
        /// the provider is not helper-based.
        helpers: Vec<String>,
    },
    /// The provider answered without a usable username and password.
    #[error("the credential helper returned no usable username and password")]
    UnusableCredentials,
    /// The URL could not be split into the credential fields helpers expect.
    #[error("the URL cannot be described to a credential helper")]
    UrlNotDescribable,
    /// The server only offers authentication schemes grit does not speak (no `Basic`).
    #[error("unsupported authentication scheme: {}", schemes.join(", "))]
    UnsupportedScheme {
        /// The `WWW-Authenticate` challenges the server sent.
        schemes: Vec<String>,
    },
    /// Credentials were sent and the server rejected them (`401` again).
    #[error("the server rejected the credentials")]
    Rejected,
}

impl AuthFailure {
    /// Whether credentials were sent and refused, as opposed to never being available.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected)
    }

    /// The failure an HTTP client reports when a credential provider's
    /// [`fill`](crate::credentials::CredentialProvider::fill) fails with `err`.
    ///
    /// Always [`AuthFailure::CredentialsUnavailable`]: `detail` is `err`'s message,
    /// and `helpers` is the list [`RemoteError::CredentialsUnavailable`](crate::remote::RemoteError::CredentialsUnavailable) carries (empty
    /// for any other error). Custom [`HttpClient`](crate::transport::http::HttpClient)
    /// implementations use it to report fill failures the way the built-in client does.
    #[must_use]
    pub fn from_fill_error(err: Error) -> Self {
        let helpers = match &err {
            Error::Remote(crate::remote::RemoteError::CredentialsUnavailable { helpers }) => {
                helpers.clone()
            }
            _ => Vec::new(),
        };
        Self::CredentialsUnavailable {
            detail: err.to_string(),
            helpers,
        }
    }
}

/// The command a dirty worktree blocked ([`WorktreeError::DirtyWorktree`](crate::worktree::WorktreeError::DirtyWorktree)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorktreeOperation {
    /// Switching branches or checking out a commit.
    Switch,
    /// Picking (cherry-picking) or reverting commits.
    Pick,
    /// Merging.
    Merge,
    /// Rebasing with `--exec`.
    RebaseExec,
    /// Starting a rebase that stops on conflicts (the stop rewrites the index and worktree).
    Rebase,
}

impl std::fmt::Display for WorktreeOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Switch => "switching",
            Self::Pick => "picking",
            Self::Merge => "merging",
            Self::RebaseExec => "rebasing with --exec",
            Self::Rebase => "rebasing",
        })
    }
}

/// Configuration parse and validation failures.
#[derive(Debug, Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A line in a config file could not be parsed.
    #[error("bad config line {line} in {location}")]
    BadConfigLine {
        /// Config source label or path where the error was detected.
        location: String,
        /// One-based line number within that source.
        line: usize,
    },

    /// Same as [`Self::BadConfigLine`] with an explicit file path (Git file diagnostics).
    #[error("bad config line {line} in file '{file}'")]
    BadConfigLineInFile {
        /// Path to the config file involved in the error.
        file: String,
        /// One-based line number within that file.
        line: usize,
    },

    /// A config key requires a value but none was given.
    #[error("missing value for '{key}'")]
    MissingValue {
        /// Config key name that failed validation.
        key: String,
    },

    /// `fetch.negotiationalgorithm` and similar keys with missing values.
    #[error("bad config variable '{key}' in file '{file}' at line {line}")]
    BadConfigVariable {
        /// Config key name that failed validation.
        key: String,
        /// Path to the config file involved in the error.
        file: String,
        /// One-based line number within the config source.
        line: usize,
    },

    /// `unset` or a scoped read/write was requested for a key that is not set in that file.
    #[error("key '{key}' is not set")]
    KeyNotSet {
        /// Config key that was not present.
        key: String,
    },

    /// Integer config value could not be parsed or is out of range.
    #[error("bad numeric config value '{value}' for '{key}': {reason}")]
    BadNumericValue {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
        /// Why the value or operation was rejected.
        reason: BadNumericSource,
    },

    /// Same as [`Self::BadNumericValue`] with file context.
    #[error("bad numeric config value '{value}' for '{key}' in file {file}: {reason}")]
    BadNumericValueInFile {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
        /// Path to the config file involved in the error.
        file: String,
        /// Why the value or operation was rejected.
        reason: BadNumericSource,
    },

    /// `diff.context` (or similar) failed variable validation.
    #[error("bad config variable '{key}' in file '{file}' at line {line}")]
    BadConfigVariableAtLine {
        /// Config key name that failed validation.
        key: String,
        /// Path to the config file involved in the error.
        file: String,
        /// One-based line number within the config source.
        line: usize,
    },

    /// Command-line config could not supply a parseable value for a key.
    #[error("unable to parse '{key}' from command-line config")]
    UnableToParseFromCommandLine {
        /// Config key name that failed validation.
        key: String,
    },

    /// A `remote.<name>.url` is set in a file reached through a conditional include while
    /// `includeIf.hasconfig:remote.*.url` conditions are evaluated (such URLs could change
    /// which conditions hold).
    #[error(
        "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
    )]
    RemoteUrlInHasconfigInclude,

    /// `[include]` / `[includeIf]` nesting exceeded the Git-compatible depth limit.
    #[error("exceeded maximum include depth (depth {depth} exceeds limit {limit})")]
    IncludeDepthExceeded {
        /// Current include nesting depth when the limit was hit.
        depth: usize,
        /// Maximum allowed include nesting depth.
        limit: usize,
    },

    /// A config key is not `section[.subsection].variable` with a valid section and
    /// variable name.
    #[error("{}", invalid_key_message(key, *reason))]
    InvalidKey {
        /// Key as the caller spelled it.
        key: String,
        /// Which part of the key is malformed.
        reason: InvalidKeyReason,
    },

    /// A section name passed to a section-level edit is not a valid section name.
    #[error("invalid section name: {section}")]
    InvalidSectionName {
        /// Section name as the caller spelled it.
        section: String,
    },

    /// A value pattern (`regex` or `!regex`) is not a valid regular expression.
    #[error("invalid value-pattern regex '{pattern}': {reason}")]
    InvalidValuePattern {
        /// Pattern as given, including any leading `!`.
        pattern: String,
        /// Why the regular expression engine rejected it.
        reason: String,
    },

    /// A config key regexp (for example `git config --get-regexp`) is not valid.
    #[error("invalid key pattern '{pattern}': {source}")]
    InvalidPattern {
        /// Pattern as given.
        pattern: String,
        /// Why the regular expression engine rejected it.
        #[source]
        source: regex::Error,
    },

    /// Parsed config text declares an unsupported `core.repositoryformatversion`.
    #[error("unknown repository format version: {version}")]
    UnsupportedRepositoryFormatVersion {
        /// Parsed format version.
        version: u32,
    },

    /// A repository-local config cache mutex was poisoned.
    #[error("config cache lock poisoned")]
    ConfigCacheLockPoisoned,

    /// Repository config session mutex was poisoned.
    #[error("config session lock poisoned")]
    ConfigSessionLockPoisoned,

    /// Config session state was missing after refresh.
    #[error("config session missing after refresh")]
    ConfigSessionMissingAfterRefresh,

    /// A list prefix is empty or is not `section.` / `section.subsection.`.
    #[error("invalid list prefix '{prefix}'")]
    InvalidListPrefix {
        /// Prefix as the caller spelled it.
        prefix: String,
    },

    /// A value is not one of the words the key accepts (for example `protocol.version` or
    /// `gpg.format`).
    #[error("unknown value '{value}' for '{key}'")]
    UnknownValue {
        /// Config key whose value was rejected.
        key: String,
        /// Raw config value.
        value: String,
    },

    /// A thread-count key (for example `pack.threads`) holds a negative number.
    #[error("invalid number of threads ({value}) for '{key}'")]
    InvalidThreadCount {
        /// Config key whose value was rejected.
        key: String,
        /// The negative value.
        value: i64,
    },

    /// `GIT_CONFIG_COUNT` is not a non-negative integer no larger than `i32::MAX`.
    #[error("bogus count in GIT_CONFIG_COUNT: '{value}'")]
    InvalidEnvironmentCount {
        /// Raw `GIT_CONFIG_COUNT` value.
        value: String,
    },

    /// `GIT_CONFIG_COUNT` names more entries than there are `GIT_CONFIG_KEY_<n>` variables.
    #[error("missing config key GIT_CONFIG_KEY_{index}")]
    MissingEnvironmentKey {
        /// Zero-based index of the first missing `GIT_CONFIG_KEY_<n>`.
        index: usize,
    },

    /// `GIT_CONFIG_PARAMETERS` is not a whitespace-separated list of single-quoted entries.
    #[error("bogus format in GIT_CONFIG_PARAMETERS")]
    MalformedConfigParameters,

    /// A relative `include.path` / `includeIf.<cond>.path` appeared in config that did not
    /// come from a file on disk, so there is no directory to resolve it against.
    #[error("relative config includes must come from files")]
    RelativeIncludeOutsideFile,

    /// The config location (`--local` or `--worktree`) needs a repository, but none was given.
    #[error("not in a git repository")]
    RepositoryRequired,

    /// No home directory or `GIT_CONFIG_GLOBAL` is available to place the global config file.
    #[error("could not determine global config path")]
    NoGlobalConfigPath,

    /// A config value that must be a date (an approxidate such as `2.weeks.ago`) is
    /// not one.
    #[error("'{value}' is not a date for '{key}'")]
    InvalidDate {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
    },

    /// The config file could not be written because a `.lock` file is already present.
    #[error("could not lock config file {path}: File exists")]
    ConfigFileLocked {
        /// Path to the config file that could not be locked.
        path: String,
    },

    /// A single-value operation was requested but the key has multiple values.
    #[error("cannot overwrite multiple values with a single value for '{key}'")]
    MultipleValues {
        /// Config key name that failed validation.
        key: String,
    },

    /// Per-worktree config was requested but `extensions.worktreeConfig` is off.
    #[error(
        "worktree config is only allowed when extensions.worktreeConfig is enabled in the common config"
    )]
    WorktreeConfigNotEnabled,

    /// `remove-section` or `rename-section` did not find the named section.
    #[error("section '{section}' not found")]
    SectionNotFound {
        /// Section header name (e.g. `remote.origin`).
        section: String,
    },

    /// Inline `--comment` text must not span lines.
    #[error("no multi-line comment allowed")]
    MultilineCommentNotAllowed,

    /// `status.showUntrackedFiles` must be `no`, `normal`, `all`, or a Git boolean synonym.
    #[error(
        "invalid value '{value}' for 'status.showUntrackedFiles' (expected no, normal, all, boolean words, or integer 0 vs nonzero)"
    )]
    InvalidUntrackedFilesMode {
        /// Raw config value that could not be parsed.
        value: String,
    },

    /// `merge.directoryRenames` must be `conflict` or a Git boolean.
    #[error(
        "invalid value '{value}' for 'merge.directoryRenames' (expected true, false or conflict)"
    )]
    InvalidDirectoryRenames {
        /// Raw config value that could not be parsed.
        value: String,
    },

    /// The value of `key` is not valid for the type the key requires (a boolean, an integer
    /// or a color).
    #[error("{source} for '{key}'")]
    InvalidValue {
        /// Config key whose value was rejected, as the caller spelled it.
        key: String,
        /// Why the value was rejected.
        #[source]
        source: ConfigValueError,
    },
}

/// Why a raw config value could not be read as a boolean, integer or color.
///
/// Returned by [`crate::config::parse_bool`], [`crate::config::parse_i64`] and
/// [`crate::config::parse_color`], which see only the value. Readers that also know the key
/// ([`crate::config::ConfigSet::get_bool`], [`crate::config::ConfigSet::get_i64`]) wrap it in
/// [`ConfigError::InvalidValue`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigValueError {
    /// Not one of the boolean words (`true`/`yes`/`on`, `false`/`no`/`off`) or an integer.
    #[error("bad boolean config value '{value}'")]
    InvalidBool {
        /// Raw config value that could not be parsed.
        value: String,
    },

    /// An integer was required but the value is empty or only whitespace.
    #[error("empty integer value")]
    EmptyInteger,

    /// Not a decimal integer with an optional `k`/`m`/`g` suffix.
    #[error("invalid integer: '{value}'")]
    InvalidInteger {
        /// Raw config value (trimmed) that could not be parsed.
        value: String,
    },

    /// The integer does not fit in an `i64` once its `k`/`m`/`g` suffix is applied.
    #[error("integer overflow: '{value}'")]
    IntegerOverflow {
        /// Raw config value (trimmed) that overflowed.
        value: String,
    },

    /// Not a color specification (color names, numbers or `#rrggbb`, plus attributes).
    #[error("bad color value '{value}'")]
    InvalidColor {
        /// Raw config value (trimmed) that could not be parsed.
        value: String,
    },
}

impl ConfigValueError {
    /// Attach the config `key` whose value was rejected.
    ///
    /// # Parameters
    ///
    /// - `key`: the config key as the caller spelled it (for example `core.bare`).
    ///
    /// # Returns
    ///
    /// [`ConfigError::InvalidValue`] carrying `key` and this error as its source.
    #[must_use]
    pub fn for_key(self, key: impl Into<String>) -> ConfigError {
        ConfigError::InvalidValue {
            key: key.into(),
            source: self,
        }
    }
}

/// Which part of a config key [`ConfigError::InvalidKey`] rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidKeyReason {
    /// The key contains a newline or carriage return.
    LineBreak,
    /// The key has no `.`, so there is no section.
    MissingSection,
    /// The key ends with `.`, so there is no variable name.
    MissingVariable,
    /// The section is empty or has characters other than alphanumerics (Unicode
    /// [`char::is_alphanumeric`]) and `-`.
    InvalidSection,
    /// The variable name does not start with an ASCII letter, or has characters other than
    /// alphanumerics (Unicode [`char::is_alphanumeric`]) and `-`.
    InvalidVariable,
}

fn invalid_key_message(key: &str, reason: InvalidKeyReason) -> String {
    match reason {
        InvalidKeyReason::LineBreak => format!(
            "invalid key: '{}'",
            key.replace('\n', "\\n").replace('\r', "\\r")
        ),
        InvalidKeyReason::MissingSection => format!("key does not contain a section: '{key}'"),
        InvalidKeyReason::MissingVariable => {
            format!("key does not contain variable name: '{key}'")
        }
        InvalidKeyReason::InvalidSection => format!("invalid key (bad section): '{key}'"),
        InvalidKeyReason::InvalidVariable => format!("invalid key (bad variable name): '{key}'"),
    }
}

impl From<crate::config::GitConfigIntStrictError> for BadNumericSource {
    fn from(value: crate::config::GitConfigIntStrictError) -> Self {
        match value {
            crate::config::GitConfigIntStrictError::InvalidUnit => Self::InvalidUnit,
            crate::config::GitConfigIntStrictError::OutOfRange => Self::OutOfRange,
        }
    }
}

/// Submodule layout and state failures: where a submodule's git directory lives and whether
/// its checkout can be inspected.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubmoduleError {
    /// The submodule path is empty.
    #[error("submodule path is empty")]
    EmptyPath,

    /// A component of the submodule path is a symbolic link, so the path could escape the
    /// work tree.
    #[error("submodule path '{path}' passes through symbolic link '{component}'")]
    SymlinkInPath {
        /// Submodule path relative to the work tree.
        path: String,
        /// The component that is a symbolic link.
        component: String,
    },

    /// A legacy submodule git dir (`modules/<name>`) does not end with `/<name>`.
    #[error("git dir {} of submodule '{name}' does not end with its name", .git_dir.display())]
    GitDirNameMismatch {
        /// Submodule name.
        name: String,
        /// Git directory that was checked.
        git_dir: std::path::PathBuf,
    },

    /// A submodule git dir lies inside another submodule's git dir.
    #[error("git dir of submodule '{name}' lies inside another submodule's git dir")]
    GitDirNested {
        /// Submodule name.
        name: String,
    },

    /// An encoded submodule git dir has no `modules/` path segment.
    #[error("submodule git dir {} has no modules/ segment", .git_dir.display())]
    GitDirOutsideModules {
        /// Git directory that was checked.
        git_dir: std::path::PathBuf,
    },

    /// The segment after `modules/` in an encoded submodule git dir contains `/`.
    #[error("encoded submodule git dir {} has '/' after modules/", .git_dir.display())]
    GitDirSegmentHasSlash {
        /// Git directory that was checked.
        git_dir: std::path::PathBuf,
    },

    /// Another submodule already uses this git dir.
    #[error("git dir {} of submodule '{name}' is already used by another submodule", .git_dir.display())]
    GitDirInUse {
        /// Submodule name.
        name: String,
        /// Git directory that was checked.
        git_dir: std::path::PathBuf,
    },

    /// This git dir differs from another submodule's only by case, on a case-insensitive
    /// file system.
    #[error("git dir {} of submodule '{name}' differs only in case from another submodule's", .git_dir.display())]
    GitDirCaseConflict {
        /// Submodule name.
        name: String,
        /// Git directory that was checked.
        git_dir: std::path::PathBuf,
    },

    /// Every candidate git dir for a new submodule was invalid or already taken.
    #[error("could not choose a git dir for submodule '{name}'")]
    GitDirUnavailable {
        /// Submodule name.
        name: String,
    },

    /// `submodulePathConfig` is enabled but `submodule.<name>.gitdir` is not set.
    #[error("submodule.{name}.gitdir is not set (submodulePathConfig is enabled)")]
    GitDirNotConfigured {
        /// Submodule name.
        name: String,
    },

    /// The submodule has an embedded git dir but its `HEAD` could not be read.
    #[error("could not read HEAD of submodule '{path}'")]
    HeadUnreadable {
        /// Submodule path relative to the superproject work tree.
        path: String,
    },

    /// The submodule has no checkout (no embedded git directory) to update.
    #[error("submodule at '{path}' is not checked out")]
    NotCheckedOut {
        /// Submodule work tree path.
        path: String,
    },

    /// The submodule's `HEAD` names an object that is missing or is not a commit.
    #[error("HEAD of submodule '{path}' does not point to a readable commit")]
    HeadCommitMissing {
        /// Submodule path relative to the superproject work tree.
        path: String,
    },
}

impl ConfigError {
    /// Git stderr body for this error (without a `fatal:` prefix).
    #[must_use]
    pub fn message_body(&self) -> String {
        self.to_string()
    }
}

/// Reference lock failures while creating or updating a ref.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefLockError {
    /// A non-empty directory occupies the path where the ref file must be created.
    #[error(
        "cannot lock ref '{refname}': there is a non-empty directory '{path}' blocking reference '{refname}'"
    )]
    DirectoryInTheWay {
        /// Full ref name (for example `refs/heads/main`).
        refname: String,
        /// Directory path blocking creation of the ref file.
        path: String,
    },

    /// Another process holds the `packed-refs.lock` file (or it already exists).
    #[error("Unable to create '{lock_path}': File exists.")]
    PackedRefsLockHeld {
        /// Path to the lock file that could not be created.
        lock_path: String,
    },
}

/// Clean/smudge filter and EOL conversion failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterError {
    /// External clean or smudge filter command failed.
    #[error("{path}: {phase} filter '{driver}' failed")]
    ExternalFilterFailed {
        /// Filter driver name from `.gitattributes`.
        driver: String,
        /// Repository-relative or filesystem path related to the failure.
        path: String,
        /// Whether the clean or smudge filter hook failed.
        phase: FilterPhase,
    },

    /// A required clean or smudge filter could not be started (for example, no shell to run
    /// it), so the content was neither filtered nor stored unfiltered.
    #[error("{path}: cannot start {phase} filter '{driver}' ({program}): {detail}")]
    ExternalFilterSpawnFailed {
        /// Filter driver name from `.gitattributes`.
        driver: String,
        /// Repository-relative path being filtered.
        path: String,
        /// Whether the clean or smudge filter was being started.
        phase: FilterPhase,
        /// Program grit tried to spawn (`sh` for shell-command filters).
        program: String,
        /// OS error kind from the failed spawn.
        kind: std::io::ErrorKind,
        /// OS error message from the failed spawn.
        detail: String,
    },

    /// Filter output did not round-trip (clean/smudge mismatch).
    #[error("{path}: {detail}")]
    NotFilteredProperly {
        /// Repository-relative or filesystem path related to the failure.
        path: String,
        /// Why clean/smudge output did not round-trip.
        detail: String,
    },

    /// Invalid `working-tree-encoding` attribute value.
    #[error("true/false are no valid working-tree-encodings")]
    InvalidWorkingTreeEncoding,

    /// Line-ending conversion would modify the worktree against user settings.
    #[error("{detail}")]
    LineEndingWouldChange {
        /// Additional human-readable detail for the failure.
        detail: String,
    },

    /// A long-running filter process delayed a checkout, which this operation cannot wait for.
    #[error("{path}: filter delayed the checkout, which this operation does not support")]
    DelayedCheckoutUnsupported {
        /// Repository-relative path being checked out.
        path: String,
    },

    /// Shell filter subprocess exited non-zero.
    #[error("filter command exited with status {status}")]
    Failed {
        /// Subprocess exit status from the filter command.
        status: i32,
    },
}

/// Which filter hook failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPhase {
    /// Index → worktree (smudge).
    Smudge,
    /// Worktree → index (clean).
    Clean,
}

impl std::fmt::Display for FilterPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Smudge => f.write_str("smudge"),
            Self::Clean => f.write_str("clean"),
        }
    }
}

/// Unified diff / patch application errors.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyError {
    /// Patch bytes are malformed at a known input location.
    #[error("corrupt patch at {input}:{line}")]
    CorruptPatch {
        /// Patch input label (file name or `stdin`).
        input: String,
        /// One-based line number within the patch input.
        line: usize,
    },

    /// Preimage context could not be located in the blob at any search offset.
    #[error(
        "patch failed: context mismatch at hunk {hunk_index} (expected near line {expected_line})"
    )]
    ContextMismatch {
        /// Zero-based index of the hunk in the input slice.
        hunk_index: usize,
        /// One-based line number from the hunk header (`old_start`).
        expected_line: usize,
    },

    /// A later hunk would modify lines already changed by an earlier hunk.
    #[error("patch failed: overlapping hunks at hunk {hunk_index}")]
    HunksOverlap {
        /// Zero-based index of the hunk that overlaps prior applied changes.
        hunk_index: usize,
    },

    /// The patch removes more lines than exist in the blob (without a valid EOF match).
    #[error("patch failed: unexpected end of file at hunk {hunk_index}")]
    UnexpectedEof {
        /// Zero-based index of the offending hunk.
        hunk_index: usize,
    },

    /// A path named by the patch is not valid UTF-8.
    #[error("diff path is not valid UTF-8")]
    PathNotUtf8,

    /// A traditional (non-git) patch header names no file that the strip level leaves.
    #[error("unable to find filename in traditional patch")]
    MissingFilename,

    /// A `diff --git` header path has fewer leading components than the strip level removes.
    #[error("malformed {side} path in diff --git header: {path}")]
    MalformedHeaderPath {
        /// Which side of the header the path is on.
        side: PatchSide,
        /// The path as written in the header.
        path: String,
    },

    /// A mode header line (`old mode`, `new file mode`, …) carries no mode.
    #[error("invalid mode on line {line}: {text}")]
    InvalidMode {
        /// One-based line number in the patch input.
        line: usize,
        /// The header line as written.
        text: String,
    },

    /// A `@@ -a,b +c,d @@` hunk header could not be parsed.
    #[error("invalid hunk header: {header}")]
    InvalidHunkHeader {
        /// The header line as written.
        header: String,
    },

    /// A `GIT binary patch` section is malformed or cannot be applied.
    #[error(transparent)]
    BinaryPatch(#[from] BinaryPatchError),

    /// A text hunk could not be matched or applied.
    #[error("patch failed: {path}:{line}")]
    PatchFailed {
        /// File path from the patch.
        path: String,
        /// One-based line number in the patch input.
        line: usize,
    },

    /// Index or blob apply failed for a path.
    #[error("could not apply patch to {path}: {message}")]
    Failed {
        /// Repository-relative path.
        path: String,
        /// Failure detail.
        message: String,
    },

    /// `--cached` apply requires a stage-0 index entry at the preimage path.
    #[error("'{path}' is not in the index")]
    NotInIndex {
        /// Missing path.
        path: String,
    },

    /// A binary diff marker without `GIT binary patch` payload cannot be applied.
    #[error("binary patch data missing for '{path}'")]
    BinaryDataMissing {
        /// Path from the patch.
        path: String,
    },

    /// The caller's [`crate::progress::CancelToken`] stopped the apply between files.
    #[error("operation cancelled")]
    Cancelled,
}

/// Side of a patch a header path belongs to ([`ApplyError::MalformedHeaderPath`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PatchSide {
    /// The preimage (`a/…`, `---`).
    Old,
    /// The postimage (`b/…`, `+++`).
    New,
}

impl std::fmt::Display for PatchSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Old => "old",
            Self::New => "new",
        })
    }
}

/// Why a `GIT binary patch` section could not be read or applied ([`ApplyError::BinaryPatch`]).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BinaryPatchError {
    /// A section header is neither `literal <size>` nor `delta <size>`.
    #[error("unsupported binary patch section: '{header}'")]
    UnsupportedSection {
        /// The section header as written.
        header: String,
    },
    /// The size after `literal` / `delta` is not a decimal number.
    #[error("invalid binary patch section size: '{size}'")]
    InvalidSectionSize {
        /// The size text as written.
        size: String,
    },
    /// A payload line is empty where a length marker was expected.
    #[error("empty binary patch payload line")]
    EmptyLine,
    /// A payload line starts with a character that is not a length marker (`A`–`Z`, `a`–`z`).
    #[error("invalid binary patch line length marker: '{marker}'")]
    InvalidLineLength {
        /// The marker character as written.
        marker: char,
    },
    /// A payload line's base85 body is truncated, has bytes outside the alphabet, or does not
    /// decode to its declared length.
    #[error("invalid binary patch base85")]
    InvalidBase85,
    /// The zlib payload of a section could not be inflated.
    #[error("failed to inflate binary patch payload: {detail}")]
    Inflate {
        /// The zlib library's description of the failure.
        detail: String,
    },
    /// A literal section inflated to a different size than its header declared.
    #[error("binary patch literal size {actual} != declared {declared}")]
    SizeMismatch {
        /// Bytes after inflating.
        actual: usize,
        /// Size from the section header.
        declared: usize,
    },
    /// A delta section was applied without the preimage blob it is relative to.
    #[error("binary patch delta requires a base blob")]
    DeltaWithoutBase,
    /// The patch has a forward section but no reverse section.
    #[error("binary patch has no reverse section")]
    NoReverseSection,
}

/// `--pathspec-from-file` / `--pathspec-file-nul` parse failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathspecFromFileParseError {
    /// A NUL-separated entry is not valid UTF-8.
    #[error("invalid UTF-8 in pathspec-from-file (NUL mode)")]
    InvalidUtf8InNulRecord,
    /// A NUL-terminated entry used C-style quoting (forbidden in NUL mode).
    #[error("pathspec-from-file: line is not NUL terminated: {line}")]
    QuotedLineInNulMode {
        /// Offending entry text.
        line: String,
    },
    /// A line-mode entry had invalid double-quote wrapping.
    #[error("invalid C-style quoting: {line}")]
    InvalidCStyleQuoting {
        /// Raw line from the file.
        line: String,
    },
    /// Backslash escape at end of a quoted line.
    #[error("invalid escape at end of string")]
    InvalidEscapeAtEnd,
    /// Octal escape shorter than three digits.
    #[error("truncated octal escape")]
    TruncatedOctalEscape,
    /// Octal escape bytes are not valid UTF-8.
    #[error("invalid octal bytes")]
    InvalidOctalBytes,
    /// Octal digits do not form a valid byte value.
    #[error("invalid octal escape value")]
    InvalidOctalValue,
    /// Unknown escape after backslash in a quoted line.
    #[error("invalid escape sequence \\{escape}")]
    InvalidEscapeSequence {
        /// Character after the backslash.
        escape: char,
    },
    /// Quoted line decoded to invalid UTF-8.
    #[error("invalid UTF-8 in quoted pathspec")]
    InvalidUtf8InQuotedLine,
}

/// Failure while resolving argv and `--pathspec-from-file` pathspecs against a work tree.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathspecResolveError {
    /// An argv pathspec was the empty string.
    #[error("invalid path ''")]
    EmptyPathspec,
    /// Resolved path lies outside the repository work tree.
    #[error("{elt}: '{path}' is outside repository at '{work_tree}'")]
    OutsideRepository {
        /// User-facing pathspec token (argv element).
        elt: String,
        /// Resolved absolute path outside the work tree.
        path: String,
        /// Canonical work tree root.
        work_tree: String,
    },
    /// `--pathspec-from-file` bytes could not be parsed.
    #[error(transparent)]
    FromFile(#[from] PathspecFromFileParseError),
    /// Command requires a working tree but the repository is bare.
    #[error("this operation needs a working tree")]
    NeedsWorkTree,
    /// Incompatible `GIT_*_PATHSPECS` global flags.
    #[error(transparent)]
    InvalidGlobals(#[from] crate::pathspec::PathspecGlobalsError),
}

/// Why a `:(attr:...)` pathspec clause was rejected.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttrPathspecError {
    /// The clause names no attribute requirement (`:(attr:)`).
    #[error("attr requirement list is empty")]
    Empty,
    /// More than one `attr:` clause appears in one pathspec.
    #[error("a pathspec may contain at most one attr clause")]
    MultipleAttrClauses,
    /// A requirement names an attribute that is not valid (`-a=b`, `bad name`, `bad/name`).
    #[error("'{name}' is not a usable attribute name")]
    InvalidName {
        /// The attribute name as written.
        name: String,
    },
    /// A `\` escape ends the value or precedes whitespace.
    #[error("attr value ends with a dangling escape")]
    DanglingEscape,
    /// A `\` escape precedes something other than `,`.
    #[error("attr values may only escape ','")]
    UnsupportedEscape,
}

/// Path outside the repository during pathspec or transport resolution.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathOutsideRepoError {
    /// A resolved path escapes the repository work tree or git directory.
    #[error("'{subpath}' is outside repository at '{repo_root}'")]
    OutsideRepository {
        /// Repository-relative or filesystem path related to the failure.
        path: String,
        /// Path component that lies outside the repository root.
        subpath: String,
        /// Canonical repository root used for the outside-path check.
        repo_root: String,
    },
}

/// Why a path could not be opened as a Git repository; carried by [`RepositoryError::NotARepository`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NotARepositoryReason {
    /// No usable git directory exists at `path`, or discovery found none at or above it.
    #[error("{}", crate::git_path::path_for_error_message(path))]
    NoRepositoryAt {
        /// Directory, git directory, or `.git` path that was probed.
        path: std::path::PathBuf,
    },
    /// A `.git` entry is a FIFO, socket or device rather than a regular file or directory.
    #[error(
        "invalid gitfile format: {} is not a regular file",
        crate::git_path::path_for_error_message(path)
    )]
    GitfileNotRegularFile {
        /// The offending `.git` entry.
        path: std::path::PathBuf,
    },
    /// A gitfile has no `gitdir: <path>` line.
    #[error("invalid gitfile format")]
    GitfileMissingGitdir {
        /// The gitfile that was parsed.
        path: std::path::PathBuf,
    },
    /// A gitfile exists but could not be read (permissions, invalid UTF-8, ...).
    #[error("{error}")]
    GitfileUnreadable {
        /// The gitfile that could not be read.
        path: std::path::PathBuf,
        /// The underlying read failure.
        error: std::io::Error,
    },
}

pub(crate) fn maintenance_running_message(holder: &crate::maintenance::GcLockHolder) -> String {
    match (holder.pid, holder.host.as_deref()) {
        (Some(pid), Some(host)) => {
            format!("repository maintenance is already running (process {pid} on {host})")
        }
        (Some(pid), None) => format!("repository maintenance is already running (process {pid})"),
        _ => "repository maintenance is already running".to_owned(),
    }
}

pub(crate) fn restore_unmerged_message(paths: &[String]) -> String {
    match paths {
        [path] => format!("path '{path}' is unmerged"),
        _ => format!(
            "paths are unmerged: {}",
            paths
                .iter()
                .map(|p| format!("'{p}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Failure decoding object bytes from loose storage or a pack entry.
///
/// Used as the `source` of [`ObjectError::CorruptLoose`] and [`ObjectError::CorruptPacked`]
/// so object-read layers do not store a recursive [`enum@Error`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObjectReadFailure {
    /// Filesystem read failed while loading object bytes.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// zlib inflate/deflate failure on object payload bytes.
    #[error("zlib error: {detail}")]
    Zlib {
        /// The zlib library's description of the failure.
        detail: String,
    },
    /// Object header or payload failed structural checks (see [`ObjectError::Corrupt`]).
    #[error("corrupt object: {detail}")]
    CorruptObject {
        /// What is wrong with the object bytes.
        detail: String,
    },
    /// Declared size did not match bytes read from a stream.
    #[error("object stream ended after {actual} of {expected} bytes")]
    ObjectStreamTruncated {
        /// Payload size the reader expected.
        expected: u64,
        /// Bytes read before EOF.
        actual: u64,
    },
    /// Object type token in a header was not recognized.
    #[error("unknown object type '{name}'")]
    UnknownObjectType {
        /// The type token as read.
        name: String,
    },
    /// Loose object header type field exceeds Git's limit.
    #[error("header for {oid} too long, exceeds 32 bytes")]
    ObjectHeaderTooLong {
        /// Object id (hex) of the loose object with an oversized header.
        oid: String,
    },
    /// Delta chain exceeded the read-side depth limit.
    #[error("delta chain too deep (limit {limit})")]
    DeltaChainTooDeep {
        /// Configured maximum chain depth.
        limit: usize,
    },
    /// Payload failed the same checks used when refusing writes (`git fsck` rules).
    #[error("refusing to write invalid {kind} object: {}", .check.report_line())]
    InvalidObject {
        /// Kind the bytes were parsed as.
        kind: crate::objects::ObjectKind,
        /// First broken rule.
        check: crate::fsck_standalone::FsckError,
    },
}

impl ObjectReadFailure {
    /// Maps the [`ObjectError`] decode failures that may appear while reading stored object
    /// bytes.
    ///
    /// Returns `Err(err)` when `err` is not one of those failures (callers must not wrap
    /// unrelated failures as object corruption). Used only by
    /// [`Error::wrap_loose_read_failure`] and [`Error::wrap_packed_read_failure`].
    pub(crate) fn try_from_object_read_error(err: Error) -> std::result::Result<Self, Error> {
        match err {
            Error::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(Self::Io(e)),
            Error::Object(ObjectError::Zlib { detail }) => Ok(Self::Zlib { detail }),
            Error::Object(ObjectError::Corrupt { detail }) => Ok(Self::CorruptObject { detail }),
            Error::Object(ObjectError::StreamTruncated { expected, actual }) => {
                Ok(Self::ObjectStreamTruncated { expected, actual })
            }
            Error::Object(ObjectError::UnknownType { name }) => {
                Ok(Self::UnknownObjectType { name })
            }
            Error::Object(ObjectError::HeaderTooLong { oid }) => {
                Ok(Self::ObjectHeaderTooLong { oid })
            }
            Error::Object(ObjectError::DeltaChainTooDeep { limit }) => {
                Ok(Self::DeltaChainTooDeep { limit })
            }
            Error::Object(ObjectError::Invalid { kind, check }) => {
                Ok(Self::InvalidObject { kind, check })
            }
            other => Err(other),
        }
    }
}

/// The top-level error type for all grit-lib operations.
///
/// Every variant is one functional area (plus the cross-cutting [`Self::Io`],
/// [`Self::LockHeld`] and [`Self::Cancelled`]). Each area enum carries that area's specific
/// failures with structured fields; match on the area first, then on its enum.
///
/// # Examples
///
/// ```
/// use grit_lib::error::{Error, ObjectError};
///
/// let err = Error::Object(ObjectError::NotFound { name: "1234abcd".into() });
/// assert_eq!(err.area(), "object");
/// assert_eq!(err.kind_name(), "object_not_found");
/// assert!(matches!(err, Error::Object(ObjectError::NotFound { .. })));
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Repository discovery, layout, format, and maintenance-lock failures.
    #[error(transparent)]
    Repository(#[from] RepositoryError),

    /// Object database, pack, commit-graph, multi-pack-index, and promisor failures.
    #[error(transparent)]
    Object(#[from] ObjectError),

    /// Index read, write, extension, or write-tree failure.
    #[error(transparent)]
    Index(crate::index::IndexError),

    /// Reference, reflog, ref store, and `HEAD` state failures.
    #[error(transparent)]
    Ref(#[from] RefError),

    /// Revision lookup, parsing, and walk failures.
    #[error(transparent)]
    Revision(#[from] RevisionError),

    /// Configuration parse, validation, and write failures.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Work tree, path, pathspec, checkout, filter, submodule, and linked worktree failures.
    #[error(transparent)]
    Worktree(#[from] crate::worktree::WorktreeError),

    /// Patch application, hunk selection, and blame failures.
    #[error(transparent)]
    Diff(#[from] DiffError),

    /// Commit, identity, signing, and patch-scoped commit failures.
    #[error(transparent)]
    Commit(crate::porcelain::commit_error::CommitError),

    /// Merge, cherry-pick, revert, and in-progress operation failures.
    #[error(transparent)]
    Merge(crate::porcelain::merge_error::MergeError),

    /// History rewrite (`grit history`, rebase) failures.
    #[error(transparent)]
    History(crate::history::HistoryError),

    /// Stash push, apply, and stack failures.
    #[error(transparent)]
    Stash(crate::porcelain::stash_error::StashError),

    /// Branch create, delete, lookup, and upstream configuration failures.
    #[error(transparent)]
    Branch(#[from] crate::porcelain::branch_error::BranchError),

    /// Tag create, delete, and verification failures.
    #[error(transparent)]
    Tag(#[from] crate::porcelain::tag::TagError),

    /// Notes ref, notes tree, and notes merge failures.
    #[error(transparent)]
    Notes(#[from] NotesError),

    /// Remote transport, fetch, push, and clone failures.
    #[error(transparent)]
    Remote(crate::remote::RemoteError),

    /// A Git hook subprocess failed or could not be started.
    #[error(transparent)]
    Hook(crate::hooks::HookError),

    /// An I/O error from the underlying filesystem.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A lock file (for example `.git/index.lock` or `refs/heads/main.lock`) already exists, so
    /// another process is updating the same file or a crashed one left the lock behind.
    ///
    /// Operations that return this before changing anything leave the repository untouched.
    /// The lock file belongs to whoever created it and is never removed. Retry once the other
    /// process finishes, or delete the lock path when no process owns it (a `holder` with
    /// `running: false` means the lock is stale).
    #[error(transparent)]
    LockHeld(#[from] LockHeldError),

    /// A long-running library operation stopped at a cancellation checkpoint.
    ///
    /// Use [`Error::is_cancelled`] to test for it, including cancellations recorded inside
    /// area errors.
    #[error("operation cancelled")]
    Cancelled,
}

impl Error {
    /// Stable snake_case name of this error's specific failure (for example
    /// `"not_a_repository"`), for scripts and serialized output that must branch on the
    /// failure kind.
    ///
    /// The names are the leaf failure names and change only when a failure is renamed.
    /// Match on the error value itself in Rust code.
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::error::Error;
    /// use grit_lib::porcelain::CommitError;
    ///
    /// assert_eq!(
    ///     Error::Commit(CommitError::NothingToCommit).kind_name(),
    ///     "nothing_to_commit"
    /// );
    /// assert_eq!(Error::Cancelled.kind_name(), "cancelled");
    /// ```
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Repository(e) => e.kind_name(),
            Self::Object(e) => e.kind_name(),
            Self::Index(e) => e.kind_name(),
            Self::Ref(e) => e.kind_name(),
            Self::Revision(e) => e.kind_name(),
            Self::Config(..) => "config",
            Self::Worktree(e) => e.kind_name(),
            Self::Diff(e) => e.kind_name(),
            Self::Commit(e) => e.kind_name(),
            Self::Merge(e) => e.kind_name(),
            Self::History(..) => "history",
            Self::Stash(e) => e.kind_name(),
            Self::Branch(e) => e.kind_name(),
            Self::Tag(e) => e.kind_name(),
            Self::Notes(e) => e.kind_name(),
            Self::Remote(e) => e.kind_name(),
            Self::Hook(..) => "hook",
            Self::Io(..) => "io",
            Self::LockHeld(..) => "lock_held",
            Self::Cancelled => "cancelled",
        }
    }

    /// Functional area of this error: the snake_case name of its umbrella variant
    /// (`"repository"`, `"object"`, `"index"`, `"ref"`, …).
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::error::Error;
    ///
    /// assert_eq!(Error::Cancelled.area(), "cancelled");
    /// ```
    #[must_use]
    pub fn area(&self) -> &'static str {
        match self {
            Self::Repository(..) => "repository",
            Self::Object(..) => "object",
            Self::Index(..) => "index",
            Self::Ref(..) => "ref",
            Self::Revision(..) => "revision",
            Self::Config(..) => "config",
            Self::Worktree(..) => "worktree",
            Self::Diff(..) => "diff",
            Self::Commit(..) => "commit",
            Self::Merge(..) => "merge",
            Self::History(..) => "history",
            Self::Stash(..) => "stash",
            Self::Branch(..) => "branch",
            Self::Tag(..) => "tag",
            Self::Notes(..) => "notes",
            Self::Remote(..) => "remote",
            Self::Hook(..) => "hook",
            Self::Io(..) => "io",
            Self::LockHeld(..) => "lock_held",
            Self::Cancelled => "cancelled",
        }
    }

    /// Build [`WorktreeError::DirtyWorktree`](crate::worktree::WorktreeError::DirtyWorktree),
    /// sorting and deduplicating `paths`.
    #[must_use]
    pub fn dirty_worktree(operation: WorktreeOperation, mut affected_paths: Vec<String>) -> Self {
        affected_paths.sort();
        affected_paths.dedup();
        Self::Worktree(crate::worktree::WorktreeError::DirtyWorktree {
            operation,
            paths: affected_paths,
        })
    }

    /// Build [`WorktreeError::WouldOverwrite`](crate::worktree::WorktreeError::WouldOverwrite)
    /// for switch and similar tree transitions.
    #[must_use]
    pub fn would_overwrite(reason: OverwriteReason, mut affected_paths: Vec<String>) -> Self {
        affected_paths.sort();
        affected_paths.dedup();
        let message = match reason {
            OverwriteReason::LocalChanges => {
                "you have uncommitted changes — commit them before switching".into()
            }
            OverwriteReason::Untracked => {
                let first = affected_paths
                    .first()
                    .map(String::as_str)
                    .unwrap_or("<path>");
                format!("untracked file '{first}' would be overwritten — move or remove it first")
            }
        };
        Self::Worktree(crate::worktree::WorktreeError::WouldOverwrite {
            reason,
            paths: affected_paths,
            message,
        })
    }

    /// Build [`crate::remote::RemoteError::Unreadable`] for `url`.
    #[must_use]
    pub fn remote_unreadable(url: impl Into<String>, reason: RemoteUnreadableReason) -> Self {
        Self::Remote(crate::remote::RemoteError::unreadable(url, reason))
    }

    /// Whether this is an unreadable remote failure (see [`crate::remote::RemoteError::is_remote_unreadable`]).
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        match self {
            Self::Remote(e) => e.is_remote_unreadable(),
            _ => false,
        }
    }

    /// When this is [`Error::LockHeld`], returns the structured lock failure.
    #[must_use]
    pub fn lock_held(&self) -> Option<&LockHeldError> {
        match self {
            Self::LockHeld(err) => Some(err),
            _ => None,
        }
    }

    /// When this is a worktree or git-dir path I/O failure, returns the shared leaf shape.
    #[must_use]
    pub fn path_io(&self) -> Option<PathIoError> {
        let io = match self {
            Self::Worktree(crate::worktree::WorktreeError::PathIo(io)) => io,
            Self::Repository(RepositoryError::GitDirPathIo(io)) => io,
            _ => return None,
        };
        Some(PathIoError {
            operation: io.operation,
            path: io.path.clone(),
            source: preserve_io_error(&io.source),
        })
    }

    /// Whether this is a work-tree path I/O failure caused by another process holding the
    /// file open (a Windows sharing or lock violation, `EBUSY`, or would-block), so retrying
    /// after closing that program can succeed.
    #[must_use]
    pub fn is_worktree_file_in_use(&self) -> bool {
        matches!(
            self,
            Self::Worktree(crate::worktree::WorktreeError::PathIo(io))
                if matches!(io.operation, PathIoOperation::Worktree(_))
                    && crate::worktree::io_error::is_file_in_use(&io.source)
        )
    }

    /// Whether this is [`RepositoryError::git_dir_path_io`] caused by another process holding a git-dir file
    /// open (Windows sharing or access-denied during replace, `EBUSY`, …).
    #[must_use]
    pub fn is_git_dir_file_in_use(&self) -> bool {
        match self {
            Self::Repository(RepositoryError::GitDirPathIo(io)) => match io.operation {
                PathIoOperation::GitDir(operation) => {
                    crate::git_dir::io_error::is_git_dir_transient_contention(operation, &io.source)
                }
                PathIoOperation::Worktree(_) => false,
            },
            _ => false,
        }
    }

    /// Whether this is [`Error::Cancelled`] or an area failure recording a cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::History(e) => e.is_cancelled(),
            Self::Worktree(e) => e.is_cancelled(),
            Self::Io(err)
                if err.to_string() == crate::transport::stateless_http::TRANSPORT_IO_CANCELLED =>
            {
                true
            }
            _ => false,
        }
    }

    /// Whether an object read failed because a stored copy is damaged (zlib/header/truncation),
    /// so composite stores may consult the next backend.
    #[must_use]
    pub fn is_damaged_object(&self) -> bool {
        match self {
            Self::Object(
                ObjectError::Zlib { .. }
                | ObjectError::Corrupt { .. }
                | ObjectError::CorruptLoose { .. }
                | ObjectError::CorruptPacked { .. },
            ) => true,
            Self::Io(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => true,
            _ => false,
        }
    }

    pub(crate) fn is_object_read_wrap_source(&self) -> bool {
        match self {
            Self::Io(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => true,
            Self::Object(
                ObjectError::Zlib { .. }
                | ObjectError::Corrupt { .. }
                | ObjectError::StreamTruncated { .. }
                | ObjectError::UnknownType { .. }
                | ObjectError::HeaderTooLong { .. }
                | ObjectError::DeltaChainTooDeep { .. }
                | ObjectError::Invalid { .. },
            ) => true,
            _ => false,
        }
    }

    /// Attach loose-object path context to low-level read failures at the ODB boundary.
    #[must_use]
    pub(crate) fn wrap_loose_read_failure(
        oid: crate::objects::ObjectId,
        path: std::path::PathBuf,
        err: Error,
    ) -> Error {
        if matches!(err, Self::Object(ObjectError::CorruptLoose { .. })) {
            return err;
        }
        if !err.is_object_read_wrap_source() {
            return err;
        }
        match ObjectReadFailure::try_from_object_read_error(err) {
            Ok(source) => Self::Object(ObjectError::CorruptLoose {
                oid,
                path,
                source: Box::new(source),
            }),
            Err(err) => err,
        }
    }

    /// Attach pack location context to low-level read failures at the ODB boundary.
    #[must_use]
    pub(crate) fn wrap_packed_read_failure(
        oid: crate::objects::ObjectId,
        pack: std::path::PathBuf,
        offset: u64,
        err: Error,
    ) -> Error {
        if matches!(err, Self::Object(ObjectError::CorruptPacked { .. })) {
            return err;
        }
        if !err.is_object_read_wrap_source() {
            return err;
        }
        match ObjectReadFailure::try_from_object_read_error(err) {
            Ok(source) => Self::Object(ObjectError::CorruptPacked {
                oid,
                pack,
                offset,
                source: Box::new(source),
            }),
            Err(err) => err,
        }
    }

    /// Build [`WorktreeError::InvalidPathComponent`](crate::worktree::WorktreeError::InvalidPathComponent).
    #[must_use]
    pub fn invalid_path_component(
        path: impl Into<String>,
        reason: crate::path_validate::PathComponentError,
    ) -> Self {
        Self::Worktree(crate::worktree::WorktreeError::InvalidPathComponent {
            path: path.into(),
            reason,
        })
    }

    /// When set, the CLI should prefix stderr with `fatal: ` for Git-compatible commands.
    #[must_use]
    pub fn wants_fatal_prefix(&self) -> bool {
        use crate::worktree::WorktreeError;
        matches!(
            self,
            Error::Config(_)
                | Error::Worktree(
                    WorktreeError::Submodule(_)
                        | WorktreeError::Filter(_)
                        | WorktreeError::OutsideRepo(_)
                )
                | Error::Ref(RefError::Lock(_))
        )
    }

    /// Format for Git-style stderr (adds `fatal:` / `error:` when appropriate).
    #[must_use]
    pub fn git_stderr_message(&self) -> String {
        match self {
            Error::Diff(DiffError::Apply(ApplyError::CorruptPatch { .. })) => {
                crate::diagnostics::error_line(&self.to_string())
            }
            Error::Worktree(crate::worktree::WorktreeError::WouldOverwrite { message, .. }) => {
                message.clone()
            }
            e if e.wants_fatal_prefix() => crate::diagnostics::fatal_line(&e.to_string()),
            other => other.to_string(),
        }
    }
}

impl From<crate::hooks::HookError> for Error {
    fn from(err: crate::hooks::HookError) -> Self {
        Self::Hook(err)
    }
}

impl From<crate::porcelain::commit_error::CommitError> for Error {
    fn from(err: crate::porcelain::commit_error::CommitError) -> Self {
        Self::Commit(err)
    }
}

impl From<crate::porcelain::merge_error::MergeError> for Error {
    fn from(err: crate::porcelain::merge_error::MergeError) -> Self {
        Self::Merge(err)
    }
}

impl From<crate::history::HistoryError> for Error {
    fn from(err: crate::history::HistoryError) -> Self {
        Self::History(err)
    }
}

impl From<crate::porcelain::stash_error::StashError> for Error {
    fn from(err: crate::porcelain::stash_error::StashError) -> Self {
        Self::Stash(err)
    }
}

impl From<crate::remote::RemoteError> for Error {
    fn from(err: crate::remote::RemoteError) -> Self {
        Self::Remote(err)
    }
}

impl From<crate::ident::CommitIdentityError> for Error {
    fn from(err: crate::ident::CommitIdentityError) -> Self {
        crate::porcelain::commit_error::CommitError::from(err).into()
    }
}

impl From<crate::refs::store::RefStoreError> for Error {
    fn from(err: crate::refs::store::RefStoreError) -> Self {
        match err {
            crate::refs::store::RefStoreError::GitDirPathIo {
                path,
                operation,
                os_error,
            } => RepositoryError::git_dir_path_io(
                path,
                operation,
                crate::refs::store::RefStoreError::git_dir_io_source(os_error),
            )
            .into(),
            crate::refs::store::RefStoreError::Hook(hook) => Error::Hook(hook.into_hook_error()),
            other => Error::Ref(RefError::Store(other)),
        }
    }
}

impl From<RefLockError> for Error {
    fn from(err: RefLockError) -> Self {
        Self::Ref(RefError::Lock(err))
    }
}

impl From<crate::reftable::ReftableError> for Error {
    fn from(err: crate::reftable::ReftableError) -> Self {
        Self::Ref(RefError::Reftable(err))
    }
}

impl From<crate::rev_parse_error::RevParseError> for Error {
    fn from(err: crate::rev_parse_error::RevParseError) -> Self {
        Self::Revision(RevisionError::Parse(err))
    }
}

impl From<crate::rev_list_error::RevListError> for Error {
    fn from(err: crate::rev_list_error::RevListError) -> Self {
        Self::Revision(RevisionError::List(err))
    }
}

impl From<ApplyError> for Error {
    fn from(err: ApplyError) -> Self {
        match err {
            ApplyError::Cancelled => Self::Cancelled,
            other => Self::Diff(DiffError::Apply(other)),
        }
    }
}

impl From<crate::userdiff::FuncnamePatternError> for Error {
    fn from(err: crate::userdiff::FuncnamePatternError) -> Self {
        Self::Diff(DiffError::FuncnamePattern(err))
    }
}

impl From<crate::porcelain::hunk_select::HunkSelectError> for Error {
    fn from(err: crate::porcelain::hunk_select::HunkSelectError) -> Self {
        Self::Diff(DiffError::HunkSelection(err))
    }
}

impl From<FilterError> for Error {
    fn from(err: FilterError) -> Self {
        Self::Worktree(crate::worktree::WorktreeError::Filter(err))
    }
}

impl From<SubmoduleError> for Error {
    fn from(err: SubmoduleError) -> Self {
        Self::Worktree(crate::worktree::WorktreeError::Submodule(err))
    }
}

impl From<PathOutsideRepoError> for Error {
    fn from(err: PathOutsideRepoError) -> Self {
        Self::Worktree(crate::worktree::WorktreeError::OutsideRepo(err))
    }
}

impl From<PathspecFromFileParseError> for Error {
    fn from(err: PathspecFromFileParseError) -> Self {
        Self::Worktree(crate::worktree::WorktreeError::PathspecFromFile(err))
    }
}

impl From<crate::bitmap_walk::BitmapWalkError> for Error {
    fn from(err: crate::bitmap_walk::BitmapWalkError) -> Self {
        Self::Object(ObjectError::BitmapWalk(err))
    }
}

impl From<crate::bitmap_walk::BitmapWalkUnsupported> for Error {
    fn from(err: crate::bitmap_walk::BitmapWalkUnsupported) -> Self {
        Self::Object(ObjectError::BitmapWalk(err.into()))
    }
}

impl From<crate::pack_bitmap::BitmapError> for Error {
    fn from(err: crate::pack_bitmap::BitmapError) -> Self {
        Self::Object(ObjectError::BitmapWalk(err.into()))
    }
}

impl From<crate::object_inspect::ResolveError> for Error {
    fn from(err: crate::object_inspect::ResolveError) -> Self {
        Self::Revision(RevisionError::Resolve(err))
    }
}

impl From<MidxError> for Error {
    fn from(err: MidxError) -> Self {
        Self::Object(ObjectError::Midx(err))
    }
}

impl From<crate::offload::OffloadError> for Error {
    fn from(err: crate::offload::OffloadError) -> Self {
        Self::Object(ObjectError::Offload(err))
    }
}

impl From<crate::objects::TreeEntryError> for Error {
    fn from(err: crate::objects::TreeEntryError) -> Self {
        Self::Object(ObjectError::InvalidTreeEntry(err))
    }
}

impl From<crate::ident::SignatureError> for Error {
    fn from(err: crate::ident::SignatureError) -> Self {
        Self::Object(ObjectError::InvalidSignature(err))
    }
}

impl From<crate::config::ident_resolve::IdentityError> for Error {
    fn from(err: crate::config::ident_resolve::IdentityError) -> Self {
        Self::Commit(crate::porcelain::commit_error::CommitError::Identity(err))
    }
}

impl From<crate::ident::AuthorArgError> for Error {
    fn from(err: crate::ident::AuthorArgError) -> Self {
        Self::History(crate::history::HistoryError::InvalidAuthor(err))
    }
}

impl From<crate::change_id::ChangeIdError> for Error {
    fn from(err: crate::change_id::ChangeIdError) -> Self {
        Self::Commit(crate::porcelain::commit_error::CommitError::ChangeId(err))
    }
}

impl From<crate::tree_sha256::TreeSha256ClaimError> for Error {
    fn from(err: crate::tree_sha256::TreeSha256ClaimError) -> Self {
        Self::Tag(crate::porcelain::tag::TagError::TreeSha256Claim(err))
    }
}

impl From<Cancelled> for Error {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

impl From<crate::worktree::LinkedWorktreeError> for Error {
    fn from(value: crate::worktree::LinkedWorktreeError) -> Self {
        Self::Worktree(value.into())
    }
}

impl From<PathIoError> for Error {
    fn from(value: PathIoError) -> Self {
        match value.operation {
            PathIoOperation::Worktree(_) => {
                Self::Worktree(crate::worktree::WorktreeError::PathIo(value))
            }
            PathIoOperation::GitDir(_) => Self::Repository(RepositoryError::GitDirPathIo(value)),
        }
    }
}

impl From<crate::lockfile::LockError> for Error {
    /// [`LockError::Held`](crate::lockfile::LockError::Held) becomes [`Error::LockHeld`] with the
    /// recorded [`LockHolder`](crate::lockfile::LockHolder), and I/O failures become [`Error::Io`].
    #[cold]
    fn from(err: crate::lockfile::LockError) -> Self {
        use crate::lockfile::{LockError, LockHolder};
        match err {
            LockError::Held { path } => Self::LockHeld(LockHeldError {
                holder: LockHolder::read(&path),
                path,
            }),
            LockError::Io { source, .. } => Self::Io(source),
        }
    }
}

impl From<crate::serve::ServeError> for Error {
    fn from(err: crate::serve::ServeError) -> Self {
        Self::Remote(crate::remote::RemoteError::Serve(err))
    }
}

#[cfg(feature = "unstable-hosting")]
impl From<crate::snapshot_checkout::CheckoutError> for Error {
    fn from(err: crate::snapshot_checkout::CheckoutError) -> Self {
        Self::Remote(crate::remote::RemoteError::Snapshot(err))
    }
}

impl From<crate::clone::CloneError> for Error {
    fn from(err: crate::clone::CloneError) -> Self {
        Self::Remote(crate::remote::RemoteError::Clone(Box::new(err)))
    }
}

impl From<crate::porcelain::pull::PullError> for Error {
    fn from(err: crate::porcelain::pull::PullError) -> Self {
        Self::Merge(crate::porcelain::merge_error::MergeError::Pull(err))
    }
}

impl From<crate::remote::push_plan::PushPlanError> for Error {
    fn from(err: crate::remote::push_plan::PushPlanError) -> Self {
        Self::Remote(crate::remote::RemoteError::PushPlan(err))
    }
}

impl From<crate::history::RebaseSessionError> for Error {
    fn from(err: crate::history::RebaseSessionError) -> Self {
        match err {
            crate::history::RebaseSessionError::NothingToCommit => {
                Self::Commit(crate::porcelain::commit_error::CommitError::NothingToCommit)
            }
            other => Self::History(crate::history::HistoryError::RebaseSession(Box::new(other))),
        }
    }
}

impl From<crate::porcelain::stash_error::StashPushValidationError> for Error {
    fn from(err: crate::porcelain::stash_error::StashPushValidationError) -> Self {
        Self::Stash(crate::porcelain::stash_error::StashError::PushValidation(
            err,
        ))
    }
}

impl From<crate::bundle::BundleError> for Error {
    fn from(err: crate::bundle::BundleError) -> Self {
        use crate::bundle::BundleError;
        match err {
            BundleError::Cancelled => Self::Cancelled,
            BundleError::ObjectNotFound { name } => Self::Object(ObjectError::NotFound { name }),
            other => Self::Remote(crate::remote::RemoteError::Bundle(other)),
        }
    }
}

impl From<crate::fetch_request::FetchRefspecError> for Error {
    fn from(e: crate::fetch_request::FetchRefspecError) -> Self {
        Self::Remote(crate::remote::RemoteError::FetchRefspec(e))
    }
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Serializes as `{"kind": <Error::kind_name>, "area": <Error::area>, "message": <Display text>}`,
/// plus structured fields for lock, overwrite, dirty-worktree, and conflict-marker failures.
#[cfg(feature = "serde-models")]
impl serde::Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use crate::worktree::WorktreeError;
        use serde::ser::SerializeStruct;

        match self {
            Self::LockHeld(lock) => {
                let mut out = serializer.serialize_struct("Error", 5)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("area", self.area())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field(
                    "path",
                    &crate::git_path::user_display_path(lock.path.as_path()),
                )?;
                if let Some(holder) = lock.holder {
                    out.serialize_field(
                        "holder",
                        &LockHolderSerde {
                            pid: holder.pid,
                            running: holder.running,
                        },
                    )?;
                }
                out.end()
            }
            Self::Worktree(WorktreeError::WouldOverwrite { reason, paths, .. }) => {
                let mut out = serializer.serialize_struct("Error", 5)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("area", self.area())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("reason", &reason.to_string())?;
                out.serialize_field("paths", paths)?;
                out.end()
            }
            Self::Worktree(WorktreeError::DirtyWorktree { operation, paths }) => {
                let mut out = serializer.serialize_struct("Error", 5)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("area", self.area())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("operation", &operation.to_string())?;
                out.serialize_field("paths", paths)?;
                out.end()
            }
            Self::Worktree(WorktreeError::UnmergedConflictMarkers { paths }) => {
                let mut out = serializer.serialize_struct("Error", 4)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("area", self.area())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("paths", paths)?;
                out.end()
            }
            Self::Stash(stash) => stash.serialize(serializer),
            _ => {
                let mut out = serializer.serialize_struct("Error", 3)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("area", self.area())?;
                out.serialize_field("message", &self.to_string())?;
                out.end()
            }
        }
    }
}

#[cfg(test)]
mod object_read_failure_tests {
    use super::{Error, ObjectError, ObjectReadFailure};

    #[test]
    fn try_from_object_read_error_rejects_unrelated_umbrella_variants() {
        assert!(matches!(
            ObjectReadFailure::try_from_object_read_error(Error::Cancelled),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn try_from_object_read_error_maps_decode_variants() {
        assert!(matches!(
            ObjectReadFailure::try_from_object_read_error(Error::Object(ObjectError::Zlib {
                detail: "bad".into()
            })),
            Ok(ObjectReadFailure::Zlib { .. })
        ));
    }
}

#[cfg(test)]
mod umbrella_area_tests {
    use super::{
        ConfigError, DiffError, Error, ErrorSummary, LockHeldError, NotesError, ObjectError,
        RefError, RepositoryError, RevisionError,
    };
    use crate::history::HistoryError;
    use crate::hooks::{HookError, HookOutput};
    use crate::index::IndexError;
    use crate::porcelain::branch_error::BranchError;
    use crate::porcelain::commit_error::CommitError;
    use crate::porcelain::merge_error::MergeError;
    use crate::porcelain::stash_error::StashError;
    use crate::porcelain::tag::TagError;
    use crate::remote::RemoteError;
    use crate::worktree::WorktreeError;

    /// Area name for each umbrella variant, written without a wildcard arm so a new variant
    /// fails to compile here until it is listed in [`AREAS`].
    fn area_by_variant(err: &Error) -> &'static str {
        match err {
            Error::Repository(_) => "repository",
            Error::Object(_) => "object",
            Error::Index(_) => "index",
            Error::Ref(_) => "ref",
            Error::Revision(_) => "revision",
            Error::Config(_) => "config",
            Error::Worktree(_) => "worktree",
            Error::Diff(_) => "diff",
            Error::Commit(_) => "commit",
            Error::Merge(_) => "merge",
            Error::History(_) => "history",
            Error::Stash(_) => "stash",
            Error::Branch(_) => "branch",
            Error::Tag(_) => "tag",
            Error::Notes(_) => "notes",
            Error::Remote(_) => "remote",
            Error::Hook(_) => "hook",
            Error::Io(_) => "io",
            Error::LockHeld(_) => "lock_held",
            Error::Cancelled => "cancelled",
        }
    }

    const AREAS: &[&str] = &[
        "repository",
        "object",
        "index",
        "ref",
        "revision",
        "config",
        "worktree",
        "diff",
        "commit",
        "merge",
        "history",
        "stash",
        "branch",
        "tag",
        "notes",
        "remote",
        "hook",
        "io",
        "lock_held",
        "cancelled",
    ];

    fn one_error_per_area() -> Vec<Error> {
        vec![
            Error::Repository(RepositoryError::RequiresWorkTree),
            Error::Object(ObjectError::InvalidId {
                input: "xyz".into(),
            }),
            Error::Index(IndexError::CacheTreeCorrupt),
            Error::Ref(RefError::UnbornHead),
            Error::Revision(RevisionError::NotFound {
                spec: "nope".into(),
            }),
            Error::Config(ConfigError::ConfigCacheLockPoisoned),
            Error::Worktree(WorktreeError::RemovePathsRequired),
            Error::Diff(DiffError::BlameWorktreeBareRepository),
            Error::Commit(CommitError::EmptyCommitMessage),
            Error::Merge(MergeError::NotFastForward),
            Error::History(HistoryError::EmptyMessage),
            Error::Stash(StashError::NoInitialCommit),
            Error::Branch(BranchError::NoPreviousBranch),
            Error::Tag(TagError::EmptyMessage),
            Error::Notes(NotesError::AlreadyExists {
                object: crate::objects::ObjectId::zero(),
            }),
            Error::Remote(RemoteError::HttpUnavailable),
            Error::Hook(HookError::Failed {
                hook_name: "pre-commit".into(),
                status: 1,
                output: HookOutput::default(),
            }),
            Error::Io(std::io::Error::other("disk")),
            Error::LockHeld(LockHeldError {
                path: ".git/index.lock".into(),
                holder: None,
            }),
            Error::Cancelled,
        ]
    }

    #[test]
    fn umbrella_error_has_one_variant_per_area() {
        let samples = one_error_per_area();
        assert_eq!(samples.len(), AREAS.len());
        let mut seen = std::collections::HashSet::new();
        for (err, expected) in samples.iter().zip(AREAS) {
            assert_eq!(area_by_variant(err), *expected);
            assert_eq!(err.area(), *expected, "{err:?}");
            assert!(seen.insert(err.area()), "area {expected} listed twice");
            assert!(!err.kind_name().is_empty());
        }
    }

    #[test]
    fn area_errors_convert_into_their_umbrella_variant() {
        let cases: Vec<(Error, &str)> = vec![
            (RepositoryError::RequiresWorkTree.into(), "repository"),
            (RefError::UnbornHead.into(), "ref"),
            (
                RevisionError::NotFound { spec: "x".into() }.into(),
                "revision",
            ),
            (CommitError::EmptyCommitMessage.into(), "commit"),
            (MergeError::NotFastForward.into(), "merge"),
            (HistoryError::EmptyMessage.into(), "history"),
            (StashError::NoInitialCommit.into(), "stash"),
            (BranchError::NoPreviousBranch.into(), "branch"),
            (TagError::EmptyMessage.into(), "tag"),
            (RemoteError::HttpUnavailable.into(), "remote"),
            (IndexError::CacheTreeCorrupt.into(), "index"),
            (WorktreeError::RemovePathsRequired.into(), "worktree"),
        ];
        for (err, area) in cases {
            assert_eq!(err.area(), area, "{err:?}");
        }
    }

    #[test]
    fn error_summary_keeps_kind_area_message_and_cancellation() {
        let summary = ErrorSummary::from(&Error::Ref(RefError::UnbornHead));
        assert_eq!(summary.kind(), Error::Ref(RefError::UnbornHead).kind_name());
        assert_eq!(summary.area(), "ref");
        assert_eq!(
            summary.message(),
            Error::Ref(RefError::UnbornHead).to_string()
        );
        assert_eq!(summary.to_string(), summary.message());
        assert!(!summary.is_cancelled());

        let cancelled = ErrorSummary::from(Error::Cancelled);
        assert!(cancelled.is_cancelled());
        assert_eq!(cancelled.kind(), "cancelled");
        assert_eq!(cancelled.area(), "cancelled");
        assert_eq!(cancelled.clone(), cancelled);
        assert_eq!(
            std::mem::size_of::<ErrorSummary>(),
            std::mem::size_of::<usize>()
        );
    }
}

#[cfg(feature = "serde-models")]
#[derive(serde::Serialize)]
struct LockHolderSerde {
    pid: u64,
    running: bool,
}
