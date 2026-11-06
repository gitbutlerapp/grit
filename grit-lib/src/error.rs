//! Shared error types for grit-lib.
//!
//! Library code uses [`enum@Error`] (a `thiserror` enum) so callers can match on
//! specific failure modes. The binary wraps these with `anyhow` for human-
//! readable top-level reporting.
//!
//! Library guide: [typed errors](https://grit-scm.com/docs/library/errors) and [embedding](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::error::Error;
//! # fn main() {
//! let _ = Error::Message("example".into());
//! # }
//! ```

use std::path::PathBuf;

use thiserror::Error;

use crate::refs::RefError;

pub use crate::git_dir::GitDirPathOperation;
pub use crate::midx_error::MidxError;
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

/// Operation stopped because [`Cancel`](crate::progress::Cancel) fired before completion.
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

    /// Human-readable summary (same text as [`Error::WorktreePathIo`] / [`Error::GitDirPathIo`]).
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

/// Why a Git config numeric value was rejected (`git config int` strict parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Why a remote could not be read or written ([`Error::RemoteUnreadable`]).
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
    /// challenge (those are [`Error::Auth`]).
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

fn remote_unreadable_message(url: Option<&str>, reason: &RemoteUnreadableReason) -> String {
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

/// Why an HTTP remote refused access ([`Error::Auth`]).
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
        /// (see [`Error::CredentialsUnavailable`]); empty when none is configured or
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
    /// and `helpers` is the list an [`Error::CredentialsUnavailable`] carries (empty
    /// for any other error). Custom [`HttpClient`](crate::transport::http::HttpClient)
    /// implementations use it to report fill failures the way the built-in client does.
    #[must_use]
    pub fn from_fill_error(err: Error) -> Self {
        let helpers = match &err {
            Error::CredentialsUnavailable { helpers } => helpers.clone(),
            _ => Vec::new(),
        };
        Self::CredentialsUnavailable {
            detail: err.to_string(),
            helpers,
        }
    }
}

/// The command a dirty worktree blocked ([`Error::DirtyWorktree`]).
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
#[derive(Debug, Error, Clone, PartialEq, Eq)]
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

/// Sparse-checkout cone pattern parse warnings (non-fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseCheckoutWarning {
    /// A cone pattern line was not recognized.
    UnrecognizedPattern {
        /// Raw pattern text from the sparse-checkout file.
        pattern: String,
    },
    /// A negative cone pattern line was not recognized.
    UnrecognizedNegativePattern {
        /// Raw negative pattern text from the sparse-checkout file.
        pattern: String,
    },
    /// The same cone pattern appears more than once in the sparse-checkout file.
    RepeatedPattern {
        /// Duplicated pattern text.
        pattern: String,
    },
    /// Cone pattern matching was disabled due to invalid sparse-checkout contents.
    DisablingConePatternMatching,
}

impl SparseCheckoutWarning {
    /// Human-readable body without a `warning:` prefix.
    #[must_use]
    pub fn body(&self) -> String {
        match self {
            Self::UnrecognizedPattern { pattern } => {
                format!("unrecognized pattern: '{pattern}'")
            }
            Self::UnrecognizedNegativePattern { pattern } => {
                format!("unrecognized negative pattern: '{pattern}'")
            }
            Self::RepeatedPattern { pattern } => format!(
                "your sparse-checkout file may have issues: pattern '{pattern}' is repeated"
            ),
            Self::DisablingConePatternMatching => "disabling cone pattern matching".to_owned(),
        }
    }

