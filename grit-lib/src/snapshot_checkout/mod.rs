//! Snapshot checkouts: start work on a repository without cloning its history.
//!
//! A server that offers snapshot checkouts (`grit serve` does) answers under
//! its own endpoint, `/.grit/checkouts/<repository>`, separate from the Git
//! smart-HTTP paths, so a client can tell a server that has them from one that
//! doesn't. A checkout is one **snapshot commit**: the target branch's tree,
//! cut down to the paths asked for (as Git's cone-mode sparse checkout has
//! them: everything under each path, plus the files directly in every
//! directory above one), in a commit with fixed metadata whose message names
//! the real commit it stands for. Because the metadata is fixed, every
//! checkout of the same paths at the same commit gets the same snapshot.
//!
//! The server hands the snapshot out as a bundle, which the client downloads
//! with [`crate::resumable::download_resumable`] (so an interrupted checkout
//! resumes) and turns into a new repository whose history is that one commit,
//! with the snapshot at [`upstream_ref`] (`refs/remotes/origin/<branch>`) and a
//! new work branch on top of it checked out. Its `origin` fetches only the
//! checkout's snapshots ([`snapshot_fetch_refspec`], no tags) and pushes to the
//! check-in area ([`out_push_refspec`]), so plain `git` and `grit` fetches and
//! pushes in a checkout never bring the full history or touch the server's
//! real branches.
//!
//! - Wire types: [`CheckoutServerInfo`], [`CheckoutRequest`], [`CheckoutInfo`].
//! - Server side: [`normalize_paths`], [`filtered_tree`], [`snapshot_object`],
//!   [`make_snapshot`].
//! - Client side: [`CheckoutUrl`], [`CheckoutClient`], [`materialize`],
//!   [`CheckoutMarker`], [`push_for_checkin`], [`fetch_snapshot`], [`refresh`].
//!
//! Work comes back with a **check-in**: the client gives commits made with
//! plain Git a change id ([`give_change_ids`]), pushes its branches' raw tips
//! to [`out_ref`] ([`push_for_checkin`]) and asks the server to [`graft`] them
//! onto the real history ([`graft_checkin_branch`]) and set the server's
//! branches ([`set_checked_in_branch`], deciding with [`decide_checkin`],
//! which also accepts a rebase that keeps every commit by change id:
//! [`keeps_every_commit`]). The
//! server can [`integrate`] a branch into the target with a merge commit, and
//! keeps **slices** — named sets of paths — in [`SLICES_PATH`] on the target
//! branch ([`edit_slices`]). Those are the only ways the target changes: the
//! server refuses plain pushes to it with a [`TargetGuard`]. A **refresh** asks the server for a snapshot of the newest target on
//! top of the checkout's last one ([`CheckoutClient::update`]), fetches it from
//! [`target_ref`] ([`fetch_snapshot`]) and brings the current branch onto it
//! ([`refresh`]). The client's transfers always go to the URL its marker
//! records, whatever `origin` has become. Once a checked-in branch has been
//! integrated into the branch the checkout snapshots, the checkout records
//! that it landed ([`land_integration`], [`record_landed`]), so refreshes
//! never replay its commits onto snapshots that already have them, and
//! [`Landing::move_off`] puts the user on a fresh work branch.
//!
//! The server keeps each checkout's record, and the work-branch names it has
//! given out, in a [`CheckoutStore`].

mod checkin;
mod graft;
mod guard;
mod ids;
mod landing;
mod refresh;
mod refs;
mod store;
mod target;
mod transfer;
mod tree;

pub use checkin::{
    decide_checkin, graft_checkin_branch, set_checked_in_branch, BranchSet, CheckinDecision,
    CheckinRefusal, CheckinRule,
};
pub use graft::{graft, GraftError, Grafted};
pub use guard::TargetGuard;
pub use ids::{change_keys, give_change_ids, keeps_every_commit};
pub use landing::{land_integration, move_off_landed_branch, record_landed, Landing, MovedOff};
pub use refresh::{
    ensure_refreshable, refresh, BranchOnOldSnapshot, FollowedBranch, RefreshOutcome, RefreshedHead,
};
pub use store::{valid_checkout_id, CheckoutRecord, CheckoutStore, ReservedName, StoredCheckout};
pub use target::{
    edit_slices, integrate, missing_paths, parse_slices, read_slices, render_slices,
    same_branch_name, target_branch, valid_slice_name, Integration, SliceEdit, SlicesEdited,
    TargetChange, TargetRefusal, SLICES_PATH,
};
pub use transfer::{fetch_snapshot, push_for_checkin, FetchedSnapshot};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fetch::Progress;
use crate::objects::{
    parse_commit, parse_tree, serialize_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry,
};
use crate::odb::Odb;
use crate::progress::Cancel;
use crate::repo::{InitOptions, Repository};
use crate::transport::http::{HttpClient, HttpMethod, HttpRequest};

/// Path under the server root where snapshot checkouts are served:
/// `/.grit/checkouts/<repository>`.
pub const CHECKOUT_API: &str = ".grit/checkouts";

/// Name and email of every snapshot commit's author and committer.
pub const SNAPSHOT_IDENT: &str = "grit-checkout <snapshot@grit-checkout>";

/// Trailer naming the real commit a snapshot stands for.
pub const SNAPSHOT_BASE_TRAILER: &str = "Grit-Checkout-Base";

/// File in the repository's common git directory (shared by all its worktrees)
/// that marks it as a snapshot checkout.
pub const MARKER_FILE: &str = "grit-checkout.json";

const TREE_MODE: u32 = 0o040000;
const GITLINK_MODE: u32 = 0o160000;

/// `GET /.grit/checkouts/<repo>`: what the server would check out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutServerInfo {
    /// The repository's name.
    pub name: String,
    /// The target branch checkouts start from.
    pub branch: String,
    /// Where the target branch is.
    pub head: String,
}

/// `POST /.grit/checkouts/<repo>`: make a checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRequest {
    /// Paths to check out; empty is the whole tree.
    #[serde(default)]
    pub paths: Vec<String>,
    /// What the client calls the checkout (its directory name), for the server's log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// The work branch name the client would like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
}

/// One snapshot commit of a checkout and the real commit it stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRoot {
    /// The snapshot commit.
    pub commit: String,
    /// The real commit of the target branch it stands for.
    pub base: String,
}

