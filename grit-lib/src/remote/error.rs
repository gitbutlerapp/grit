//! [`RemoteError`] and conversions for transport, fetch, push, and clone.

use std::path::PathBuf;

use thiserror::Error;

use super::push_plan::PushPlanError;
use crate::error::{AuthFailure, RemoteUnreadableReason};
use crate::fetch_request::FetchRefspecError;
use crate::partial_clone::PartialCloneError;
use crate::protocol::ProtocolError;
use crate::resumable::ResumableError;
use crate::serve::ServeError;

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
    #[error("{detail}")]
    InvalidUrl {
        /// Why the URL or path could not be used.
        detail: String,
    },
    /// Smart HTTP requires the `http-ureq` feature or an injected HTTP client factory.
    #[error("HTTP remotes require the http-ureq feature or an HttpClientFactory")]
    HttpUnavailable,
    /// The argument is neither a configured remote name nor a URL or path.
    #[error("unknown remote '{name}'")]
    UnknownRemote {
        /// The argument as given.
        name: String,
    },
    /// [`RepositoryRemotes::add`](crate::repo_handles::RepositoryRemotes::add) found an
    /// existing `remote.<name>.*` section.
    #[error("remote '{name}' already exists")]
    RemoteExists {
        /// The remote name.
        name: String,
    },
    /// The name cannot be used for a remote (`refs/remotes/<name>/…` would not be a valid ref).
    #[error("'{name}' is not a valid remote name")]
    InvalidRemoteName {
        /// The name as given.
        name: String,
    },

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
    /// Serving a fetch or push to a client failed at the protocol level.
    #[error(transparent)]
    Serve(ServeError),
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
    #[error("{detail}")]
    Ssh {
        /// What failed while starting or talking to `ssh`.
        detail: String,
    },
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
    #[error("{detail}")]
    Http {
        /// What the HTTP client reported.
        detail: String,
    },
    /// HTTP worker thread panicked.
    #[error("HTTP worker thread panicked")]
    HttpWorkerPanicked,
    /// HTTP transfer stalled below the configured low-speed limit.
    #[error("{detail}")]
    HttpLowSpeedStall {
        /// The transfer rate and limit that were crossed.
        detail: String,
    },
    /// Credential helper or askpass failure.
    #[error("{detail}")]
    Credential {
        /// What the credential helper or askpass reported.
        detail: String,
    },
    /// A snapshot checkout from a grit server failed for a server-specific reason.
    #[error(transparent)]
    Snapshot(crate::snapshot_checkout::CheckoutError),
    /// Clone-specific failure (destination not empty, missing `--branch` ref, failed checkout).
    #[error(transparent)]
    Clone(Box<crate::clone::CloneError>),
    /// A bundle used as a remote (or written for one) could not be read, written, or applied.
    #[error(transparent)]
    Bundle(crate::bundle::BundleError),
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

    /// Stable slug aligned with former [`Error::kind_name`](crate::error::Error::kind_name) values for absorbed variants.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NoUrl { .. } => "remote_no_url",
            Self::UnsupportedScheme => "unsupported_scheme",
            Self::InvalidUrl { .. } => "invalid_url",
            Self::HttpUnavailable => "http_unavailable",
            Self::UnknownRemote { .. } => "unknown_remote",
            Self::RemoteExists { .. } => "remote_exists",
            Self::InvalidRemoteName { .. } => "invalid_remote_name",
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
            Self::Serve(_) => "serve",
            Self::LocalUploadPackPanicked => "local_upload_pack_panicked",
            Self::Resumable(_) => "resumable",
            Self::FetchRefspec(_) => "fetch_refspec",
            Self::FetchIntoCheckedOutBranch { .. } => "fetch_into_checked_out_branch",
            Self::Protocol(_) => "protocol",
            Self::PushPlan(_) => "push_plan",
            Self::PartialClone(_) => "partial_clone",
            Self::MalformedTransportUrl { .. } => "malformed_transport_url",
            Self::Transport { .. } => "transport",
            Self::HttpStatus { .. } => "http_status",
            Self::Ssh { .. } => "ssh",
            Self::Http { .. } => "http",
            Self::HttpWorkerPanicked => "http_worker_panicked",
            Self::HttpLowSpeedStall { .. } => "http_low_speed_stall",
            Self::Credential { .. } => "credential",
            Self::Bundle(_) => "bundle",
            Self::Clone(_) => "clone",
            Self::Snapshot(_) => "snapshot",
        }
    }

    /// Functional area for analytics and JSON (`area`), matching [`Error::area`](crate::error::Error::area) when wrapped.
    #[must_use]
    pub fn area(&self) -> &'static str {
        "remote"
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