    /// Git-style `warning: …` line.
    #[must_use]
    pub fn format_line(&self) -> String {
        crate::diagnostics::warning_line(&self.body())
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

    /// Patch path bytes are not valid UTF-8.
    #[error("diff path is not valid UTF-8: {detail}")]
    DiffPathNotUtf8 {
        /// Underlying UTF-8 error detail.
        detail: String,
    },

    /// Traditional unified diff without a recognizable `---` / `+++` filename.
    #[error("unable to find filename in traditional patch")]
    TraditionalPatchFilenameMissing,

    /// Malformed `a/` or `b/` path in a `diff --git` header line.
    #[error("malformed {side} path in diff --git header: {raw}")]
    MalformedDiffGitPath {
        /// Which side failed (`old` or `new`).
        side: &'static str,
        /// Raw header fragment.
        raw: String,
    },

    /// Binary patch section size could not be parsed.
    #[error("invalid binary patch section size: {detail}")]
    InvalidBinaryPatchSectionSize {
        /// Parse failure detail.
        detail: String,
    },

    /// Binary patch payload ended before a complete section.
    #[error("binary patch section missing or truncated")]
    BinaryPatchSectionMissing,

    /// Base85 decode failure in a binary patch line.
    #[error("invalid binary patch base85: {detail}")]
    InvalidBinaryPatchBase85 {
        /// Decode error detail.
        detail: String,
    },

    /// Unexpected character while decoding binary patch length prefix.
    #[error("unexpected character in binary patch line: '{ch}'")]
    BinaryPatchUnexpectedChar {
        /// Offending character.
        ch: char,
    },

    /// Binary patch line exceeds the encoded length limit.
    #[error("binary patch line too long")]
    BinaryPatchLineTooLong,

    /// Zlib compression failed while building a binary patch.
    #[error("zlib compress failed: {detail}")]
    ZlibCompressFailed {
        /// Underlying zlib message.
        detail: String,
    },

    /// Zlib finish failed while building a binary patch.
    #[error("zlib compress finish failed: {detail}")]
    ZlibCompressFinishFailed {
        /// Underlying zlib message.
        detail: String,
    },

    /// Writing binary patch text failed.
    #[error("write binary patch {phase} failed: {detail}")]
    WriteBinaryPatchFailed {
        /// Which section was being written (`header`, `forward`, `reverse`).
        phase: &'static str,
        /// Underlying error detail.
        detail: String,
    },

    /// Inflating binary patch payload failed.
    #[error("failed to inflate binary patch payload: {detail}")]
    BinaryPatchInflateFailed {
        /// Underlying zlib message.
        detail: String,
    },

    /// Delta-style binary patch without a base blob.
    #[error("binary patch delta requires a base blob")]
    BinaryPatchDeltaRequiresBase,

    /// Binary patch lacks a reverse section.
    #[error("binary patch has no reverse section")]
    BinaryPatchNoReverseSection,

    /// Extended header line has an invalid or empty mode value.
    #[error("invalid mode on line {line}: {raw}")]
    InvalidPatchModeLine {
        /// One-based line number in the patch input.
        line: usize,
        /// Full header line text.
        raw: String,
    },

    /// Binary patch section header is not `literal` or `delta`.
    #[error("unsupported binary patch section: '{header}'")]
    UnsupportedBinaryPatchSection {
        /// Raw section header line.
        header: String,
    },

    /// A binary patch payload line was empty.
    #[error("empty binary patch payload line")]
    EmptyBinaryPatchPayloadLine,

    /// Encoded binary patch line length is outside the supported range.
    #[error("binary patch line length out of range: {raw_len}")]
    BinaryPatchLineLengthOutOfRange {
        /// Requested encoded length.
        raw_len: usize,
    },

    /// Inflated literal size does not match the declared size.
    #[error("binary patch literal size {actual} != declared {declared}")]
    BinaryPatchLiteralSizeMismatch {
        /// Size after inflation.
        actual: usize,
        /// Size from the section header.
        declared: usize,
    },

    /// Hunk header line could not be parsed.
    #[error("invalid hunk header: {header}: {detail}")]
    InvalidHunkHeader {
        /// Raw `@@` line.
        header: String,
        /// Parse failure detail.
        detail: String,
    },

    /// Hunk range field could not be parsed.
    #[error("invalid hunk range: {detail}")]
    InvalidHunkRange {
        /// Parse failure detail.
        detail: String,
    },

    /// Other apply parse failures.
    #[error("{0}")]
    Other(String),

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

    /// The caller's [`crate::progress::Cancel`] or
    /// Cancellation stopped the apply between files.
    #[error("operation cancelled")]
    Cancelled,
}

impl ApplyError {
    /// Stable kind slug for all apply failures (`apply`).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        "apply"
    }
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
    #[error("{0}")]
    InvalidGlobals(String),
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
    #[error("'{0}' is not a usable attribute name")]
    InvalidName(String),
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

/// Why a path could not be opened as a Git repository; carried by [`Error::NotARepository`].
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

fn maintenance_running_message(holder: &crate::maintenance::GcLockHolder) -> String {
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

/// The top-level error type for all grit-lib operations.
///
/// # Examples
///
/// ```
/// use grit_lib::error::Error;
///
/// let err = Error::Message("repository not found".into());
/// assert!(err.to_string().contains("repository not found"));
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A repository could not be found or is structurally invalid.
    #[error("not a git repository (or any of the parent directories): {0}")]
    NotARepository(NotARepositoryReason),

    /// A bare repository was found but access is forbidden by safe.bareRepository.
    #[error("cannot use bare repository '{0}' (safe.bareRepository is 'explicit')")]
    ForbiddenBareRepository(String),

    /// The repository is owned by a different user (safe.directory).
    #[error("detected dubious ownership in repository at '{0}'")]
    DubiousOwnership(String),

    /// Repository format version is not supported by this implementation.
    #[error("unsupported repository format version '{0}'")]
    UnsupportedRepositoryFormatVersion(u32),

    /// `extensions.refStorage` names an unsupported backend.
    #[error("invalid value for 'extensions.refstorage': '{value}'")]
    InvalidRefStorageFormat {
        /// Raw `extensions.refStorage` value from the repository config.
        value: String,
    },

    /// [`crate::repo::Repository::init_with_outcome`] refused to change object format on reinit.
    #[error("cannot change object format in an existing repository")]
    InitObjectFormatChange,

    /// [`crate::repo::Repository::init_with_outcome`] with incompatible `--bare` and separate git dir.
    #[error("cannot combine bare repository with separate git directory")]
    InitBareWithSeparateGitDir,

    /// A supplied object ID string was not valid hex or the wrong length.
    #[error("invalid object id '{0}'")]
    InvalidObjectId(String),

    /// SHA-1 object hashing detected a chosen-prefix collision (sha1dc).
    #[error("SHA-1 hash collision detected for object {oid}")]
    HashCollision {
        /// Plain SHA-1 digest for the colliding input (not safe to use as an object id).
        oid: crate::objects::ObjectId,
    },

    /// The requested object does not exist in the object store.
    #[error("object not found: {0}")]
    ObjectNotFound(String),

    /// A partial-clone promisor object is not present locally and no fetcher ran.
    #[error("promised object not available locally: {0}")]
    PromisedObjectMissing(crate::objects::ObjectId),

    /// Promised objects could not be fetched: every promisor remote was asked, and some
    /// failed. [`Self::PromisedObjectMissing`] is used instead when every remote answered
    /// but none had the object.
    #[error("{}", promisor_fetch_failed_message(missing, failures))]
    PromisorFetchFailed {
        /// The first object still missing after every remote was tried.
        missing: crate::objects::ObjectId,
        /// One entry per remote that failed, in the order they were tried: the remote's
        /// name and its first error.
        failures: Vec<(String, Error)>,
    },

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {0}")]
    CorruptObject(String),

    /// A pack index (`.idx`) file's trailing checksum does not match its contents.
    ///
    /// Only reported by reads that verify the index trailer, such as
    /// [`crate::pack::read_pack_index`]; the checksum of the `.pack` the index
    /// describes is checked separately.
    #[error("corrupt object: index checksum mismatch for {}: {mismatch}", crate::git_path::user_display_path(.path))]
    PackIndexChecksumMismatch {
        /// The `.idx` file that failed verification.
        path: std::path::PathBuf,
        /// Expected and stored trailer digests.
        mismatch: crate::hash::TrailerMismatch,
    },

    /// Indexing or unpacking a pack left deltas whose bases are neither in the pack nor
    /// in the object database: a thin pack without its bases, or a ref-delta cycle.
    #[error("corrupt object: {count} delta(s) could not be resolved")]
    UnresolvedDeltas {
        /// Number of deltas left unresolved.
        count: u64,
    },

    /// A pack being written that is not thin reached a delta before its base: the base
    /// is missing from the pack, or the planned deltas form a cycle. The receiver could
    /// not resolve that delta, so the pack was abandoned.
    #[error("pack delta for {target} comes before its base {base}")]
    PackDeltaBaseNotWritten {
        /// The object that would have been stored as a delta.
        target: crate::objects::ObjectId,
        /// Its delta base, not written ahead of it.
        base: crate::objects::ObjectId,
    },

    /// [`crate::objects::Objects::write`] refused a payload that `git fsck` would reject;
    /// nothing was written.
    #[error("refusing to write invalid {kind} object: {}", .check.report_line())]
    InvalidObject {
        /// Kind the payload was meant to be stored as.
        kind: crate::objects::ObjectKind,
        /// The first fsck rule the payload breaks (`check.id` is Git's fsck message id).
        check: crate::fsck_standalone::FsckError,
    },

    /// [`crate::objects::Objects::write_tree`] refused an entry.
    #[error(transparent)]
    InvalidTreeEntry(#[from] crate::objects::TreeEntryError),

    /// A [`crate::ident::Signature`] cannot be written as an author, committer or tagger line.
    #[error(transparent)]
    InvalidSignature(#[from] crate::ident::SignatureError),

    /// An object exists but has a different kind than the operation requires, such as a
    /// tree or blob where a commit was expected.
    #[error("object {oid} is a {actual}, not a {expected}")]
    UnexpectedObjectKind {
        /// The object that was read.
        oid: crate::objects::ObjectId,
        /// The kind the operation required.
        expected: crate::objects::ObjectKind,
        /// The kind the object actually has.
        actual: crate::objects::ObjectKind,
    },

    /// A commit-graph write was given a commit whose parent is neither among the commits
    /// being written nor in the base graph layers, so its parent edge cannot be encoded.
    #[error("commit-graph cannot encode parent {parent} of commit {commit}: parent not in graph")]
    CommitGraphParentMissing {
        /// Commit whose parent list was being encoded.
        commit: crate::objects::ObjectId,
        /// Parent that is missing from the graph.
        parent: crate::objects::ObjectId,
    },

    /// In-pack delta resolution exceeded the read-side chain depth limit (guards
    /// against cyclic ref-delta chains and unbounded recursion; Git has no read limit).
    #[error("delta chain too deep (limit {limit})")]
    DeltaChainTooDeep {
        /// Maximum delta chain depth allowed while reading packed objects.
        limit: usize,
    },

    /// An unsupported or unknown object type was encountered.
    #[error("unknown object type '{0}'")]
    UnknownObjectType(String),

    /// Loose object header type field exceeds Git's 32-byte limit.
    #[error("header for {oid} too long, exceeds 32 bytes")]
    ObjectHeaderTooLong {
        /// Object id (hex) of the loose object with an oversized header.
        oid: String,
    },

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

    /// A zlib compression or decompression failure.
    #[error("zlib error: {0}")]
    Zlib(String),

    /// A loose object file is present but its stored bytes could not be decoded.
    #[error("loose object {oid} at {} is unreadable: {source}", crate::git_path::user_display_path(.path))]
    #[non_exhaustive]
    CorruptLooseObject {
        /// Object id implied by the loose object path.
        oid: crate::objects::ObjectId,
        /// Path to the loose object file (often under `.git/objects/`).
        path: std::path::PathBuf,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<Error>,
    },

    /// A pack entry names the object but its stored bytes could not be decoded.
    #[error(
        "packed object {oid} in {} (offset {offset}) is unreadable: {source}",
        pack.display()
    )]
    #[non_exhaustive]
    CorruptPackedObject {
        /// Object id from the pack index.
        oid: crate::objects::ObjectId,
        /// Path to the `.pack` file.
        pack: std::path::PathBuf,
        /// Byte offset of the object record in the pack.
        offset: u64,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<Error>,
    },

    /// Loose object bytes hash to a different OID than the file path implies (`git fsck` / `read_loose_object`).
    #[error("{real_oid}: hash-path mismatch, found at: {path}")]
    LooseHashMismatch {
        /// Repository-relative or filesystem path to the loose object file.
        path: String,
        /// Hex object id of the unpacked contents.
        real_oid: String,
    },

    /// A prepared loose object could not be written during parallel index staging.
    #[error("could not store {path}: {source}")]
    LooseObjectWrite {
        /// Index-relative path of the blob being stored.
        path: String,
        /// Underlying filesystem error from the object store.
        source: std::io::Error,
    },

    /// Filesystem I/O failed on a repository-relative worktree path (checkout, staging, diff).
    #[error("{}", crate::worktree::io_error::format_worktree_path_io_message(.path, *operation, .source))]
    WorktreePathIo {
        /// Index- or worktree-relative path grit was operating on.
        path: String,
        /// What the operation was trying to do when I/O failed.
        operation: WorktreePathOperation,
        /// Underlying OS error.
        source: std::io::Error,
    },

    /// Filesystem I/O failed on a path under the repository git directory (objects, refs, logs, …).
    #[error("{}", crate::git_dir::io_error::format_git_dir_io_message(.path, *operation, .source))]
    GitDirPathIo {
        /// Display path such as `.git/objects` or `.git/refs/heads/main`.
        path: String,
        /// What the operation was trying to do when I/O failed.
        operation: GitDirPathOperation,
        /// Underlying OS error.
        source: std::io::Error,
    },

    /// A streamed object write or hash ended before the declared size was read.
    #[error("object stream ended after {actual} of {expected} bytes")]
    ObjectStreamTruncated {
        /// Payload size the caller declared.
        expected: u64,
        /// Bytes the stream delivered before end of file.
        actual: u64,
    },

    /// A worktree file hashed earlier in an operation had different content when stored.
    #[error("{path} changed while it was being added")]
    WorktreeFileChanged {
        /// Index-relative path of the file.
        path: String,
    },

    /// The index file is missing, truncated, or has a bad header.
    #[error("index error: {0}")]
    IndexError(String),

    /// Stage-1/2/3 entries are present; a stage-0 tree cannot be built.
    #[error("index contains unmerged entries")]
    IndexUnmerged,

    /// Another maintenance run holds the repository's `gc.pid` lock
    /// ([`crate::maintenance::GcLock`]); nothing was changed.
    #[error("{}", maintenance_running_message(holder))]
    MaintenanceRunning {
        /// Process recorded in `gc.pid`.
        holder: crate::maintenance::GcLockHolder,
    },

    /// Staging ([`crate::porcelain::add::stage`] with explicit paths, or the auto-stage pass of
    /// [`crate::porcelain::commit::commit_worktree`]) refused paths that still contain conflict
    /// markers: either they are unmerged, or their conflict stages were dropped (the index keeps
    /// a resolve-undo record) and at least one marker block is not in the `HEAD` version. A dropped-stage
    /// path stops the whole staging pass before the index is written.
    #[error("unmerged paths still have conflict markers")]
    UnmergedConflictMarkers {
        /// Repository-relative paths that were not staged because markers remain.
        paths: Vec<String>,
    },

    /// A file path is a prefix of another index path at the same tree level.
    #[error(
        "index path prefix conflict between '{}' and '{}'",
        directory.display(),
        file.display()
    )]
    IndexPathPrefixConflict {
        /// Shorter path (treated as the ambiguous directory prefix).
        directory: std::path::PathBuf,
        /// Longer path that extends the prefix with an additional component.
        file: std::path::PathBuf,
    },

    /// The `FSMN` index extension ends before its header, token, or EWAH payload.
    #[error("index fsmonitor extension payload is truncated")]
    IndexFsmonitorExtensionTruncated,

    /// The `FSMN` extension header version is not supported.
    #[error("index fsmonitor extension has unsupported header version {0}")]
    IndexFsmonitorExtensionBadVersion(u32),

    /// The EWAH bitmap embedded in `FSMN` could not be decoded.
    #[error("index fsmonitor extension EWAH bitmap is invalid")]
    IndexFsmonitorExtensionEwahInvalid,

    /// The cache-tree extension references more entries than the index contains. Git emits this
    /// (verbatim, prefixed with `error: `) when a tree with duplicate path entries is read into the
    /// index (`t4058-diff-duplicates`).
    #[error("corrupted cache-tree has entries not present in index")]
    CacheTreeCorrupt,

    /// Reference storage, resolution, HEAD state, and reflog failures.
    #[error(transparent)]
    Ref(#[from] crate::refs::RefError),

    /// Git notes read, write, and merge failures.
    #[error(transparent)]
    Notes(#[from] crate::notes::NotesError),

    /// A general path-related error (invalid UTF-8, out-of-bounds, etc.).
    #[error("path error: {0}")]
    PathError(String),

    /// `--pathspec-from-file` parse failure.
    #[error(transparent)]
    PathspecFromFile(#[from] PathspecFromFileParseError),

    /// A tree/index path component is forbidden (`.git`, HFS/NTFS aliases, etc.).
    #[error("invalid path '{0}'")]
    InvalidPath(String),

    /// A repository-relative path failed [`crate::path_validate`] rules while populating the index or checkout.
    #[error("invalid path '{path}': {reason}")]
    InvalidPathComponent {
        /// Full repository-relative path that was rejected.
        path: String,
        /// Which component rule failed.
        reason: crate::path_validate::PathComponentError,
    },

    /// A configuration file parsing or access error.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Submodule git dir layout or submodule state failure.
    #[error(transparent)]
    Submodule(#[from] SubmoduleError),

    /// Branch create, delete, or upstream configuration failure.
    #[error(transparent)]
    Branch(#[from] crate::porcelain::branch_error::BranchError),

    /// Filter or EOL conversion failure.
    #[error(transparent)]
    Filter(#[from] FilterError),

    /// Revision parsing, walking, merge-base, and object-filter failure.
    #[error(transparent)]
    Revision(#[from] crate::revision_error::RevisionError),

    /// Diff, patch apply, and blame failure.
    #[error(transparent)]
    Diff(#[from] crate::diff::error::DiffError),

    /// Path resolves outside the repository.
    #[error(transparent)]
    PathOutsideRepo(#[from] PathOutsideRepoError),

    /// A commit/tag signing or signature-verification error.
    #[error("{0}")]
    Signing(String),

    /// HTTP authentication failed: the server required credentials (`401`) and
    /// either no credential provider was wired, the provider could not supply a
    /// usable username/password, the server demanded an unsupported auth scheme,
    /// or the supplied credentials were rejected.
    ///
    /// Distinct from [`Error::Message`] so embedders can detect an authentication
    /// failure (and e.g. fall back to an interactive/subprocess path) rather than
    /// string-matching, and so the failure surfaces typed instead of hanging.
    #[error("authentication failed for {url}: {reason}")]
    Auth {
        /// Repository URL with credentials redacted and smart-HTTP endpoint suffixes
        /// (`/info/refs?service=…`, `/git-upload-pack`, `/git-receive-pack`) removed.
        url: String,
        /// What went wrong.
        reason: AuthFailure,
    },

    /// A credential provider could not supply a complete username and password:
    /// no helper had them and no askpass program could be asked.
    ///
    /// Returned by [`HelperCredentialProvider`](crate::credentials::HelperCredentialProvider)
    /// instead of prompting on a terminal; transports surface it as
    /// [`AuthFailure::CredentialsUnavailable`].
    #[error("{}", crate::credentials::NON_INTERACTIVE_MESSAGE)]
    CredentialsUnavailable {
        /// The `credential.helper` programs that were asked, in order (empty when none
        /// is configured for the URL).
        helpers: Vec<String>,
    },

    /// An atomic wire push was requested, but the remote did not advertise the
    /// `atomic` capability. No ref-update commands or pack data were sent.
    #[error("the remote does not support atomic pushes")]
    PushAtomicUnsupported,

    /// A push carried `--push-option` values but the remote `git-receive-pack`
    /// did not advertise the `push-options` capability, so the options cannot be
    /// transmitted.
    ///
    /// Distinct from [`Error::Message`] so embedders can detect this specific
    /// negotiation failure (and e.g. fall back to a subprocess push) rather than
    /// string-matching. The message matches Git's
    /// `fatal: the receiving end does not support push options`.
    #[error("the receiving end does not support push options")]
    PushOptionsUnsupported,

    /// `git fetch --unshallow` on a repository that is not shallow.
    #[error("repository is not shallow")]
    FetchUnshallowNotShallow,

    /// A deepen/shallow fetch was requested but the remote did not advertise the
    /// `shallow` capability.
    #[error("the remote does not support shallow fetches")]
    FetchShallowUnsupported,

    /// A `--deepen` fetch was requested but the remote did not advertise
    /// `deepen-relative`.
    #[error("the remote does not support deepen-relative")]
    FetchDeepenRelativeUnsupported,

    /// A bundle fetch or verify found prerequisite commits missing from history.
    #[error("repository lacks these prerequisite commits")]
    BundleMissingPrerequisites,

    /// Streaming fetch/clone temp pack disappeared before index-pack (often real-time antivirus).
    #[error(
        "received pack file missing: {}. If this persists, check real-time antivirus or disk cleanup for quarantined pack temp files.",
        crate::git_path::user_display_path(.path)
    )]
    ReceivedPackTempMissing {
        /// Path to the temp pack that was expected on disk.
        path: std::path::PathBuf,
    },

    /// A pathspec's `:(attr:...)` magic is malformed.
    #[error("invalid attr magic in pathspec '{spec}': {reason}")]
    InvalidAttrPathspec {
        /// The offending pathspec as given.
        spec: String,
        /// What is wrong with its attr clause.
        #[source]
        reason: AttrPathspecError,
    },

    /// A pathspec did not match any file known to Git (index or HEAD tree).
    #[error("pathspec '{spec}' did not match any file(s) known to git")]
    PathspecNoMatch {
        /// User-supplied pathspec that matched nothing.
        spec: String,
    },

    /// [`crate::porcelain::restore::restore_paths`] conflict options were combined with `--staged`.
    #[error("conflict restore options cannot be used with --staged")]
    RestoreConflictWithStaged,

    /// [`crate::porcelain::restore::restore_paths`] conflict options were combined with `--source`.
    #[error("conflict restore options cannot be used with --source")]
    RestoreConflictWithSource,

    /// A conflict side was missing from the index for the requested path.
    #[error("path '{path}' does not have their version")]
    RestoreConflictMissingTheirs {
        /// Repository-relative path.
        path: String,
    },

    /// A conflict side was missing from the index for the requested path.
    #[error("path '{path}' does not have our version")]
    RestoreConflictMissingOurs {
        /// Repository-relative path.
        path: String,
    },

    /// Three-way remerge needs stages 1, 2, and 3 in the index.
    #[error("path '{path}' is not unmerged")]
    RestoreConflictNotUnmerged {
        /// Repository-relative path.
        path: String,
    },

    /// [`crate::porcelain::restore::restore_paths`] was asked to restore the worktree from the
    /// index for paths that still have conflict stages; pick a side or rebuild the markers instead.
    ///
    /// Nothing is written when this is returned.
    #[error("{}", restore_unmerged_message(paths))]
    RestorePathUnmerged {
        /// Matched repository-relative paths with conflict stages (sorted, deduplicated).
        paths: Vec<String>,
    },

    /// A literal pathspec names a file under a symlink directory component.
    #[error("pathspec '{spec}' is beyond a symbolic link")]
    PathspecBeyondSymlink {
        /// User-supplied pathspec that crosses a symlink directory.
        spec: String,
    },

    /// [`crate::porcelain::commit::CommitScope::Paths`] was used while a merge is in progress.
    #[error("cannot do a partial commit during a merge")]
    PartialCommitDuringMerge,

    /// Partial commit, amend, or patch commit while a cherry-pick or revert is in progress.
    #[error("cannot do a partial commit during a cherry-pick or revert")]
    PartialCommitDuringReplay,

    /// [`continue_replay_sequence`](crate::porcelain::replay_sequence::continue_replay_sequence) with a dirty index after the step was already committed.
    #[error("cannot continue replay: index differs from HEAD after the step was committed")]
    ReplayContinueDirtyIndex,

    /// [`skip_replay_step`](crate::porcelain::replay_sequence::skip_replay_step) when the current step was already committed on a moved branch tip.
    #[error("nothing to skip: current step already committed; run continue instead")]
    ReplaySkipAlreadyCommitted,

    /// An embedded git repository (nested `.git` checkout) has no commit checked out on its
    /// current branch, so it cannot be recorded as a gitlink or as regular files.
    #[error("embedded git repository '{path}' has no commit checked out")]
    EmbeddedRepoWithoutCommit {
        /// Repository-relative path to the nested checkout directory.
        path: String,
    },

    /// Explicit [`crate::porcelain::add::stage`] pathspecs name paths excluded by ignore rules.
    #[error("the following paths are ignored:\n{}", paths.join("\n"))]
    PathspecIgnored {
        /// Repository-relative paths (or user labels) that cannot be staged while ignored.
        paths: Vec<String>,
    },

    /// [`crate::porcelain::paths::remove_paths`] refused paths whose removal would lose
    /// content, because [`crate::porcelain::paths::RemoveOptions::force`] was not set.
    #[error(
        "refusing to remove paths with unsaved content:\n{}",
        paths.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    )]
    RemoveBlocked {
        /// Each refused path with the reason it was refused.
        paths: Vec<crate::porcelain::paths::BlockedRemoval>,
    },

    /// A directory pathspec was used without `recursive` ([`crate::porcelain::paths::remove_paths`]).
    #[error("not removing '{0}' recursively without -r")]
    NotRemovingRecursively(String),

    /// A porcelain path operation requires a checked-out work tree.
    #[error("this operation must be run in a work tree")]
    RequiresWorkTree,

    /// A commit was requested with an empty message body after normalization.
    #[error("empty commit message")]
    EmptyCommitMessage,

    /// [`crate::porcelain::paths::remove_paths`] was called with no pathspecs.
    #[error("you must specify path(s) to remove")]
    RemovePathsRequired,

    /// [`crate::porcelain::commit::commit_with_scope`] was called with an empty
    /// [`CommitScope::Paths`](crate::porcelain::commit::CommitScope::Paths) pathspec list.
    #[error("you must specify path(s)")]
    CommitPathsRequired,

    /// [`crate::porcelain::paths::move_path`] / [`crate::porcelain::paths::move_paths`] source is not tracked.
    #[error("'{src}' is not under version control")]
    MoveSourceNotTracked {
        /// Untracked or missing source path.
        src: String,
    },

    /// Move refused because the destination already exists without `--force`.
    #[error("destination '{destination}' already exists")]
    MoveDestinationExists {
        /// Source path in the move request.
        src: String,
        /// Existing destination path.
        destination: String,
    },

    /// Move refused because a trailing slash named a destination that is not a directory.
    #[error("destination '{destination}' is not a directory")]
    MoveDestinationNotDirectory {
        /// Destination path (trailing slash already removed).
        destination: String,
    },

    /// Move refused because the destination lies under the source directory.
    #[error("cannot move '{src}' to a subdirectory of itself, '{destination}'")]
    MoveIntoSubdirectory {
        /// Source directory path.
        src: String,
        /// Invalid destination under the source.
        destination: String,
    },

    /// Move refused because source and destination are the same path.
    #[error("cannot move '{src}' into itself")]
    MoveIntoSelf {
        /// Source path.
        src: String,
        /// Destination equal to the source.
        destination: String,
    },

    /// Move refused because the source has no index entries.
    #[error("nothing to move from '{src}'")]
    MoveNothingToMove {
        /// Source path with no index entries.
        src: String,
    },

    /// Multi-source move would write more than one source to the same destination path.
    #[error("multiple sources would move to '{destination}'")]
    MoveDuplicateDestination {
        /// Destination path that would receive more than one source.
        destination: String,
    },

    /// A worktree source path is missing or not a file, directory, or symlink.
    #[error("cannot move '{src}' to '{destination}': bad source path")]
    MoveBadSource {
        /// Source path.
        src: String,
        /// Planned destination path.
        destination: String,
    },

    /// Internal invariant: a planned rename references a missing index entry.
    #[error("internal error: missing index entry for '{path}'")]
    MoveMissingIndexEntry {
        /// Index path that should exist during a move.
        path: String,
    },

    /// The remote could not be read or written: a missing repository, a failed connection, an
    /// HTTP error status, or no Git handshake.
    ///
    /// Use [`Error::is_remote_unreadable`] to test for it through wrapper errors.
    ///
    /// Not used for cancellation; see [`Error::is_cancelled`].
    #[error("{}", remote_unreadable_message(url.as_deref(), reason))]
    RemoteUnreadable {
        /// Remote URL or resolved local path, with credentials redacted. `None` when the
        /// failing layer only had a connection, not the URL it was opened from.
        url: Option<String>,
        /// What went wrong.
        reason: RemoteUnreadableReason,
    },

    /// The remote was reached but aborted the conversation with its own error text: an
    /// `ERR <message>` pkt-line (for example upload-pack declining a ref it does not
    /// serve) or a packet on side-band channel 3.
    ///
    /// The text is the server's, not grit's: show it to the user, but do not parse it.
    #[error("remote error: {message}")]
    RemoteFatal {
        /// The server's message, without the `ERR ` prefix or trailing whitespace.
        message: String,
    },

    /// The upload-pack serving a local path or `file://` URL in this process failed
    /// (for example it was asked for an object it does not have). Repository errors
    /// of the served side are returned as themselves instead.
    #[error("local upload-pack: {0}")]
    LocalUploadPack(Box<crate::serve::ServeError>),

    /// The thread serving a local path or `file://` URL panicked.
    #[error("local upload-pack stopped unexpectedly")]
    LocalUploadPackPanicked,

    /// A resumable bundle download or upload failed.
    #[error(transparent)]
    Resumable(#[from] Box<crate::resumable::ResumableError>),

    /// Explicit fetch refspec resolution failed (missing or ambiguous source).
    #[error("{0}")]
    FetchRefspec(#[from] crate::fetch_request::FetchRefspecError),

    /// Fetch refused to update the branch checked out in this or a linked worktree.
    #[error("cannot fetch into checked-out branch '{branch}'")]
    FetchIntoCheckedOutBranch {
        /// Short branch name under `refs/heads/`.
        branch: String,
    },

    /// Checkout or switch would overwrite local or untracked worktree content.
    #[error("{message}")]
    WouldOverwrite {
        /// Whether tracked local edits or untracked paths block the operation.
        reason: OverwriteReason,
        /// Repository-relative paths that would be overwritten (sorted, deduplicated).
        paths: Vec<String>,
        /// Human-readable summary (depends on [`OverwriteReason`]).
        message: String,
    },

    /// Staged or unstaged changes block an operation that needs a clean worktree.
    #[error("you have uncommitted changes — commit them before {operation}")]
    DirtyWorktree {
        /// The operation that was refused.
        operation: WorktreeOperation,
        /// Repository-relative paths with staged or unstaged changes (sorted, deduplicated;
        /// empty when the caller only knew that something changed).
        paths: Vec<String>,
    },

    /// Author or committer on [`crate::porcelain::commit::CommitRequest`] is invalid.
    #[error(transparent)]
    InvalidIdentity(#[from] crate::ident::CommitIdentityError),

    /// A long-running library operation stopped at a cancellation checkpoint.
    ///
    /// Use [`Error::is_cancelled`] to test for it through wrapper errors
    /// ([`crate::remote::RemoteError::is_cancelled`], [`crate::remote::clone::CloneError::is_cancelled`], …).
    #[error("operation cancelled")]
    Cancelled,

    /// A checkout failed or was cancelled before it finished, and putting the work tree back
    /// failed for some paths.
    ///
    /// Refs and the index are unchanged, as after any failed checkout; only `unrestored`
    /// differ from the index. Use [`Error::is_cancelled`] to see whether `source` was a
    /// cancellation.
    #[error("{source}; {} work-tree path(s) could not be restored", unrestored.len())]
    CheckoutRollbackIncomplete {
        /// Why the checkout stopped.
        source: Box<Error>,
        /// Repository-relative paths left different from the index (sorted, deduplicated).
        unrestored: Vec<String>,
    },

    /// No usable author or committer identity is configured
    /// ([`crate::repo::Repository::identity`]).
    #[error(transparent)]
    Identity(#[from] crate::config::ident_resolve::IdentityError),

    /// User-facing message that should be printed verbatim (no extra prefix).
    ///
    /// Used for revision errors that must match Git's `fatal:` lines exactly.
    #[error("{0}")]
    Message(String),

    /// A merge was requested with fast-forward only, but the histories diverged.
    #[error("not possible to fast-forward; refusing to merge")]
    NotFastForward,

    /// The index tree matches the parent commit tree and [`allow_empty`](crate::porcelain::commit::CommitRequest::allow_empty) is false.
    #[error("nothing to commit")]
    NothingToCommit,

    /// No staged changes are available for a [`CommitScope::Staged`](crate::porcelain::commit::CommitScope::Staged)
    /// commit, but the work tree has unstaged or untracked paths.
    #[error("nothing staged to commit")]
    NothingStagedToCommit {
        /// Tracked paths that differ between the index and the work tree.
        unstaged: usize,
        /// Untracked paths under the work tree.
        untracked: usize,
    },

    /// [`replay_commit`](crate::porcelain::replay::replay_commit) does not replay merge commits
    /// until callers can select a mainline parent explicitly.
    #[error("merge commit {oid}")]
    MergeCommit {
        /// Object id of the merge commit that cannot be replayed yet.
        oid: crate::objects::ObjectId,
    },

    /// Cherry-pick source is already checked out as `HEAD`.
    #[error("commit already at HEAD")]
    ReplaySourceAtHead,

    /// Sequencer todo lists a command grit does not implement yet.
    #[error("unknown sequencer command '{command}'")]
    SequencerUnknownCommand {
        /// Verb read from `.git/sequencer/todo`.
        command: String,
    },

    /// [`continue_replay_sequence`](crate::porcelain::replay_sequence::continue_replay_sequence) refuses while unmerged index entries remain.
    #[error("cannot continue replay: unmerged index entries remain")]
    ReplayContinueUnmerged,

    /// No cherry-pick or revert sequence is in progress.
    #[error("no cherry-pick or revert in progress")]
    NoReplayInProgress,

    /// [`crate::porcelain::merge::abort_branch_merge`] was called with no merge in progress.
    #[error("there is no merge in progress")]
    NoMergeInProgress,

    /// The command would move `HEAD` while a paused operation still depends on it
    /// (see [`operation_blocking_switch`](crate::state::operation_blocking_switch)).
    #[error("{} in progress", .0.description())]
    OperationInProgress(crate::state::InProgressOperation),

    /// Tag create/delete validation failure ([`crate::porcelain::tag::TagError`]).
    #[error(transparent)]
    Tag(#[from] crate::porcelain::tag::TagError),

    /// Linked worktree add/remove/move/lock/repair failure ([`crate::worktree::WorktreeError`]).
    #[error(transparent)]
    Worktree(#[from] crate::worktree::WorktreeError),

    /// Stash commit object has fewer parents or an invalid shape for apply/show.
    #[error("corrupt stash commit: {0}")]
    CorruptStash(&'static str),

    /// `stash@{{n}}` is out of range for the current stash reflog.
    #[error("stash entry stash@{{{n}}} not found")]
    StashNotFound {
        /// Stash reflog index (`stash@{{n}}`).
        n: usize,
    },

    /// A stash commit id was given to [`crate::repo_handles::RepositoryStash::pop`] but is not on
    /// `refs/stash` (use [`crate::repo_handles::RepositoryStash::apply_commit`] to recover it).
    #[error("stash commit {oid} is not on the stash stack")]
    StashCommitNotOnStack {
        /// Stash W commit object id.
        oid: crate::objects::ObjectId,
    },

    /// Pushing or creating a stash ([`crate::repo_handles::RepositoryStash::push`]) requires at least one commit.
    #[error("cannot stash without a commit")]
    StashNoInitialCommit,

    /// Stash create/apply/pop requires a working tree (not a bare repository).
    #[error("cannot stash in a bare repository")]
    StashBareRepository,

    /// [`crate::repo_handles::RepositoryStash::apply`] with `restore_index` could
    /// not merge the current index with the stashed index parent.
    #[error("conflicts in index while applying stash with --index")]
    StashIndexConflict,

    /// Incompatible [`StashPushOptions`](crate::stash::StashPushOptions) flags.
    #[error("{0}")]
    StashOptionConflict(String),

    /// Partial [`crate::repo_handles::RepositoryStash::push`] hunk selection failed.
    #[error(transparent)]
    HunkSelection(#[from] crate::porcelain::hunk_select::HunkSelectError),

    /// HEAD does not resolve while applying a stash entry.
    #[error("missing HEAD while applying stash")]
    StashMissingHead,

    /// Applying a stash would overwrite locally modified worktree content.
    #[error(
        "your local changes to the following files would be overwritten by stash apply: {paths}"
    )]
    StashWouldOverwriteLocalChanges {
        /// Space-separated list of paths that would be overwritten.
        paths: String,
    },

    /// The index could not be written after a successful stash apply.
    #[error("writing index after stash apply: {0}")]
    StashIndexWrite(String),

    /// A symlink blob in a stash commit is not valid UTF-8.
    #[error("symlink target is not UTF-8")]
    StashSymlinkNotUtf8,

    /// Applying a stash with an untracked-files parent would replace an existing worktree path.
    #[error("stash untracked restore blocked: '{path}' already exists on disk")]
    StashCouldNotRestoreUntracked {
        /// Worktree path that already exists and blocks untracked restore.
        path: String,
    },

    /// [`CommitRequest::amend`](crate::porcelain::commit::CommitRequest::amend) was set on an unborn branch.
    #[error("cannot amend: no commits on this branch yet")]
    AmendUnborn,

    /// Multi-pack-index load or write failure.
    #[error(transparent)]
    Midx(#[from] MidxError),

    /// Offloading blobs to a large-object promisor was refused
    /// ([`crate::maintenance::offload_blobs`]).
    #[error(transparent)]
    Offload(#[from] crate::offload::OffloadError),

    /// A Git hook subprocess failed or could not be started.
    #[error(transparent)]
    Hook(crate::hooks::HookError),

    /// History rewrite planning failed ([`crate::history::HistoryError`]).
    #[error(transparent)]
    History(#[from] crate::history::HistoryError),

    /// The operation needs the files-backed `objects/` directory, but the repository's
    /// [`Odb`](crate::odb::Odb) uses a custom [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary)
    /// store. Nothing was written; use a files-backed repository for this operation.
    #[error("{operation} requires the files-backed object store; it is not supported with a custom primary")]
    UnsupportedObjectStore {
        /// Operation that was refused (for example `"fetch"`, `"push"`, `"gc"`).
        operation: &'static str,
    },

    /// The operation fetches objects back from a promisor remote, and the repository
    /// has none: no `remote.<name>.promisor = true` and no `extensions.partialClone`.
    /// Nothing was changed.
    #[error("{operation} needs a promisor remote to fetch objects back from")]
    NoPromisorRemote {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },

    /// The operation would delete objects, and `extensions.preciousObjects` says this
    /// repository never loses any (it may be shared as an alternate). Nothing was
    /// changed.
    #[error("{operation} deletes objects, which extensions.preciousObjects forbids here")]
    PreciousObjectsRepository {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },

    /// [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) was given an
    /// [`OdbBuilder`](crate::odb::OdbBuilder) rooted somewhere other than the repository's
    /// `objects/` directory.
    #[error(
        "object database {} is not the repository's objects directory {}",
        builder.display(),
        repository.display()
    )]
    OdbObjectsDirMismatch {
        /// The builder's objects directory, canonicalized when it exists.
        builder: std::path::PathBuf,
        /// The repository's canonical objects directory.
        repository: std::path::PathBuf,
    },

    /// [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) was given an
    /// [`OdbBuilder`](crate::odb::OdbBuilder) whose custom primary or read source hashes
    /// objects with a different algorithm than the repository's `extensions.objectformat`
    /// (SHA-1 when unset).
    #[error(
        "object store hashes with {} but the repository's object format is {}",
        store.name(),
        repository.name()
    )]
    #[non_exhaustive]
    ObjectFormatMismatch {
        /// The repository's object format from its config.
        repository: crate::objects::HashAlgo,
        /// The first injected store's [`hash_algo`](crate::odb::store::ObjectStore::hash_algo)
        /// that differs from `repository`.
        store: crate::objects::HashAlgo,
    },

    /// Parsing a [`crate::tree_sha256::TreeSha256`] from hex failed (expected 64 hex digits).
    #[error("invalid tree-sha256 hex")]
    TreeSha256InvalidHex,

    /// A revision passed to [`crate::tree_sha256::tree_sha256`] did not peel to a commit or tree.
    #[error("tree-sha256: revision resolves to {found:?}, not a commit or tree")]
    TreeSha256InvalidRevision {
        /// Object kind after peeling annotated tags.
        found: crate::objects::ObjectKind,
    },

    /// One or more submodules required for [`crate::tree_sha256::tree_sha256`] are missing.
    #[error("tree-sha256: missing submodules")]
    TreeSha256MissingSubmodules(Vec<crate::tree_sha256::MissingSubmodule>),

    /// `--tree-sha256` was requested with signing explicitly disabled.
    #[error("tree-sha256 requires a signed tag")]
    TreeSha256RequiresSigning,

    /// [`crate::tree_sha256::verify_tag`] was given an object that is not an annotated tag.
    #[error("verify-tag: object {oid} is a {found:?}, not an annotated tag")]
    TagVerifyNotTag {
        /// Object id passed to verification.
        oid: crate::objects::ObjectId,
        /// Kind stored in the ODB.
        found: crate::objects::ObjectKind,
    },

    /// Annotated tag header block has no `object` line.
    #[error("verify-tag: tag has no object header")]
    TagVerifyMissingObjectHeader,

    /// Annotated tag carries more than one `tree-sha256` header line.
    #[error("verify-tag: tag has more than one tree-sha256 header")]
    TagVerifyDuplicateTreeSha256Header,

    /// Annotated tag tree-sha256 header parsing failed.
    #[error(transparent)]
    TreeSha256Claim(#[from] crate::tree_sha256::TreeSha256ClaimError),
}

impl Error {
    /// Stable snake_case name of this error's variant (for example `"not_a_repository"`),
    /// for scripts and serialized output that must branch on the failure kind.
    ///
    /// The name is the variant name in snake_case; it changes only when the variant is
    /// renamed. Match on the error value itself in Rust code.
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::error::Error;
    ///
    /// assert_eq!(Error::NothingToCommit.kind_name(), "nothing_to_commit");
    /// assert_eq!(Error::Cancelled.kind_name(), "cancelled");
    /// ```
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NotARepository(..) => "not_a_repository",
            Self::ForbiddenBareRepository(..) => "forbidden_bare_repository",
            Self::DubiousOwnership(..) => "dubious_ownership",
            Self::UnsupportedRepositoryFormatVersion(..) => "unsupported_repository_format_version",
            Self::InvalidRefStorageFormat { .. } => "invalid_ref_storage_format",
            Self::InitObjectFormatChange => "init_object_format_change",
            Self::InitBareWithSeparateGitDir => "init_bare_with_separate_git_dir",
            Self::InvalidObjectId(..) => "invalid_object_id",
            Self::HashCollision { .. } => "hash_collision",
            Self::ObjectNotFound(..) => "object_not_found",
            Self::PromisedObjectMissing(..) => "promised_object_missing",
            Self::PromisorFetchFailed { .. } => "promisor_fetch_failed",
            Self::CorruptObject(..) => "corrupt_object",
            Self::PackIndexChecksumMismatch { .. } => "pack_index_checksum_mismatch",
            Self::UnresolvedDeltas { .. } => "unresolved_deltas",
            Self::PackDeltaBaseNotWritten { .. } => "pack_delta_base_not_written",
            Self::InvalidObject { .. } => "invalid_object",
            Self::InvalidTreeEntry(..) => "invalid_tree_entry",
            Self::InvalidSignature(..) => "invalid_signature",
            Self::UnexpectedObjectKind { .. } => "unexpected_object_kind",
            Self::CommitGraphParentMissing { .. } => "commit_graph_parent_missing",
            Self::DeltaChainTooDeep { .. } => "delta_chain_too_deep",
            Self::UnknownObjectType(..) => "unknown_object_type",
            Self::ObjectHeaderTooLong { .. } => "object_header_too_long",
            Self::Io(..) => "io",
            Self::LockHeld(..) => "lock_held",
            Self::Zlib(..) => "zlib",
            Self::CorruptLooseObject { .. } => "corrupt_loose_object",
            Self::CorruptPackedObject { .. } => "corrupt_packed_object",
            Self::LooseHashMismatch { .. } => "loose_hash_mismatch",
            Self::LooseObjectWrite { .. } => "loose_object_write",
            Self::WorktreePathIo { .. } => "worktree_path_io",
            Self::GitDirPathIo { .. } => "git_dir_path_io",
            Self::ObjectStreamTruncated { .. } => "object_stream_truncated",
            Self::WorktreeFileChanged { .. } => "worktree_file_changed",
            Self::IndexError(..) => "index_error",
            Self::IndexUnmerged => "index_unmerged",
            Self::MaintenanceRunning { .. } => "maintenance_running",
            Self::UnmergedConflictMarkers { .. } => "unmerged_conflict_markers",
            Self::IndexPathPrefixConflict { .. } => "index_path_prefix_conflict",
            Self::IndexFsmonitorExtensionTruncated => "index_fsmonitor_extension_truncated",
            Self::IndexFsmonitorExtensionBadVersion(..) => "index_fsmonitor_extension_bad_version",
            Self::IndexFsmonitorExtensionEwahInvalid => "index_fsmonitor_extension_ewah_invalid",
            Self::CacheTreeCorrupt => "cache_tree_corrupt",
            Self::Ref(e) => e.kind_name(),
            Self::Notes(e) => e.kind_name(),
            Self::PathError(..) => "path_error",
            Self::PathspecFromFile(..) => "pathspec_from_file",
            Self::InvalidPath(..) => "invalid_path",
            Self::InvalidPathComponent { .. } => "invalid_path_component",
            Self::Config(..) => "config",
            Self::Submodule(..) => "submodule",
            Self::Branch(..) => "branch",
            Self::Filter(..) => "filter",
            Self::Revision(re) => re.kind_name(),
            Self::Diff(de) => de.kind_name(),
            Self::PathOutsideRepo(..) => "path_outside_repo",
            Self::Signing(..) => "signing",
            Self::Auth { .. } => "auth",
            Self::CredentialsUnavailable { .. } => "credentials_unavailable",
            Self::PushAtomicUnsupported => "push_atomic_unsupported",
            Self::PushOptionsUnsupported => "push_options_unsupported",
            Self::FetchUnshallowNotShallow => "fetch_unshallow_not_shallow",
            Self::FetchShallowUnsupported => "fetch_shallow_unsupported",
            Self::FetchDeepenRelativeUnsupported => "fetch_deepen_relative_unsupported",
            Self::BundleMissingPrerequisites => "bundle_missing_prerequisites",
            Self::ReceivedPackTempMissing { .. } => "received_pack_temp_missing",
            Self::InvalidAttrPathspec { .. } => "invalid_attr_pathspec",
            Self::PathspecNoMatch { .. } => "pathspec_no_match",
            Self::RestoreConflictWithStaged => "restore_conflict_with_staged",
            Self::RestoreConflictWithSource => "restore_conflict_with_source",
            Self::RestoreConflictMissingTheirs { .. } => "restore_conflict_missing_theirs",
            Self::RestoreConflictMissingOurs { .. } => "restore_conflict_missing_ours",
            Self::RestoreConflictNotUnmerged { .. } => "restore_conflict_not_unmerged",
            Self::RestorePathUnmerged { .. } => "restore_path_unmerged",
            Self::PathspecBeyondSymlink { .. } => "pathspec_beyond_symlink",
            Self::PartialCommitDuringMerge => "partial_commit_during_merge",
            Self::PartialCommitDuringReplay => "partial_commit_during_replay",
            Self::ReplayContinueDirtyIndex => "replay_continue_dirty_index",
            Self::ReplaySkipAlreadyCommitted => "replay_skip_already_committed",
            Self::EmbeddedRepoWithoutCommit { .. } => "embedded_repo_without_commit",
            Self::PathspecIgnored { .. } => "pathspec_ignored",
            Self::RemoveBlocked { .. } => "remove_blocked",
            Self::NotRemovingRecursively(..) => "not_removing_recursively",
            Self::RequiresWorkTree => "requires_work_tree",
            Self::EmptyCommitMessage => "empty_commit_message",
            Self::RemovePathsRequired => "remove_paths_required",
            Self::CommitPathsRequired => "commit_paths_required",
            Self::MoveSourceNotTracked { .. } => "move_source_not_tracked",
            Self::MoveDestinationExists { .. } => "move_destination_exists",
            Self::MoveDestinationNotDirectory { .. } => "move_destination_not_directory",
            Self::MoveIntoSubdirectory { .. } => "move_into_subdirectory",
            Self::MoveIntoSelf { .. } => "move_into_self",
            Self::MoveNothingToMove { .. } => "move_nothing_to_move",
            Self::MoveDuplicateDestination { .. } => "move_duplicate_destination",
            Self::MoveBadSource { .. } => "move_bad_source",
            Self::MoveMissingIndexEntry { .. } => "move_missing_index_entry",
            Self::RemoteUnreadable { .. } => "remote_unreadable",
            Self::RemoteFatal { .. } => "remote_fatal",
            Self::LocalUploadPack(..) => "local_upload_pack",
            Self::LocalUploadPackPanicked => "local_upload_pack_panicked",
            Self::Resumable(..) => "resumable",
            Self::FetchRefspec(..) => "fetch_refspec",
            Self::FetchIntoCheckedOutBranch { .. } => "fetch_into_checked_out_branch",
            Self::WouldOverwrite { .. } => "would_overwrite",
            Self::DirtyWorktree { .. } => "dirty_worktree",
            Self::InvalidIdentity(..) => "invalid_identity",
            Self::Cancelled => "cancelled",
            Self::Identity(..) => "identity",
            Self::Message(..) => "message",
            Self::NotFastForward => "not_fast_forward",
            Self::NothingToCommit => "nothing_to_commit",
            Self::ObjectFormatMismatch { .. } => "object_format_mismatch",
            Self::CheckoutRollbackIncomplete { .. } => "checkout_rollback_incomplete",
            Self::NothingStagedToCommit { .. } => "nothing_staged_to_commit",
            Self::MergeCommit { .. } => "merge_commit",
            Self::ReplaySourceAtHead => "replay_source_at_head",
            Self::SequencerUnknownCommand { .. } => "sequencer_unknown_command",
            Self::ReplayContinueUnmerged => "replay_continue_unmerged",
            Self::NoReplayInProgress => "no_replay_in_progress",
            Self::NoMergeInProgress => "no_merge_in_progress",
            Self::OperationInProgress(..) => "operation_in_progress",
            Self::Tag(..) => "tag",
            Self::Worktree(..) => "worktree",
            Self::CorruptStash(..) => "corrupt_stash",
            Self::StashNotFound { .. } => "stash_not_found",
            Self::StashCommitNotOnStack { .. } => "stash_commit_not_on_stack",
            Self::StashNoInitialCommit => "stash_no_initial_commit",
            Self::StashBareRepository => "stash_bare_repository",
            Self::StashIndexConflict => "stash_index_conflict",
            Self::StashOptionConflict(..) => "stash_option_conflict",
            Self::HunkSelection(..) => "hunk_selection",
            Self::StashMissingHead => "stash_missing_head",
            Self::StashWouldOverwriteLocalChanges { .. } => "stash_would_overwrite_local_changes",
            Self::StashIndexWrite(..) => "stash_index_write",
            Self::StashSymlinkNotUtf8 => "stash_symlink_not_utf8",
            Self::StashCouldNotRestoreUntracked { .. } => "stash_could_not_restore_untracked",
            Self::AmendUnborn => "amend_unborn",
            Self::Midx(..) => "midx",
            Self::Offload(..) => "offload",
            Self::Hook(..) => "hook",
            Self::History(..) => "history",
            Self::UnsupportedObjectStore { .. } => "unsupported_object_store",
            Self::NoPromisorRemote { .. } => "no_promisor_remote",
            Self::PreciousObjectsRepository { .. } => "precious_objects_repository",
            Self::OdbObjectsDirMismatch { .. } => "odb_objects_dir_mismatch",
            Self::TreeSha256InvalidHex => "tree_sha256_invalid_hex",
            Self::TreeSha256InvalidRevision { .. } => "tree_sha256_invalid_revision",
            Self::TreeSha256MissingSubmodules(..) => "tree_sha256_missing_submodules",
            Self::TreeSha256RequiresSigning => "tree_sha256_requires_signing",
            Self::TagVerifyNotTag { .. } => "tag_verify_not_tag",
            Self::TagVerifyMissingObjectHeader => "tag_verify_missing_object_header",
            Self::TagVerifyDuplicateTreeSha256Header => "tag_verify_duplicate_tree_sha256_header",
            Self::TreeSha256Claim(..) => "tree_sha256_claim",
        }
    }

    /// Build [`Error::DirtyWorktree`], sorting and deduplicating `paths`.
    #[must_use]
    pub fn dirty_worktree(operation: WorktreeOperation, mut paths: Vec<String>) -> Self {
        paths.sort();
        paths.dedup();
        Self::DirtyWorktree { operation, paths }
    }

    /// Build [`Error::WouldOverwrite`] for switch and similar tree transitions.
    #[must_use]
    pub fn would_overwrite(reason: OverwriteReason, mut paths: Vec<String>) -> Self {
        paths.sort();
        paths.dedup();
        let message = match reason {
            OverwriteReason::LocalChanges => {
                "you have uncommitted changes — commit them before switching".into()
            }
            OverwriteReason::Untracked => {
                let first = paths.first().map(String::as_str).unwrap_or("<path>");
                format!("untracked file '{first}' would be overwritten — move or remove it first")
            }
        };
        Self::WouldOverwrite {
            reason,
            paths,
            message,
        }
    }

    /// Build [`Error::RemoteUnreadable`] for `url`.
    #[must_use]
    pub fn remote_unreadable(url: impl Into<String>, reason: RemoteUnreadableReason) -> Self {
        Self::RemoteUnreadable {
            url: Some(url.into()),
            reason,
        }
    }

    /// Whether this is [`Error::RemoteUnreadable`]: the remote could not be read or written
    /// at all (as opposed to a rejected update or an empty remote).
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        matches!(self, Self::RemoteUnreadable { .. })
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
        match self {
            Self::WorktreePathIo {
                path,
                operation,
                source,
            } => Some(PathIoError::worktree(
                path.clone(),
                *operation,
                preserve_io_error(source),
            )),
            Self::GitDirPathIo {
                path,
                operation,
                source,
            } => Some(PathIoError::git_dir(
                path.clone(),
                *operation,
                preserve_io_error(source),
            )),
            _ => None,
        }
    }

    /// Whether this is [`Error::WorktreePathIo`] caused by another process holding the file
    /// open (a Windows sharing or lock violation, `EBUSY`, or would-block), so retrying after
    /// closing that program can succeed.
    #[must_use]
    pub fn is_worktree_file_in_use(&self) -> bool {
        matches!(self, Self::WorktreePathIo { source, .. } if crate::worktree::io_error::is_file_in_use(source))
    }

    /// Whether this is [`Error::GitDirPathIo`] caused by another process holding a git-dir file
    /// open (Windows sharing or access-denied during replace, `EBUSY`, …).
    #[must_use]
    pub fn is_git_dir_file_in_use(&self) -> bool {
        matches!(
            self,
            Self::GitDirPathIo {
                source,
                operation,
                ..
            } if crate::git_dir::io_error::is_git_dir_transient_contention(*operation, source)
        )
    }

    /// Whether this is [`Error::Cancelled`] or a nested history failure that was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::History(e) => e.is_cancelled(),
            Self::CheckoutRollbackIncomplete { source, .. } => source.is_cancelled(),
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
            Self::Zlib(_) | Self::CorruptObject(_) => true,
            Self::CorruptLooseObject { .. } | Self::CorruptPackedObject { .. } => true,
            Self::Io(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => true,
            _ => false,
        }
    }

    pub(crate) fn is_object_read_wrap_source(&self) -> bool {
        match self {
            Self::Zlib(_) | Self::CorruptObject(_) => true,
            Self::Io(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => true,
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
        if matches!(err, Self::CorruptLooseObject { .. }) {
            return err;
        }
        if err.is_object_read_wrap_source() {
            Self::CorruptLooseObject {
                oid,
                path,
                source: Box::new(err),
            }
        } else {
            err
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
        if matches!(err, Self::CorruptPackedObject { .. }) {
            return err;
        }
        if err.is_object_read_wrap_source() {
            Self::CorruptPackedObject {
                oid,
                pack,
                offset,
                source: Box::new(err),
            }
        } else {
            err
        }
    }

    /// Build [`Error::InvalidPathComponent`].
    #[must_use]
    pub fn invalid_path_component(
        path: impl Into<String>,
        reason: crate::path_validate::PathComponentError,
    ) -> Self {
        Self::InvalidPathComponent {
            path: path.into(),
            reason,
        }
    }

    /// When set, the CLI should prefix stderr with `fatal: ` for Git-compatible commands.
    #[must_use]
    pub fn wants_fatal_prefix(&self) -> bool {
        matches!(
            self,
            Error::Config(_)
                | Error::Submodule(_)
                | Error::Ref(RefError::RefLock(_))
                | Error::Filter(_)
                | Error::PathOutsideRepo(_)
        )
    }

    /// Format for Git-style stderr (adds `fatal:` / `error:` when appropriate).
    #[must_use]
    pub fn git_stderr_message(&self) -> String {
        match self {
            Error::Diff(crate::diff::error::DiffError::Apply(ApplyError::CorruptPatch {
                ..
            })) => crate::diagnostics::error_line(&self.to_string()),
            Error::Message(s) => s.clone(),
            Error::WouldOverwrite { message, .. } => message.clone(),
            e if e.wants_fatal_prefix() => crate::diagnostics::fatal_line(&e.to_string()),
            other => other.to_string(),
        }
    }
}

impl From<crate::rev_parse_error::RevParseError> for Error {
    fn from(err: crate::rev_parse_error::RevParseError) -> Self {
        Self::Revision(crate::revision_error::RevisionError::RevParse(err))
    }
}

impl From<crate::rev_list_error::RevListError> for Error {
    fn from(err: crate::rev_list_error::RevListError) -> Self {
        Self::Revision(crate::revision_error::RevisionError::RevList(err))
    }
}

impl From<ApplyError> for Error {
    fn from(err: ApplyError) -> Self {
        Self::Diff(crate::diff::error::DiffError::Apply(err))
    }
}

impl From<crate::hooks::HookError> for Error {
    fn from(err: crate::hooks::HookError) -> Self {
        match err {
            crate::hooks::HookError::Cancelled => Self::Cancelled,
            other => Self::Hook(other),
        }
    }
}

impl From<crate::refs::store::RefStoreError> for Error {
    fn from(err: crate::refs::store::RefStoreError) -> Self {
        match err {
            crate::refs::store::RefStoreError::GitDirPathIo {
                path,
                operation,
                os_error,
            } => Error::GitDirPathIo {
                path,
                operation,
                source: crate::refs::store::RefStoreError::git_dir_io_source(os_error),
            },
            crate::refs::store::RefStoreError::Hook(hook) => Error::Hook(hook.into_hook_error()),
            other => Error::Ref(other.into()),
        }
    }
}

impl From<crate::reftable::ReftableError> for Error {
    fn from(err: crate::reftable::ReftableError) -> Self {
        Error::Ref(err.into())
    }
}

impl From<RefLockError> for Error {
    fn from(err: RefLockError) -> Self {
        Error::Ref(err.into())
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
        let path = value
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        match value.operation {
            PathIoOperation::Worktree(operation) => Self::WorktreePathIo {
                path,
                operation,
                source: value.source,
            },
            PathIoOperation::GitDir(operation) => Self::GitDirPathIo {
                path,
                operation,
                source: value.source,
            },
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

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

fn promisor_fetch_failed_message(
    missing: &crate::objects::ObjectId,
    failures: &[(String, Error)],
) -> String {
    let causes = failures
        .iter()
        .map(|(remote, err)| format!("{remote}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("could not fetch promised object {missing} ({causes})")
}

/// Serializes as `{"kind": <Error::kind_name>, "message": <Display text>}`.
#[cfg(feature = "serde-models")]
impl serde::Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        match self {
            Self::LockHeld(lock) => {
                let mut out = serializer.serialize_struct("Error", 3)?;
                out.serialize_field("kind", self.kind_name())?;
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
            Self::WouldOverwrite { reason, paths, .. } => {
                let mut out = serializer.serialize_struct("Error", 4)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("reason", &reason.to_string())?;
                out.serialize_field("paths", paths)?;
                out.end()
            }
            Self::StashCouldNotRestoreUntracked { path } => {
                let mut out = serializer.serialize_struct("Error", 3)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("path", path)?;
                out.end()
            }
            Self::DirtyWorktree { operation, paths } => {
                let mut out = serializer.serialize_struct("Error", 4)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("operation", &operation.to_string())?;
                out.serialize_field("paths", paths)?;
                out.end()
            }
            Self::UnmergedConflictMarkers { paths } => {
                let mut out = serializer.serialize_struct("Error", 3)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("paths", paths)?;
                out.end()
            }
            _ => {
                let mut out = serializer.serialize_struct("Error", 2)?;
                out.serialize_field("kind", self.kind_name())?;
                out.serialize_field("message", &self.to_string())?;
                out.end()
            }
        }
    }
}

#[cfg(feature = "serde-models")]
#[derive(serde::Serialize)]
struct LockHolderSerde {
    pid: u64,
    running: bool,
}
