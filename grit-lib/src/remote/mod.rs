//! Remotes, fetch/push, transport, and credential helpers.
//!
//! Remote operations return [`crate::error::Result`]; remote-specific failures are
//! [`Error::Remote`](crate::error::Error::Remote) with a [`RemoteError`]. Use
//! [`Error::is_cancelled`](crate::error::Error::is_cancelled) and
//! [`RemoteError::is_remote_unreadable`] instead of matching variants one by one.
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

pub use error::RemoteError;

#[cfg(feature = "http-ureq")]
pub use core::http_client;
#[cfg(feature = "http-ureq")]
pub use core::DefaultHttpClientFactory;
pub use core::{
    fold_peeled_refs, HttpClientFactory, ListRefsOptions, PeeledRemoteRef, Remote, RemoteRef,
    RemoteUrl, DEFAULT_REMOTE,
};
pub(crate) use core::{
    is_valid_remote_name, list_refs_from_git_dir, list_refs_from_git_dir_for_fetch,
    PreflightHttpClientFactory,
};
pub(crate) use inspect::inspect_remote;
pub use inspect::{
    InspectOptions, RemoteBranchState, RemoteBranchStateKind, RemotePullBranch, RemoteSummary,
};

pub mod fetch {
    //! fetch API (re-exported at this path).

    pub use crate::fetch::{FetchMilestone, NoProgress, Progress};
}
pub mod fetch_request {
    //! Explicit fetch refspec resolution.
    pub use crate::fetch_request::{
        resolve_fetch_invocation, resolve_pull_invocation, resolve_remote_argument,
        FetchInvocation, FetchRefspecError,
    };
}
pub mod credentials {
    //! credentials API (re-exported at this path).
    #[cfg(windows)]
    pub use crate::credentials::windows_store;
    pub use crate::credentials::{
        helper_config_value, use_http_path, Credential, CredentialProvider,
        HelperCredentialProvider, HelperValueError, NON_INTERACTIVE_MESSAGE,
    };
}
#[cfg(feature = "unstable-hosting")]
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
#[cfg(feature = "unstable-hosting")]
pub mod snapshot_checkout {
    //! snapshot checkout API (re-exported at this path).

    pub use crate::snapshot_checkout::BranchSet;
    pub use crate::snapshot_checkout::CheckoutUrl;
    pub use crate::snapshot_checkout::GraftError;
    pub use crate::snapshot_checkout::Materialized;
    pub use crate::snapshot_checkout::ProjectedCommit;
    pub use crate::snapshot_checkout::Projection;
    pub use crate::snapshot_checkout::ReservedName;
    pub use crate::snapshot_checkout::{
        materialize, normalize_paths, out_ref, same_branch_name, suggest_work_branch, target_ref,
        upstream_ref, valid_branch_name, valid_checkout_id, BranchInfo, BranchRefresh,
        BranchRefreshResult, BranchRequest, CheckinBranch, CheckinRefusal, CheckinRequest,
        CheckinResponse, CheckinResult, CheckinRule, CheckinStatus, CheckinUpdate, CheckoutClient,
        CheckoutError, CheckoutErrorBody, CheckoutInfo, CheckoutMarker, CheckoutRecord,
        CheckoutRequest, CheckoutServerInfo, CheckoutState, CheckoutStore, GraftLog, Grafted,
        Ident, IntegrateRequest, IntegrateResponse, IntegrateStatus, Integration, Landing,
        MaterializeRequest, ProjectError, RefreshOutcome, RefreshedHead, Slice, SliceChange,
        SliceDeleteRequest, SliceEdit, SliceRequest, SlicesEdited, SlicesResponse, Snapshot,
        SnapshotRoot, StoredCheckout, TargetChange, TargetGuard, TargetRefusal, WorkBranchRequest,
        WorkBranchResponse, CHECKOUT_API, SLICES_PATH,
    };
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
#[cfg(feature = "unstable-hosting")]
pub mod grit_server_auth {
    //! grit_server_auth API (re-exported at this path).