/// A checkout as the server knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutInfo {
    /// The checkout's id on the server.
    pub id: String,
    /// The repository's name, the default directory to check out into.
    pub name: String,
    /// The target branch.
    pub branch: String,
    /// The checkout's paths (normalized); empty is the whole tree.
    pub paths: Vec<String>,
    /// The real commit the newest snapshot stands for.
    pub base: String,
    /// The newest snapshot commit.
    pub root: String,
    /// That commit's exact bytes, so the client can check it rebuilt the same object.
    pub root_object: String,
    /// The snapshot's tree.
    pub tree: String,
    /// Every snapshot commit of the checkout, oldest first.
    pub roots: Vec<SnapshotRoot>,
    /// Files in the snapshot.
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
    /// Size of the bundle the snapshot downloads as.
    pub bundle_bytes: u64,
    /// The work branch the client should create: the one it asked for, numbered
    /// when the server already has a branch by that name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
}

/// `GET /.grit/checkouts/<repo>/<id>`: a checkout as the server has it, and
/// where its target branch is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutState {
    /// The checkout's record.
    #[serde(flatten)]
    pub info: CheckoutInfo,
    /// Where the target branch is now, which is past [`CheckoutInfo::base`]
    /// when it moved since the last snapshot; `None` when the server no
    /// longer has the branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
}

/// `POST /.grit/checkouts/<repo>/<id>/work`: reserve a fresh work branch for
/// checkout `id`, as making a checkout does ([`CheckoutRequest::work`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkBranchRequest {
    /// The name the client would like.
    pub work: String,
}

/// The answer to a [`WorkBranchRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkBranchResponse {
    /// The name the server reserved: the one asked for, numbered past the
    /// server's branches and the names it gave out before. It is never given
    /// out again.
    pub work: String,
}

/// The ref a checkout's newest snapshot is kept at on the server.
#[must_use]
pub fn target_ref(id: &str) -> String {
    format!("refs/checkouts/{id}/target")
}

/// Whether `name` is a valid branch name (`refs/heads/<name>` is a valid ref).
#[must_use]
pub fn valid_branch_name(name: &str) -> bool {
    crate::check_ref_format::check_refname_format(
        &format!("refs/heads/{name}"),
        &crate::check_ref_format::RefNameOptions::default(),
    )
    .is_ok()
}

/// Where a checkout pushes its branch `name` for the server to graft.
#[must_use]
pub fn out_ref(id: &str, name: &str) -> String {
    format!("refs/checkouts/{id}/out/heads/{name}")
}

/// The local ref a checkout keeps its newest snapshot of `branch` at:
/// `refs/remotes/origin/<branch>`.
#[must_use]
pub fn upstream_ref(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

/// The refspec that fetches checkout `id`'s newest snapshot (at [`target_ref`]
/// on the server) into [`upstream_ref`]`(branch)`:
/// `+refs/checkouts/<id>/target:refs/remotes/origin/<branch>`.
///
/// A checkout's `origin` fetches with it, so a plain `git fetch` or
/// `grit fetch` there brings only snapshots, never the full history.
#[must_use]
pub fn snapshot_fetch_refspec(id: &str, branch: &str) -> String {
    format!("+{}:{}", target_ref(id), upstream_ref(branch))
}

/// The refspec a checkout's `origin` pushes with:
/// `+refs/heads/*:refs/checkouts/<id>/out/heads/*`.
///
/// A plain `git push` or `grit push` in a checkout then lands where check-ins
/// graft from ([`out_ref`]), never on the server's real branches.
#[must_use]
pub fn out_push_refspec(id: &str) -> String {
    format!("+refs/heads/*:{}", out_ref(id, "*"))
}

/// One branch of a check-in: its name and the tip the client pushed to
/// [`out_ref`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckinBranch {
    /// Branch name (without `refs/heads/`).
    pub name: String,
    /// The tip pushed to [`out_ref`].
    pub raw: String,
}

/// `POST /.grit/checkouts/<repo>/<id>/checkin`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckinRequest {
    /// The branches to check in.
    pub branches: Vec<CheckinBranch>,
    /// Set a branch even where it would drop commits the server has.
    #[serde(default)]
    pub force: bool,
}

/// What a check-in did to one branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckinStatus {
    /// The server had no such branch; it does now.
    Created,
    /// The server's branch moved to the graft.
    Updated,
    /// The server's branch was already there.
    Unchanged,
    /// The branch was not set.
    Refused,
}

/// The result for one branch of a check-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckinResult {
    /// The branch.
    pub name: String,
    /// What happened.
    pub status: CheckinStatus,
    /// The commit on the server's history the branch's tip became.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grafted: Option<String>,
    /// Where the server's branch was before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Commits replayed.
    #[serde(default)]
    pub commits: usize,
    /// For [`CheckinStatus::Updated`], the rule that allowed the update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<CheckinRule>,
    /// Why it was refused, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Why it was refused, for programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<CheckinRefusal>,
}

/// The answer to a check-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckinResponse {
    /// One result per branch, in request order.
    pub results: Vec<CheckinResult>,
}

/// Who did something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ident {
    /// Name.
    pub name: String,
    /// Email address.
    pub email: String,
}

impl Ident {
    /// Who the user's own commits are by, for a server-made commit made for
    /// them (an integration's merge, a slice change): the author identity a
    /// commit gets, from `GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL`, then
    /// `author.*`, then `user.*` (see
    /// [`crate::config::ident_resolve::resolve_name_with`]).
    ///
    /// # Parameters
    ///
    /// - `env`: the environment to read `GIT_AUTHOR_*` and fallbacks from.
    /// - `config`: the repository's configuration.
    ///
    /// # Errors
    ///
    /// [`crate::config::ident_resolve::IdentityError`] when no name or email
    /// is configured; a server then uses the signed-in user.
    pub fn author<E: crate::ident_resolve::IdentityEnv>(
        env: &E,
        config: &crate::config::ConfigSet,
    ) -> std::result::Result<Self, crate::ident_resolve::IdentityError> {
        use crate::ident_resolve::{resolve_email_with, resolve_name_with, IdentRole};
        Ok(Self {
            name: resolve_name_with(env, config, IdentRole::Author)?,
            email: resolve_email_with(env, config, IdentRole::Author)?,
        })
    }

