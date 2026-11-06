//! Remotes, fetch/push, transport, and credential helpers.
//!
//! Remote operations return [`RemoteResult`] / [`RemoteError`]; use [`RemoteError::is_cancelled`]
//! and [`RemoteError::is_remote_unreadable`] instead of matching nested library errors.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/network)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.remotes();
//!
//! # Ok(())
//! # }
//! ```
mod core;
mod error;
mod inspect;
pub mod push_plan;

pub use error::{RemoteError, RemoteResult};

pub(crate) use core::is_valid_remote_name;
pub use core::*;
pub use inspect::{
    inspect_remote, InspectOptions, RemoteBranchState, RemoteBranchStateKind, RemotePullBranch,
    RemoteSummary,
};

pub mod fetch {
    //! fetch API (re-exported at this path).
    pub use crate::fetch::*;
}
pub mod fetch_request {
    //! Explicit fetch refspec resolution.
    pub use crate::fetch_request::{
        resolve_fetch_invocation, resolve_pull_invocation, resolve_remote_argument,
        FetchInvocation, FetchRefspecError,
    };
}
pub mod push {
    //! push API (re-exported at this path).
    pub use crate::push::*;
}
pub mod credentials {
    //! credentials API (re-exported at this path).
    #[cfg(windows)]
    pub use crate::credentials::windows_store;
    pub use crate::credentials::{
        use_http_path, Credential, CredentialProvider, HelperCredentialProvider,
        NON_INTERACTIVE_MESSAGE,
    };
}
pub mod github_auth {
    //! GitHub device-flow authentication helpers.
    pub use crate::github_auth::{
        github_token_credential, is_github_https_url, DeviceAuthorization, DeviceFlow,
        DeviceFlowError, DeviceFlowStep, PollClock, PollStatus, SystemPollClock, GITHUB_HOST,
        TOKEN_USERNAME,
    };
}
pub mod push_bundle {
    //! Resumable push bundle upload path suffix (smart HTTP servers).
    pub use crate::push_bundle::RECEIVE_SUFFIX;
}
pub mod snapshot_checkout {
    //! snapshot checkout API (re-exported at this path).
    pub use crate::snapshot_checkout::*;
}
pub mod resumable {
    //! Resumable HTTP downloads and TUS uploads.
    pub use crate::resumable::{
        claim_resumable_download, discard_download, download_resumable, ResumableDownloadDest,
        ResumableError, ResumableEvent, TransferDirection, TUS_PATCH_CONTENT_TYPE, TUS_VERSION,
    };
}
pub mod bundle_uri {
    //! bundle-uri API (re-exported at this path).
    pub use crate::bundle_uri::{
        bundle_cache_path, BundleList, BundleListEntry, BundleListMode, BundleSeed,
    };
}
pub mod grit_server_auth {
    //! grit_server_auth API (re-exported at this path).
    pub use crate::grit_server_auth::*;
}
pub mod grit_server_share {
    //! grit_server_share API (re-exported at this path).
    pub use crate::grit_server_share::{
        accept, share, share_link, LinkOptions, LinkUses, NewShareLink, ShareAcceptReply,
        ShareAcceptRequest, ShareAcceptStatus, ShareIntegration, ShareLinkInfo, ShareMember,
        ShareMode, ShareReply, ShareRequest, ShareState, ShareView, SharedBranch,
        DEFAULT_LINK_DAYS, MAX_LINK_DAYS, SHARED_REFS,
    };
}

pub mod grit_server_space {
    //! grit_server_space API (re-exported at this path).
    pub use crate::grit_server_space::{
        fetch_space, hosted_project, integrate_branch, IntegrateBranchReply,
        IntegrateBranchRequest, IntegrateBranchStatus, Space, WaitingHead,
    };
}
pub mod transfer {
    //! transfer API (re-exported at this path).
    pub use crate::transfer::*;
}
pub mod transport {
    //! transport API (re-exported at this path).
    pub use crate::transport::http::{http_client_arc, http_fetch};
    pub use crate::transport::*;
}
pub mod transport_path {
    //! transport_path API (re-exported at this path).
    pub use crate::transport_path::{
        redact_url_for_display, resolve_local_remote_git_dir, NoDirectoryName,
    };
}
pub mod protocol {
    //! protocol API (re-exported at this path).
    pub use crate::protocol::{
        check_protocol_allowed_with, try_effective_client_protocol_version_from_config,
        ProtocolError, ProtocolPolicyInputs,
    };
}
pub mod shallow {
    //! Shallow-boundary helpers exposed for integration tests.
    pub use crate::shallow::load_shallow_oids;
}
pub mod push_report {
    //! push_report API (re-exported at this path).
    pub use crate::push_report::*;
}
pub mod clone {
    //! clone API (re-exported at this path).
    pub use crate::clone::*;
}
pub mod partial_clone {
    //! Partial-clone filter specs and promisor-remote config.
    pub use crate::partial_clone::{ObjectFilterSpec, PartialCloneError};
}
pub mod serve {
    //! serve API (re-exported at this path).
    pub use crate::serve::*;
}
pub mod pkt_line {
    //! pkt-line framing for the wire protocol.
    pub use crate::pkt_line::{read_packet, write_flush, write_line_to_vec, Packet, DELIM, FLUSH};
}
pub mod sideband_lines {
    //! sideband_lines API (re-exported at this path).
    pub use crate::sideband_lines::*;
}

pub use crate::transport_path::redact_url_for_display;