    pub use crate::grit_server_auth::{
        credential_for_url, fetch_identity, LoginFlow, LoginGrant, LoginPoll, LoginPollRequest,
        LoginStart, ServerAuthError, ServerIdentity, ServerUrl, TokenCredentialProvider,
        AUTHORIZE_PATH, IDENTITY_PATH, LOGIN_PATH, LOGIN_POLL_PATH,
    };
}
#[cfg(feature = "unstable-hosting")]
pub mod grit_server_share {
    //! grit_server_share API (re-exported at this path).
    pub use crate::grit_server_share::{
        accept, filter_listed_shares, share, share_link, LinkOptions, LinkUses, NewShareLink,
        ShareAcceptReply, ShareAcceptRequest, ShareAcceptStatus, ShareIntegration, ShareLinkInfo,
        ShareMember, ShareMode, ShareReply, ShareRequest, ShareState, ShareView, SharedBranch,
        DEFAULT_LINK_DAYS, MAX_LINK_DAYS, SHARED_REFS,
    };
}

#[cfg(feature = "unstable-hosting")]
pub mod grit_server_sync {
    //! Sync report flags for a grit hosting space.
    pub use crate::grit_server_sync::{
        sync_needs_attention, sync_needs_decision, SyncBranchAction, SyncBranchState,
        SyncReportSummary, SyncTargetAction,
    };
}

#[cfg(feature = "unstable-hosting")]
pub mod grit_server_space {
    //! grit_server_space API (re-exported at this path).
    pub use crate::grit_server_space::{
        fetch_space, hosted_project, integrate_branch, IntegrateBranchReply,
        IntegrateBranchRequest, IntegrateBranchStatus, Space, WaitingHead,
    };
}
pub mod transfer {
    //! transfer API (re-exported at this path).

    pub use crate::transfer::{
        build_pack, build_pack_from_send_list, reachable_closure, resolve_fetch_prune,
        resolve_fetch_tags, CloneReflog, FetchAllOptions, FetchDepth, FetchOptions, FetchOutcome,
        PackBuildOptions, PackObjects, PackObjectsOptions, PackStats, PushOptions, PushOutcome,
        PushRefSpec, RefUpdate, TagMode, TransferEndpoint, TransferStats, UpdateKind, UpdateMode,
    };
}
pub mod transport {
    //! transport API (re-exported at this path).
    pub use crate::transport::http::http_client_arc;

    pub use crate::transport::{
        http, read_advertisement, resolve_effective_ssh_command, Advertisement, ConnectOptions,
        Connection, GitDaemonTransport, Service, SshCommand, SshTransport, Transport,
    };
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
    pub use crate::push_report::{PushRefResult, PushRefStatus};
}
pub mod clone {
    //! clone API (re-exported at this path).

    pub use crate::clone::CloneIntoOutcome;
    pub use crate::clone::{
        cleanup_failed_clone, clone, clone_effective_single_branch, clone_into, derive_clone_dir,
        dest_preexisted_empty, should_cleanup_failed_clone, CloneCheckout, CloneError, CloneHead,
        CloneIntoOptions, CloneOptions, CloneOutcome, CloneTags,
    };
}
pub mod partial_clone {
    //! Partial-clone filter specs and promisor-remote config.
    pub use crate::partial_clone::{ObjectFilterSpec, PartialCloneError};
}
pub mod serve {
    //! serve API (re-exported at this path).

    pub use crate::serve::{
        io_error_is_request_body_limit, DeepenNotError, MissingObjectReason, ProtocolVersion,
        ReceiveHook, ReceiveHookContext, ReceiveOutcome, ReceivePack, ReceivePolicy, RefCommand,
        RefUpdateRefusal, RefUpdateResult, RequestBodyLimitExceeded, Result, ServeError,
        ServeOptions, UnpackError, UploadOutcome, UploadPack,
    };
}
pub mod pkt_line {
    //! pkt-line framing for the wire protocol.
    pub use crate::pkt_line::{read_packet, write_flush, write_line_to_vec, Packet, DELIM, FLUSH};
}
pub mod sideband_lines {
    //! sideband_lines API (re-exported at this path).
    pub use crate::sideband_lines::{RemoteLine, SidebandLines};
}

pub use crate::transport_path::redact_url_for_display;