    /// The signature of a commit by this identity at `time`, cleaned of
    /// anything that would break the commit's header
    /// ([`crate::ident::Signature::cleaned`]), since a server takes
    /// identities from its requests. A name that cleans away to nothing is
    /// replaced by the email.
    ///
    /// # Errors
    ///
    /// [`crate::ident::SignatureError`] for a `time` before the epoch or with
    /// a sub-minute offset.
    pub fn signature(
        &self,
        time: time::OffsetDateTime,
    ) -> std::result::Result<crate::ident::Signature, crate::ident::SignatureError> {
        let signature = crate::ident::Signature::cleaned(&self.name, &self.email, time)?;
        if signature.name.is_empty() {
            return Ok(crate::ident::Signature {
                name: signature.email.clone(),
                ..signature
            });
        }
        Ok(signature)
    }
}

/// `POST /.grit/checkouts/<repo>/integrate`: merge a branch into the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrateRequest {
    /// The branch on the server.
    pub name: String,
    /// Who the merge commit is by (the signed-in user when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Ident>,
}

/// What an integration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrateStatus {
    /// A merge commit moved the target.
    Merged,
    /// The target already had every commit of the branch.
    AlreadyIntegrated,
}

/// The answer to an integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrateResponse {
    /// What happened.
    pub status: IntegrateStatus,
    /// The branch.
    pub name: String,
    /// The target branch.
    pub target: String,
    /// Where the target was.
    pub previous: String,
    /// Where it is now.
    pub head: String,
    /// The branch's commits the merge brought in.
    pub commits: usize,
}

/// A named set of paths to check out together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slice {
    /// The slice's name.
    pub name: String,
    /// What it is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its paths.
    pub paths: Vec<String>,
}

/// `GET /.grit/checkouts/<repo>/slices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlicesResponse {
    /// The target branch.
    pub target: String,
    /// Where it is.
    pub head: String,
    /// The slices on it.
    pub slices: Vec<Slice>,
}

/// `PUT /.grit/checkouts/<repo>/slices/<name>`: make or change a slice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceRequest {
    /// The slice's paths.
    pub paths: Vec<String>,
    /// What it is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Make a new slice, refusing a name that exists; otherwise change one
    /// that does.
    #[serde(default)]
    pub create: bool,
    /// Who the commit is by (the signed-in user when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Ident>,
}

/// `DELETE /.grit/checkouts/<repo>/slices/<name>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceDeleteRequest {
    /// Who the commit is by (the signed-in user when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Ident>,
}

/// What a change to the slices did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceChange {
    /// The slice as it is now; none once deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<Slice>,
    /// The target branch.
    pub target: String,
    /// Where it was.
    pub previous: String,
    /// Where it is: a commit changing the slices file, or `previous` when
    /// nothing changed.
    pub head: String,
}

/// An error body the checkout endpoints answer with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutErrorBody {
    /// What went wrong.
    pub error: String,
}

/// Why a snapshot checkout failed.
#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    /// The server has no snapshot-checkout endpoint for this repository.
    #[error("{url} offers no grit checkout (no {endpoint} endpoint); it may be a plain Git server, or the repository may not exist")]
    NotOffered {
        /// The URL the user gave.
        url: String,
        /// The endpoint that was missing.
        endpoint: String,
    },
    /// The server refused a request.
    #[error("{url}: {message} (HTTP {status})")]
    Server {
        /// The request URL.
        url: String,
        /// The response status.
        status: u16,
        /// The server's explanation.
        message: String,
    },
    /// The URL cannot name a repository.
    #[error("{0} is not an http(s) URL of a repository")]
    InvalidUrl(String),
    /// A path is not a plain path inside the repository.
    #[error("{path}: {reason}")]
    InvalidPath {
        /// The path as given.
        path: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A path is not in the tree being checked out.
    #[error("{path} is not in the repository at {base}")]
    MissingPath {
        /// The path.
        path: String,
        /// The commit it was looked up in.
        base: String,
    },
    /// The downloaded snapshot is not the one the server described.
    #[error("the downloaded snapshot is {got}, but the server described {expected}")]
    Mismatch {
        /// The snapshot commit the server described.
        expected: String,
        /// What the download held.
        got: String,
    },
    /// The destination already has files.
    #[error("{0} already exists and is not empty")]
    DestinationNotEmpty(PathBuf),
    /// A name is not a valid branch name.
    #[error("{0} is not a valid branch name")]
    InvalidBranchName(String),
    /// A checkout's stored record could not be read.
    #[error("the record of checkout {id} at {path} is damaged: {detail}")]
    DamagedRecord {
        /// The checkout's id.
        id: String,
        /// The record's file.
        path: PathBuf,
        /// What was wrong with it.
        detail: String,
    },
    /// A ref kept moving while the server tried to set it.
    #[error("{refname} kept moving while it was being set")]
    RefKeptMoving {
        /// The ref.
        refname: String,
    },
    /// Every work-branch name the server reserved is a branch in the
    /// checkout already.
    #[error("the work branches the server reserved are taken here (the last was {name})")]
    WorkBranchTaken {
        /// The last name the server reserved.
        name: String,
    },
    /// The snapshot fetched is not the one the server described: the
    /// checkout was refreshed again (from another worktree, say) meanwhile.
    #[error("the server's snapshot moved again while fetching: expected {expected}, got {got}")]
    SnapshotMoved {
        /// The snapshot the server described.
        expected: String,
        /// What the fetch brought into the checkout's upstream ref.
        got: String,
    },
    /// The checkout is in the middle of an operation (a stopped rebase, a
    /// merge) that has to be finished or aborted first.
    #[error("a {} is in progress", .0.description())]
    OperationInProgress(crate::state::InProgressOperation),
    /// A fetch from or push to the server failed.
    #[error(transparent)]
    Remote(Box<crate::remote::RemoteError>),
    /// Bringing the current branch onto the new snapshot failed.
    #[error(transparent)]
    Pull(Box<crate::porcelain::pull::PullError>),
    /// The server's answer could not be parsed.
    #[error("unexpected answer from {url}: {detail}")]
    Protocol {
        /// The request URL.
        url: String,
        /// What was wrong.
        detail: String,
    },
    /// The download failed.
    #[error(transparent)]
    Download(Box<crate::resumable::ResumableError>),
    /// A repository operation failed.
    #[error(transparent)]
    Library(Box<crate::error::Error>),
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<crate::error::Error> for CheckoutError {
    fn from(e: crate::error::Error) -> Self {
        Self::Library(Box::new(e))
    }
}

impl From<crate::resumable::ResumableError> for CheckoutError {
    fn from(e: crate::resumable::ResumableError) -> Self {
        Self::Download(Box::new(e))
    }
}

impl From<crate::remote::RemoteError> for CheckoutError {
    fn from(e: crate::remote::RemoteError) -> Self {
        Self::Remote(Box::new(e))
    }
}

impl From<crate::porcelain::pull::PullError> for CheckoutError {
    fn from(e: crate::porcelain::pull::PullError) -> Self {
        Self::Pull(Box::new(e))
    }
}

/// Result alias for snapshot checkouts.
pub type Result<T> = std::result::Result<T, CheckoutError>;

/// Clean up requested paths: relative, without `.`, `..` or empty components,
/// without trailing slashes, sorted, deduplicated, and with paths inside
/// another one dropped.
///
/// # Errors
///
/// [`CheckoutError::InvalidPath`] for an absolute path or one with `.`, `..`
/// or empty components.
pub fn normalize_paths(paths: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let trimmed = p.trim().trim_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        if p.trim().starts_with('/') {
            return Err(CheckoutError::InvalidPath {
                path: p.clone(),
                reason: "paths are relative to the repository root",
            });
        }
        if trimmed
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(CheckoutError::InvalidPath {
                path: p.clone(),
                reason: "not a plain path inside the repository",
            });
        }
        out.push(trimmed.to_owned());
    }
    out.sort();
    out.dedup();
    let all = out.clone();
    out.retain(|p| {
        !all.iter().any(|q| {
            q != p && p.starts_with(q.as_str()) && p.as_bytes().get(q.len()) == Some(&b'/')
        })
    });
    Ok(out)
}

