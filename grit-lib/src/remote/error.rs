//! [`RemoteError`] and conversions for transport, fetch, push, and clone.

use std::path::PathBuf;

use thiserror::Error;

use super::push_plan::PushPlanError;
use crate::error::{AuthFailure, Error, RemoteUnreadableReason};
use crate::fetch_request::FetchRefspecError;
use crate::partial_clone::PartialCloneError;
use crate::protocol::ProtocolError;
use crate::resumable::ResumableError;
use crate::serve::ServeError;

/// Result of a remote operation ([`RemoteError`] on failure).
pub type RemoteResult<T> = std::result::Result<T, RemoteError>;

/// Errors for remote URL resolution, transport, fetch, push, and clone.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum RemoteError {
    /// A remote has no URL: `remote.<name>.url` is unset or blank, or a literal URL was empty.
    #[error("{}", match remote {
        Some(name) => format!("remote '{name}' has no URL configured"),
        None => "remote URL is empty".to_owned(),
    })]
    NoUrl {
        /// The configured remote's name; `None` when an empty literal URL was given.
        remote: Option<String>,
    },
    /// The URL uses a scheme grit does not support for remotes.
    #[error("unsupported remote URL scheme")]
    UnsupportedScheme,
    /// A URL or path failed to parse.
    #[error("{0}")]
    InvalidUrl(String),
    /// Smart HTTP requires the `http-ureq` feature or an injected HTTP client factory.
    #[error("HTTP remotes require the http-ureq feature or an HttpClientFactory")]
    HttpUnavailable,
    /// The argument is neither a configured remote name nor a URL or path.
    #[error("unknown remote '{0}'")]
    UnknownRemote(String),
    /// [`RepositoryRemotes::add`](crate::repo_handles::RepositoryRemotes::add) found an
    /// existing `remote.<name>.*` section.
    #[error("remote '{0}' already exists")]
    RemoteExists(String),
    /// The name cannot be used for a remote (`refs/remotes/<name>/…` would not be a valid ref).
    #[error("'{0}' is not a valid remote name")]
    InvalidRemoteName(String),

    /// HTTP authentication failed.
    #[error("authentication failed for {url}: {reason}")]
    Auth {
        /// Repository URL with credentials redacted.
        url: String,
        /// What went wrong.
        reason: AuthFailure,
    },
    /// A credential provider could not supply a complete username and password.
    #[error("{}", crate::credentials::NON_INTERACTIVE_MESSAGE)]
    CredentialsUnavailable {
        /// The `credential.helper` programs that were asked, in order.
        helpers: Vec<String>,
    },
    /// An atomic wire push was requested, but the remote did not advertise `atomic`.
    #[error("the remote does not support atomic pushes")]
    PushAtomicUnsupported,
    /// Push options were sent but the remote did not advertise `push-options`.
    #[error("the receiving end does not support push options")]
    PushOptionsUnsupported,
    /// `git fetch --unshallow` on a repository that is not shallow.
    #[error("repository is not shallow")]
    FetchUnshallowNotShallow,
    /// A deepen/shallow fetch was requested but the remote did not advertise `shallow`.
    #[error("the remote does not support shallow fetches")]
    FetchShallowUnsupported,
    /// A `--deepen` fetch was requested but the remote did not advertise `deepen-relative`.
    #[error("the remote does not support deepen-relative")]
    FetchDeepenRelativeUnsupported,
    /// Streaming fetch/clone temp pack disappeared before index-pack.
    #[error(
        "received pack file missing: {}. If this persists, check real-time antivirus or disk cleanup for quarantined pack temp files.",
        crate::git_path::user_display_path(.path)
    )]
    ReceivedPackTempMissing {
        /// Path to the temp pack that was expected on disk.
        path: PathBuf,
    },
    /// The remote could not be read or written at all.
    #[error("{}", crate::error::remote_unreadable_message(url.as_deref(), reason))]
    Unreadable {
        /// Remote URL or resolved local path, with credentials redacted.
        url: Option<String>,
        /// What went wrong.
        reason: RemoteUnreadableReason,
    },
    /// The remote was reached but aborted with its own error text.
    #[error("remote error: {message}")]
    Fatal {
        /// The server's message.
        message: String,
    },
    /// The upload-pack serving a local path or `file://` URL in this process failed.
    #[error("local upload-pack: {0}")]
    LocalUploadPack(Box<ServeError>),
    /// The thread serving a local path or `file://` URL panicked.
    #[error("local upload-pack stopped unexpectedly")]
    LocalUploadPackPanicked,
    /// A resumable bundle download or upload failed.
    #[error(transparent)]
    Resumable(#[from] Box<ResumableError>),
    /// Explicit fetch refspec resolution failed.
    #[error("{0}")]
    FetchRefspec(#[from] FetchRefspecError),
    /// Fetch refused to update the branch checked out in this or a linked worktree.
    #[error("cannot fetch into checked-out branch '{branch}'")]
    FetchIntoCheckedOutBranch {
        /// Short branch name under `refs/heads/`.
        branch: String,
    },
    /// Operation stopped via [`CancelToken`](crate::progress::CancelToken).
    #[error("operation cancelled")]
    Cancelled,
    /// Protocol allow/deny policy blocked the transport.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// Push planning failed before the wire protocol ran.
    #[error(transparent)]
    PushPlan(#[from] PushPlanError),
    /// Partial-clone filter configuration failed.
    #[error(transparent)]
    PartialClone(#[from] PartialCloneError),
    /// Malformed remote URL for a specific transport scheme.
    #[error("{detail}")]
    MalformedTransportUrl {
        /// `git`, `ssh`, `http`, or `file`.
        scheme: &'static str,
        /// Parse or validation detail.
        detail: String,
    },
    /// SSH subprocess or URL setup failed.
    #[error("{0}")]
    Ssh(String),
    /// Low-level transport failure (connection, DNS, TLS, or I/O outside HTTP status handling).
    #[error("{}", crate::error::remote_transport_message(.url.as_deref(), .detail))]
    Transport {
        /// Redacted repository URL when known.
        url: Option<String>,
        /// What failed (for example `connection refused`).
        detail: String,
    },
    /// HTTP status from an embedder [`HttpClient`](crate::transport::http::HttpClient) outside auth/unreadable mapping.
    #[error("{}", crate::error::remote_http_status_message(.url.as_deref(), *status))]
    HttpStatus {
        /// Redacted repository URL when known.
        url: Option<String>,
        /// Response status code.
        status: u16,
    },
    /// HTTP client or worker failure outside unreadable/auth classification.
    #[error("{0}")]
    Http(String),
    /// HTTP worker thread panicked.
    #[error("HTTP worker thread panicked")]
    HttpWorkerPanicked,
    /// HTTP transfer stalled below the configured low-speed limit.
    #[error("{0}")]
    HttpLowSpeedStall(String),
    /// Credential helper or askpass failure.
    #[error("{0}")]
    Credential(String),
    /// Non-remote library failure during a remote operation.
    #[error(transparent)]
    Nested(#[from] Box<Error>),
}

impl RemoteError {
    /// Map a low-level I/O error to [`RemoteError::Transport`].
    pub fn from_io(err: std::io::Error) -> Self {
        Self::Transport {
            url: None,
            detail: err.to_string(),
        }
    }

    /// Whether the remote could not be read or written at all.
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        matches!(self, Self::Unreadable { .. })
    }

    /// Whether the operation was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(
                self,
                Self::Nested(err) if err.is_cancelled()
            )
    }

    /// Stable slug aligned with former [`Error::kind_name`] values for absorbed variants.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NoUrl { .. } => "remote_no_url",
            Self::UnsupportedScheme => "unsupported_scheme",
            Self::InvalidUrl(_) => "invalid_url",
            Self::HttpUnavailable => "http_unavailable",
            Self::UnknownRemote(_) => "unknown_remote",
            Self::RemoteExists(_) => "remote_exists",
            Self::InvalidRemoteName(_) => "invalid_remote_name",
            Self::Auth { .. } => "auth",
            Self::CredentialsUnavailable { .. } => "credentials_unavailable",
            Self::PushAtomicUnsupported => "push_atomic_unsupported",
            Self::PushOptionsUnsupported => "push_options_unsupported",
            Self::FetchUnshallowNotShallow => "fetch_unshallow_not_shallow",
            Self::FetchShallowUnsupported => "fetch_shallow_unsupported",
            Self::FetchDeepenRelativeUnsupported => "fetch_deepen_relative_unsupported",
            Self::ReceivedPackTempMissing { .. } => "received_pack_temp_missing",
            Self::Unreadable { .. } => "remote_unreadable",
            Self::Fatal { .. } => "remote_fatal",
            Self::LocalUploadPack(_) => "local_upload_pack",
            Self::LocalUploadPackPanicked => "local_upload_pack_panicked",
            Self::Resumable(_) => "resumable",
            Self::FetchRefspec(_) => "fetch_refspec",
            Self::FetchIntoCheckedOutBranch { .. } => "fetch_into_checked_out_branch",
            Self::Cancelled => "cancelled",
            Self::Protocol(_) => "protocol",
            Self::PushPlan(_) => "push_plan",
            Self::PartialClone(_) => "partial_clone",
            Self::MalformedTransportUrl { .. } => "malformed_transport_url",
            Self::Transport { .. } => "transport",
            Self::HttpStatus { .. } => "http_status",
            Self::Ssh(_) => "ssh",
            Self::Http(_) => "http",
            Self::HttpWorkerPanicked => "http_worker_panicked",
            Self::HttpLowSpeedStall(_) => "http_low_speed_stall",
            Self::Credential(_) => "credential",
            Self::Nested(err) => err.kind_name(),
        }
    }

    /// Build [`RemoteError::Unreadable`] for `url`.
    #[must_use]
    pub fn unreadable(url: impl Into<String>, reason: RemoteUnreadableReason) -> Self {
        Self::Unreadable {
            url: Some(url.into()),
            reason,
        }
    }
}

impl From<Error> for RemoteError {
    fn from(err: Error) -> Self {
        match err {
            Error::Remote(remote) => remote,
            Error::Cancelled => Self::Cancelled,
            other => Self::Nested(Box::new(other)),
        }
    }
}