/// The entry at `path` (slash-separated) under `tree`, as `(mode, oid)`.
fn lookup(odb: &Odb, tree: ObjectId, path: &[u8]) -> crate::error::Result<Option<(u32, ObjectId)>> {
    let mut current = (TREE_MODE, tree);
    for component in path.split(|b| *b == b'/') {
        if current.0 != TREE_MODE {
            return Ok(None);
        }
        let entries = parse_tree(&odb.read(&current.1)?.data)?;
        match entries.into_iter().find(|e| e.name == component) {
            Some(e) => current = (e.mode, e.oid),
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// A tree being assembled from entries at full paths.
#[derive(Default)]
struct Dir {
    entries: BTreeMap<Vec<u8>, Node>,
}

enum Node {
    Entry(u32, ObjectId),
    Dir(Dir),
}

impl Dir {
    fn insert(&mut self, path: &[u8], mode: u32, oid: ObjectId) {
        match path.iter().position(|b| *b == b'/') {
            None => {
                self.entries.insert(path.to_vec(), Node::Entry(mode, oid));
            }
            Some(i) => {
                let node = self
                    .entries
                    .entry(path[..i].to_vec())
                    .or_insert_with(|| Node::Dir(Dir::default()));
                if let Node::Dir(dir) = node {
                    dir.insert(&path[i + 1..], mode, oid);
                }
            }
        }
    }

    fn write(self, odb: &Odb) -> crate::error::Result<ObjectId> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for (name, node) in self.entries {
            let (mode, oid) = match node {
                Node::Entry(mode, oid) => (mode, oid),
                Node::Dir(dir) => (TREE_MODE, dir.write(odb)?),
            };
            entries.push(TreeEntry { mode, name, oid });
        }
        entries.sort_by(|a, b| {
            tree_entry_cmp(&a.name, a.mode == TREE_MODE, &b.name, b.mode == TREE_MODE)
        });
        odb.write(ObjectKind::Tree, &serialize_tree(&entries))
    }
}

/// `tree` cut down to `paths`: each path's entry where it is, and the files
/// directly in every directory above one, the top level included, as Git's
/// cone-mode sparse checkout has them. Selected directories' trees are reused
/// as they are, so only the root and the directories above nested paths are
/// written. No paths is the whole tree.
///
/// # Errors
///
/// [`CheckoutError::MissingPath`] when `tree` has no entry at one of `paths`,
/// and object read or write failures.
pub fn filtered_tree(odb: &Odb, tree: ObjectId, paths: &[String]) -> Result<ObjectId> {
    cut_tree(odb, tree, paths, true)
}

/// [`filtered_tree`], skipping paths `tree` doesn't have instead of failing.
pub(crate) fn view_tree(odb: &Odb, tree: ObjectId, paths: &[String]) -> Result<ObjectId> {
    cut_tree(odb, tree, paths, false)
}

/// Whether `path` (a file's slash-separated path) is in the view `paths`
/// give, as [`filtered_tree`] cuts it: under one of them, or directly in a
/// directory above one (the top level included). No paths is the whole tree.
pub(crate) fn in_view(path: &[u8], paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    let parent = path
        .iter()
        .rposition(|b| *b == b'/')
        .map_or(&b""[..], |i| &path[..i]);
    let under = |dir: &[u8], p: &[u8]| p.starts_with(dir) && p.get(dir.len()) == Some(&b'/');
    parent.is_empty()
        || paths
            .iter()
            .map(String::as_bytes)
            .any(|p| path == p || under(p, path) || under(parent, p))
}

fn cut_tree(odb: &Odb, tree: ObjectId, paths: &[String], strict: bool) -> Result<ObjectId> {
    if paths.is_empty() {
        return Ok(tree);
    }
    let mut dirs: Vec<&[u8]> = vec![b""];
    for p in paths {
        let p = p.as_bytes();
        dirs.extend(
            p.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'/')
                .map(|(i, _)| &p[..i]),
        );
    }
    dirs.sort();
    dirs.dedup();
    let mut root = Dir::default();
    for dir in dirs {
        let oid = if dir.is_empty() {
            tree
        } else {
            match lookup(odb, tree, dir)? {
                Some((TREE_MODE, oid)) => oid,
                _ => continue,
            }
        };
        for entry in parse_tree(&odb.read(&oid)?.data)? {
            if entry.mode == TREE_MODE {
                continue;
            }
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                [dir, b"/", &entry.name].concat()
            };
            root.insert(&path, entry.mode, entry.oid);
        }
    }
    for p in paths {
        match lookup(odb, tree, p.as_bytes())? {
            Some((mode, oid)) => root.insert(p.as_bytes(), mode, oid),
            None if strict => {
                return Err(CheckoutError::MissingPath {
                    path: p.clone(),
                    base: tree.to_hex(),
                })
            }
            None => {}
        }
    }
    Ok(root.write(odb)?)
}

/// The bytes of the snapshot commit standing for `base` with tree `tree` on
/// `parent`.
///
/// The author and committer are [`SNAPSHOT_IDENT`] at `base_time` (UTC), and
/// the message names `base`, repeats its message, and ends with a
/// [`SNAPSHOT_BASE_TRAILER`] trailer. Nothing else goes in, so the same inputs
/// always give the same commit.
#[must_use]
pub fn snapshot_object(
    tree: ObjectId,
    parent: Option<ObjectId>,
    base: ObjectId,
    base_time: i64,
    base_message: &str,
) -> Vec<u8> {
    let base = base.to_hex();
    let who = format!("{SNAPSHOT_IDENT} {base_time} +0000");
    let mut s = format!("tree {}\n", tree.to_hex());
    if let Some(p) = parent {
        s.push_str(&format!("parent {}\n", p.to_hex()));
    }
    let subject = base_message.lines().next().unwrap_or_default().trim();
    let quoted: String = base_message
        .trim_end()
        .lines()
        .map(|l| format!("    {l}").trim_end().to_owned() + "\n")
        .collect();
    s.push_str(&format!(
        "author {who}\ncommitter {who}\n\nSnapshot of {}: {subject}\n\nStands for {base}:\n\n{quoted}\n{SNAPSHOT_BASE_TRAILER}: {base}\n",
        &base[..base.len().min(12)]
    ));
    s.into_bytes()
}

/// A snapshot commit written into a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot commit.
    pub commit: ObjectId,
    /// Its tree.
    pub tree: ObjectId,
    /// The real commit it stands for.
    pub base: ObjectId,
    /// The commit's exact bytes.
    pub object: Vec<u8>,
    /// Files in the tree (submodules not counted).
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
}

/// Write the snapshot of `base` filtered to `paths` (see [`filtered_tree`])
/// on `parent` into `repo`, with the number and size of its files.
///
/// # Errors
///
/// [`CheckoutError::MissingPath`] for a path `base` lacks, and object read or
/// write failures.
pub fn make_snapshot(
    repo: &Repository,
    base: ObjectId,
    paths: &[String],
    parent: Option<ObjectId>,
) -> Result<Snapshot> {
    let odb = repo.odb();
    let commit = parse_commit(&odb.read(&base)?.data)?;
    let tree = filtered_tree(odb, commit.tree, paths).map_err(|e| match e {
        CheckoutError::MissingPath { path, .. } => CheckoutError::MissingPath {
            path,
            base: base.to_hex(),
        },
        other => other,
    })?;
    let time = crate::ident::parse_signature_times(&commit.committer)
        .map(|t| t.unix_seconds)
        .unwrap_or(0);
    let object = snapshot_object(tree, parent, base, time, &commit.message);
    let id = odb.write(ObjectKind::Commit, &object)?;
    let (files, bytes) = tree_stats(odb, tree)?;
    Ok(Snapshot {
        commit: id,
        tree,
        base,
        object,
        files,
        bytes,
    })
}

/// Files under `tree` and their total size, without reading file contents.
fn tree_stats(odb: &Odb, tree: ObjectId) -> crate::error::Result<(u64, u64)> {
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut stack = vec![tree];
    while let Some(oid) = stack.pop() {
        for entry in parse_tree(&odb.read(&oid)?.data)? {
            match entry.mode {
                TREE_MODE => stack.push(entry.oid),
                GITLINK_MODE => {}
                _ => {
                    files += 1;
                    bytes += odb.read_info(&entry.oid)?.size;
                }
            }
        }
    }
    Ok((files, bytes))
}

/// A repository URL split into what snapshot checkouts need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutUrl {
    /// The repository URL, for Git (`origin`).
    pub repo_url: String,
    /// The repository's name (last path segment, without `.git`).
    pub name: String,
    /// The checkout endpoint, `<server>/.grit/checkouts/<path>`.
    pub api: String,
}

impl CheckoutUrl {
    /// Split `url` (`http(s)://host[:port]/<path to repository>[.git]`).
    ///
    /// # Errors
    ///
    /// [`CheckoutError::InvalidUrl`] when `url` is not http(s) or names no
    /// repository.
    pub fn parse(url: &str) -> Result<Self> {
        let invalid = || CheckoutError::InvalidUrl(url.to_owned());
        let parsed = url::Url::parse(url).map_err(|_| invalid())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(invalid());
        }
        let path = parsed.path().trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.is_empty() {
            return Err(invalid());
        }
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let mut server = parsed.clone();
        server.set_path("");
        server.set_query(None);
        server.set_fragment(None);
        let server = server.as_str().trim_end_matches('/').to_owned();
        Ok(Self {
            repo_url: format!("{server}/{path}.git"),
            name,
            api: format!("{server}/{CHECKOUT_API}/{path}"),
        })
    }
}

/// Talks to a server's snapshot-checkout endpoint.
pub struct CheckoutClient<'a> {
    http: &'a dyn HttpClient,
    url: CheckoutUrl,
    given: String,
}

impl<'a> CheckoutClient<'a> {
    /// A client for the repository at `url`.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::InvalidUrl`] as for [`CheckoutUrl::parse`].
    pub fn new(http: &'a dyn HttpClient, url: &str) -> Result<Self> {
        Ok(Self {
            http,
            url: CheckoutUrl::parse(url)?,
            given: url.to_owned(),
        })
    }

    /// The parsed URL.
    #[must_use]
    pub fn url(&self) -> &CheckoutUrl {
        &self.url
    }

    fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<T> {
        let mut request = HttpRequest::new(method, url).header("Accept", "application/json");
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body);
        }
        let response = self.http.send(&request)?;
        let status = response.status;
        let bytes = response.into_bytes()?;
        if status == 404 && url == self.url.api {
            return Err(CheckoutError::NotOffered {
                url: self.given.clone(),
                endpoint: self.url.api.clone(),
            });
        }
        if !(200..300).contains(&status) {
            let message = serde_json::from_slice::<CheckoutErrorBody>(&bytes)
                .map(|b| b.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).trim().to_owned());
            return Err(CheckoutError::Server {
                url: url.to_owned(),
                status,
                message,
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| CheckoutError::Protocol {
            url: url.to_owned(),
            detail: e.to_string(),
        })
    }

    /// What the server would check out.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::NotOffered`] when the server has no checkout endpoint
    /// for the repository, and request failures.
    pub fn server_info(&self) -> Result<CheckoutServerInfo> {
        self.json(HttpMethod::Get, &self.url.api, None)
    }

    /// Make a checkout on the server.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::NotOffered`], and the server's refusal (a path it
    /// doesn't have, an empty repository).
    pub fn create(&self, request: &CheckoutRequest) -> Result<CheckoutInfo> {
        let body = serde_json::to_vec(request).map_err(|e| CheckoutError::Protocol {
            url: self.url.api.clone(),
            detail: e.to_string(),
        })?;
        self.json(HttpMethod::Post, &self.url.api, Some(&body))
    }

    /// The URL of checkout `id`'s bundle.
    #[must_use]
    pub fn bundle_url(&self, id: &str) -> String {
        format!("{}/{id}/bundle", self.url.api)
    }

    /// Checkout `id` as the server has it, and where its target branch is now
    /// ([`CheckoutState::tip`]). Changes nothing on the server, so a dry run can
    /// tell whether a refresh would move.
    ///
    /// # Errors
    ///
    /// Request failures, and the server's refusal (no such checkout).
    pub fn show(&self, id: &str) -> Result<CheckoutState> {
        self.json(HttpMethod::Get, &format!("{}/{id}", self.url.api), None)
    }

    fn post_json<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: HttpMethod,
        url: &str,
        body: &T,
    ) -> Result<R> {
        let body = serde_json::to_vec(body).map_err(|e| CheckoutError::Protocol {
            url: url.to_owned(),
            detail: e.to_string(),
        })?;
        self.json(method, url, Some(&body))
    }

    /// Ask the server to graft branches the client pushed to [`out_ref`].
    ///
    /// # Errors
    ///
    /// Request failures and the server's refusal of the whole request.
    pub fn checkin(&self, id: &str, request: &CheckinRequest) -> Result<CheckinResponse> {
        self.post_json(
            HttpMethod::Post,
            &format!("{}/{id}/checkin", self.url.api),
            request,
        )
    }

    /// Ask the server for a snapshot of the newest target branch on top of
    /// checkout `id`'s last one (kept at [`target_ref`]).
    ///
    /// # Errors
    ///
    /// Request failures.
    pub fn update(&self, id: &str) -> Result<CheckoutInfo> {
        self.post_json(
            HttpMethod::Post,
            &format!("{}/{id}/update", self.url.api),
            &(),
        )
    }

    /// Reserve a fresh work branch for checkout `id`: `wanted`, numbered past
    /// the server's branches and the names it gave out before.
    ///
    /// # Returns
    ///
    /// The name reserved, which the server never gives out again.
    ///
    /// # Errors
    ///
    /// Request failures, and the server's refusal (no such checkout, a name
    /// that is not a valid branch name).
    pub fn reserve_work_branch(&self, id: &str, wanted: &str) -> Result<String> {
        let reserved: WorkBranchResponse = self.post_json(
            HttpMethod::Post,
            &format!("{}/{id}/work", self.url.api),
            &WorkBranchRequest {
                work: wanted.to_owned(),
            },
        )?;
        Ok(reserved.work)
    }

    /// Merge a branch into the target on the server.
    ///
    /// # Errors
    ///
    /// Request failures, and the server's refusal (a conflict, no such branch).
    pub fn integrate(&self, request: &IntegrateRequest) -> Result<IntegrateResponse> {
        self.post_json(
            HttpMethod::Post,
            &format!("{}/integrate", self.url.api),
            request,
        )
    }

    /// The slices on the target branch.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::NotOffered`] and request failures.
    pub fn slices(&self) -> Result<SlicesResponse> {
        self.server_info()?;
        self.json(HttpMethod::Get, &format!("{}/slices", self.url.api), None)
    }

    /// Make or change slice `name`.
    ///
    /// # Errors
    ///
    /// Request failures and the server's refusal (a name that exists or
    /// doesn't, a path the target lacks).
    pub fn put_slice(&self, name: &str, request: &SliceRequest) -> Result<SliceChange> {
        self.post_json(
            HttpMethod::Put,
            &format!("{}/slices/{name}", self.url.api),
            request,
        )
    }

    /// Delete slice `name`.
    ///
    /// # Errors
    ///
    /// Request failures and the server's refusal (no such slice).
    pub fn delete_slice(&self, name: &str, request: &SliceDeleteRequest) -> Result<SliceChange> {
        self.post_json(
            HttpMethod::Delete,
            &format!("{}/slices/{name}", self.url.api),
            request,
        )
    }
}

/// Local branches with work to check in: every branch whose tip is a commit
/// on top of one of the checkout's snapshots (not a snapshot itself).
///
/// # Errors
///
/// Ref and ancestry failures.
pub fn branches_with_work(
    repo: &Repository,
    marker: &CheckoutMarker,
) -> Result<Vec<(String, ObjectId)>> {
    let roots: Vec<ObjectId> = marker
        .roots
        .iter()
        .filter_map(|r| ObjectId::from_hex(&r.commit).ok())
        .collect();
    let mut out = Vec::new();
    for (refname, tip) in crate::refs::list_refs(repo.git_dir(), "refs/heads/")? {
        if roots.contains(&tip) {
            continue;
        }
        let mut on_snapshot = false;
        for root in &roots {
            if crate::merge_base::is_ancestor(repo, *root, tip)? {
                on_snapshot = true;
                break;
            }
        }
        if on_snapshot {
            let name = refname
                .strip_prefix("refs/heads/")
                .unwrap_or(&refname)
                .to_owned();
            out.push((name, tip));
        }
    }
    Ok(out)
}

/// What makes a repository a snapshot checkout: where it came from and which
/// snapshot commits it has. Kept as [`MARKER_FILE`] in the repository's common
/// git directory, so every worktree of a checkout shares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutMarker {
    /// The repository URL it was checked out from.
    pub url: String,
    /// The checkout's id on the server.
    pub id: String,
    /// The target branch.
    pub branch: String,
    /// The checkout's paths.
    pub paths: Vec<String>,
    /// The snapshot commits, oldest first.
    pub roots: Vec<SnapshotRoot>,
    /// Tips of branches this checkout checked in that have landed in
    /// [`Self::branch`] (the server merged them into it, or it had them
    /// already): every later snapshot has their commits' changes, though not
    /// as ancestors, so refreshes never replay them ([`land_integration`]).
    /// A tip no branch, and no worktree's detached `HEAD`, builds on any
    /// more is dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub landed: Vec<String>,
}

/// Where the marker of the repository whose git directory is `git_dir` is:
/// in its common git directory, which a linked worktree's `commondir` names.
fn marker_path(git_dir: &Path) -> PathBuf {
    crate::refs::common_dir(git_dir)
        .unwrap_or_else(|| git_dir.to_path_buf())
        .join(MARKER_FILE)
}

impl CheckoutMarker {
    /// Read the marker of the repository whose git directory is `git_dir`
    /// (the main one or a linked worktree's).
    ///
    /// # Returns
    ///
    /// `None` when it is not a snapshot checkout.
    #[must_use]
    pub fn read(git_dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(marker_path(git_dir)).ok()?).ok()
    }

    /// Write it into the common git directory of the repository whose git
    /// directory is `git_dir`, replacing the old one in one rename.
    ///
    /// # Errors
    ///
    /// File write failures.
    pub fn write(&self, git_dir: &Path) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        let path = marker_path(git_dir);
        let tmp = path.with_file_name(format!("{MARKER_FILE}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The checkout's newest snapshot, the last of [`Self::roots`].
    #[must_use]
    pub fn last(&self) -> Option<&SnapshotRoot> {
        self.roots.last()
    }

    /// The local ref the checkout keeps its newest snapshot at
    /// ([`upstream_ref`] of [`Self::branch`]).
    #[must_use]
    pub fn upstream_ref(&self) -> String {
        upstream_ref(&self.branch)
    }
}

/// A checkout built on disk.
#[derive(Debug)]
pub struct Materialized {
    /// The new repository.
    pub repo: Repository,
    /// The work branch checked out.
    pub work: String,
    /// Files written to the work tree.
    pub files: u64,
}

/// Build the checkout described by `info` in `dest` from its downloaded
/// `bundle`.
///
/// Creates the repository, unpacks the bundle, checks that the snapshot
/// commit is byte for byte the one the server described, points
/// [`upstream_ref`] (`refs/remotes/origin/<branch>`, which
/// `refs/remotes/origin/HEAD` names) and the new branch `work` at it,
/// configures `origin` as `repo_url` fetching [`snapshot_fetch_refspec`] with
/// no tags and pushing [`out_push_refspec`], writes the [`CheckoutMarker`] and
/// checks out the files.
///
/// # Errors
///
/// [`CheckoutError::DestinationNotEmpty`], [`CheckoutError::Mismatch`] for a
/// download that is not the described snapshot, and repository failures.
#[allow(clippy::too_many_arguments)]
pub fn materialize(
    dest: &Path,
    info: &CheckoutInfo,
    bundle: &Path,
    repo_url: &str,
    work: &str,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<Materialized> {
    if dest.is_dir() && std::fs::read_dir(dest)?.next().is_some() {
        return Err(CheckoutError::DestinationNotEmpty(dest.to_path_buf()));
    }
    std::fs::create_dir_all(dest)?;
    let repo = Repository::init(
        dest,
        &InitOptions {
            initial_branch: Some(work.to_owned()),
            ..InitOptions::default()
        },
    )?;
    let opened = repo
        .bundles()
        .open(bundle)
        .map_err(|e| crate::error::Error::Message(format!("bundle: {e}")))?;
    let unbundled = opened
        .unbundle_with_progress(&repo, progress, cancel)
        .map_err(|e| crate::error::Error::Message(format!("bundle: {e}")))?;
    let mismatch = |got: String| CheckoutError::Mismatch {
        expected: info.root.clone(),
        got,
    };
    let root = ObjectId::from_hex(&info.root).map_err(|_| mismatch(info.root.clone()))?;
    let tip = unbundled
        .refs
        .first()
        .map(|r| r.oid)
        .ok_or_else(|| mismatch("an empty bundle".to_owned()))?;
    if tip != root {
        return Err(mismatch(tip.to_hex()));
    }
    let object = repo.odb().read(&root)?;
    if object.data != info.root_object.as_bytes() {
        return Err(mismatch(format!(
            "{} with different contents",
            root.to_hex()
        )));
    }
    let git_dir = repo.git_dir().to_path_buf();
    let upstream = upstream_ref(&info.branch);
    crate::refs::write_ref(&git_dir, &upstream, &root)?;
    crate::refs::write_symbolic_ref(&git_dir, "refs/remotes/origin/HEAD", &upstream)?;
    crate::refs::write_ref(&git_dir, &format!("refs/heads/{work}"), &root)?;
    crate::refs::write_symbolic_ref(&git_dir, "HEAD", &format!("refs/heads/{work}"))?;
    let config_path = git_dir.join("config");
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = crate::config::ConfigFile::parse(
        &config_path,
        &content,
        crate::config::ConfigScope::Local,
    )?;
    config.set("remote.origin.url", repo_url)?;
    config.set(
        "remote.origin.fetch",
        &snapshot_fetch_refspec(&info.id, &info.branch),
    )?;
    config.set("remote.origin.push", &out_push_refspec(&info.id))?;
    config.set("remote.origin.tagOpt", "--no-tags")?;
    config.write()?;
    CheckoutMarker {
        url: repo_url.to_owned(),
        id: info.id.clone(),
        branch: info.branch.clone(),
        paths: info.paths.clone(),
        roots: info.roots.clone(),
        landed: Vec::new(),
    }
    .write(&git_dir)?;
    let tree = ObjectId::from_hex(&info.tree).map_err(|_| mismatch(info.tree.clone()))?;
    crate::porcelain::checkout::checkout_between_trees_with_progress(
        &repo, None, &tree, progress, cancel,
    )?;
    let files = u64::try_from(repo.index_handle().load()?.entries().len()).unwrap_or(u64::MAX);
    Ok(Materialized {
        repo,
        work: work.to_owned(),
        files,
    })
}

const WORDS: &[&str] = &[
    "amber", "brook", "cedar", "dune", "ember", "fern", "glade", "harbor", "iris", "juniper",
    "kestrel", "lagoon", "maple", "nectar", "orchid", "pine", "quartz", "river", "sage", "thistle",
    "umber", "violet", "willow", "yarrow", "zephyr", "aspen", "birch", "coral", "delta", "elm",
    "flint", "grove", "hazel", "inlet", "jasper", "lark", "moss", "north", "oak", "pebble",
    "quill", "reed", "slate", "tide", "vale", "wren",
];

/// A work branch name for a new checkout: the local part of `email` and two
/// words chosen by `random`, like `alice/quiet-river`.
#[must_use]
pub fn suggest_work_branch(email: Option<&str>, random: [u8; 2]) -> String {
    let who: String = email
        .and_then(|e| e.split('@').next())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let who = who.trim_matches('-');
    let a = WORDS[usize::from(random[0]) % WORDS.len()];
    let b = WORDS[usize::from(random[1]) % WORDS.len()];
    if who.is_empty() {
        format!("checkout/{a}-{b}")
    } else {
        format!("{who}/{a}-{b}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents_resolve_like_commit_authors_and_sign_cleanly() {
        let env_of = |vars: &[(&str, &str)]| {
            crate::config::environment::Environment::from_vars(
                vars.iter().map(|(k, v)| ((*k).into(), (*v).into())),
                std::path::PathBuf::from("/"),
            )
        };
        let env = env_of(&[
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("GIT_AUTHOR_EMAIL", "author@example.com"),
        ]);
        let mut config = crate::config::ConfigSet::new();
        let _ = config.add_command_override("user.name", "Alice");
        let _ = config.add_command_override("user.email", "alice@example.com");
        assert_eq!(
            Ident::author(&env, &config).ok(),
            Some(Ident {
                name: "A U Thor".to_owned(),
                email: "author@example.com".to_owned()
            }),
            "the environment wins over config, as for a commit"
        );
        let bare = env_of(&[]);
        assert_eq!(
            Ident::author(&bare, &config).ok(),
            Some(Ident {
                name: "Alice".to_owned(),
                email: "alice@example.com".to_owned()
            })
        );
        assert!(Ident::author(&bare, &crate::config::ConfigSet::new()).is_err());

        let time = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let sneaky = Ident {
            name: "Eve\nencoding bogus".to_owned(),
            email: "e>v<e@x".to_owned(),
        };
        assert_eq!(
            sneaky
                .signature(time)
                .map(|s| s.header_line())
                .ok()
                .as_deref(),
            Some("Eveencoding bogus <eve@x> 1700000000 +0000")
        );
        let nameless = Ident {
            name: "<>".to_owned(),
            email: "bob@example.com".to_owned(),
        };
        assert_eq!(
            nameless.signature(time).map(|s| s.name).ok().as_deref(),
            Some("bob@example.com")
        );
    }

    #[test]
    fn paths_are_normalized() {
        let paths: Vec<String> = ["b/", "a/x", "a", " c ", "", "a/y"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            normalize_paths(&paths).ok(),
            Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
        assert!(normalize_paths(&["/abs".to_owned()]).is_err());
        assert!(normalize_paths(&["a/../b".to_owned()]).is_err());
        assert!(normalize_paths(&["a//b".to_owned()]).is_err());
    }

    #[test]
    fn views_are_the_paths_and_the_files_above_them() {
        let paths = vec!["src/deep".to_owned(), "docs/guide.md".to_owned()];
        for (path, seen) in [
            ("README", true),
            ("src/a.rs", true),
            ("src/deep/b.rs", true),
            ("src/deep/more/c.rs", true),
            ("src/other/d.rs", false),
            ("docs/guide.md", true),
            ("docs/other.md", true),
            ("docs/api/x.md", false),
            ("srcx/a.rs", false),
            ("src/deeper/a.rs", false),
        ] {
            assert_eq!(in_view(path.as_bytes(), &paths), seen, "{path}");
        }
        assert!(in_view(b"anything/at/all", &[]));
    }

    #[test]
    fn urls_split_into_repository_and_endpoint() {
        let u = CheckoutUrl::parse("http://h:8080/demo.git").ok();
        assert_eq!(
            u,
            Some(CheckoutUrl {
                repo_url: "http://h:8080/demo.git".to_owned(),
                name: "demo".to_owned(),
                api: "http://h:8080/.grit/checkouts/demo".to_owned(),
            })
        );
        let nested = CheckoutUrl::parse("https://h/org/app/").ok();
        assert_eq!(nested.as_ref().map(|u| u.name.as_str()), Some("app"));
        assert_eq!(
            nested.map(|u| u.api),
            Some("https://h/.grit/checkouts/org/app".to_owned())
        );
        assert!(CheckoutUrl::parse("http://h/").is_err());
        assert!(CheckoutUrl::parse("ssh://h/demo").is_err());
    }

    #[test]
    fn snapshot_objects_are_deterministic() {
        let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").ok();
        let base = ObjectId::from_hex("1111111111111111111111111111111111111111").ok();
        let (Some(tree), Some(base)) = (tree, base) else {
            panic!("oids");
        };
        let a = snapshot_object(tree, None, base, 1_700_000_000, "Fix it\n\nBody\n");
        assert_eq!(
            a,
            snapshot_object(tree, None, base, 1_700_000_000, "Fix it\n\nBody\n")
        );
        let text = String::from_utf8_lossy(&a);
        assert!(text.starts_with("tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor grit-checkout <snapshot@grit-checkout> 1700000000 +0000\n"), "{text}");
        assert!(
            text.contains("\n\nSnapshot of 111111111111: Fix it\n"),
            "{text}"
        );
        assert!(
            text.ends_with("Grit-Checkout-Base: 1111111111111111111111111111111111111111\n"),
            "{text}"
        );
    }

    #[test]
    fn work_branches_are_named_for_the_committer() {
        assert_eq!(
            suggest_work_branch(Some("Alice.B@example.com"), [0, 1]),
            "alice-b/amber-brook"
        );
        assert_eq!(suggest_work_branch(None, [2, 3]), "checkout/cedar-dune");
    }
}
