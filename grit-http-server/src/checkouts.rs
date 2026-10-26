//! Snapshot checkouts for `grit serve` ([`grit_lib::remote::snapshot_checkout`]).
//!
//! These live at their own endpoint, apart from the Git smart-HTTP paths, so
//! `grit checkout` can tell a server that offers them from one that doesn't:
//!
//! | Request | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /.grit/checkouts/<repo>` | read | JSON [`CheckoutServerInfo`] |
//! | `POST /.grit/checkouts/<repo>` | read | make a checkout: JSON [`CheckoutInfo`] |
//! | `GET /.grit/checkouts/<repo>/<id>` | read | JSON [`CheckoutState`]: the record and where its target is now |
//! | `GET /.grit/checkouts/<repo>/<id>/bundle` | read | the snapshot as a bundle, with `Range` |
//! | `POST /.grit/checkouts/<repo>/<id>/update` | read | refresh: JSON [`CheckoutInfo`] |
//! | `POST /.grit/checkouts/<repo>/<id>/checkin` | write | JSON [`CheckinResponse`] |
//! | `POST /.grit/checkouts/<repo>/<id>/work` | read | a fresh work branch: JSON [`WorkBranchResponse`] |
//! | `POST /.grit/checkouts/<repo>/integrate` | write | JSON [`IntegrateResponse`] |
//! | `/.grit/checkouts/<repo>/slices…` | read / write | the slices on the target |
//!
//! A checkout is a snapshot of the repository's target branch (where `HEAD`
//! points when it is made) cut down to the paths asked for. Snapshots are
//! deterministic, so checkouts of the same paths at the same commit share one,
//! and its bundle is built once. The server keeps each checkout's record in
//! the repository's [`CheckoutStore`] and points `refs/checkouts/<id>/target`
//! at its snapshot, which keeps the snapshot's objects alive. A refresh
//! snapshots the branch the checkout was made from.
//!
//! Requests on one checkout take turns, and so do check-ins setting the
//! repository's branches and the requests that move its target branch
//! (integrations and slice changes), on locks the server keeps per
//! repository. When the served repository has the target checked out in a
//! worktree, the worktree moves with it, keeping local changes. Plain pushes
//! never change the target: receive-pack refuses them
//! ([`grit_protocol::PushGuard`]).

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use grit_lib::bundle::BundleSpec;
use grit_lib::ident::Signature;
use grit_lib::objects::ObjectId;
use grit_lib::remote::snapshot_checkout::{
    edit_slices, graft_checkin_branch, integrate, make_snapshot, normalize_paths, read_slices,
    set_checked_in_branch, target_branch, target_ref, valid_checkout_id, CheckinBranch,
    CheckinRefusal, CheckinRequest, CheckinResponse, CheckinResult, CheckinRule, CheckinStatus,
    CheckoutError, CheckoutErrorBody, CheckoutInfo, CheckoutRecord, CheckoutRequest,
    CheckoutServerInfo, CheckoutState, CheckoutStore, Ident, IntegrateRequest, IntegrateResponse,
    Slice, SliceChange, SliceDeleteRequest, SliceEdit, SliceRequest, SlicesEdited, SlicesResponse,
    SnapshotRoot, StoredCheckout, TargetChange, TargetRefusal, WorkBranchRequest,
    WorkBranchResponse,
};
use grit_lib::repo::Repository;
use grit_protocol::RepositoryOptions;

use crate::checkout_locks::{CheckoutSlot, RepoLocks, TargetSlot};
use crate::server::{ServedRepository, ServerState};
use crate::store::Access;

/// An error response, boxed because responses are large.
type Rejection = Box<Response<Body>>;

fn error(status: StatusCode, message: impl Into<String>) -> Rejection {
    Box::new(
        (
            status,
            Json(CheckoutErrorBody {
                error: message.into(),
            }),
        )
            .into_response(),
    )
}

fn open(repo: &ServedRepository, options: &RepositoryOptions) -> Result<Repository, Rejection> {
    Repository::open(&repo.path, &options.clone().for_serving())
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// The tip of `branch`, which a checkout is made or refreshed from.
fn branch_tip(repo: &Repository, branch: &str) -> Result<ObjectId, Rejection> {
    grit_lib::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{branch}")).map_err(|_| {
        error(
            StatusCode::CONFLICT,
            format!("{branch} has no commits, so there is nothing to check out"),
        )
    })
}

/// The target branch's short name and tip.
fn target(repo: &Repository) -> Result<(String, ObjectId), Rejection> {
    let branch = target_branch(repo);
    let tip = branch_tip(repo, &branch)?;
    Ok((branch, tip))
}

/// Run `f` for a request on `name` that needs `needed` access, with the
/// signed-in user's email and the repository's locks.
async fn with_access<T: Send + 'static>(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    name: &str,
    needed: Access,
    f: impl FnOnce(ServedRepository, String, RepositoryOptions, &RepoLocks) -> Result<T, Rejection>
        + Send
        + 'static,
) -> Result<T, Rejection> {
    let found = state.authorize(headers, name, needed)?;
    let options = state.options.clone();
    let locks = state.checkout_locks.repo(&found.repo.path);
    tokio::task::spawn_blocking(move || f(found.repo, found.email, options, &locks))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Run `f` for an authorized read of `name`.
async fn with_repo<T: Send + 'static>(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    name: &str,
    f: impl FnOnce(ServedRepository, RepositoryOptions) -> Result<T, Rejection> + Send + 'static,
) -> Result<T, Rejection> {
    let found = state.authorize(headers, name, Access::Read)?;
    let options = state.options.clone();
    tokio::task::spawn_blocking(move || f(found.repo, options))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Run `f` for a request on checkout `id` of `name` that needs `needed`
/// access, holding the checkout until it is done, with the signed-in user's
/// email and the repository's locks.
async fn with_checkout<T: Send + 'static>(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    name: &str,
    id: String,
    needed: Access,
    f: impl FnOnce(
            ServedRepository,
            String,
            RepositoryOptions,
            &RepoLocks,
            &mut CheckoutSlot,
        ) -> Result<T, Rejection>
        + Send
        + 'static,
) -> Result<T, Rejection> {
    let found = state.authorize(headers, name, needed)?;
    if !valid_checkout_id(&id) {
        return Err(no_checkout(&id));
    }
    let options = state.options.clone();
    let locks = state.checkout_locks.repo(&found.repo.path);
    let mut slot = locks.checkout(&id).await;
    tokio::task::spawn_blocking(move || f(found.repo, found.email, options, &locks, &mut slot))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Run `f` for a request on `name` that moves its target branch and needs
/// `needed` access, holding the target until it is done, with the signed-in
/// user's email.
async fn with_target<T: Send + 'static>(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    name: &str,
    needed: Access,
    f: impl FnOnce(ServedRepository, String, RepositoryOptions, &mut TargetSlot) -> Result<T, Rejection>
        + Send
        + 'static,
) -> Result<T, Rejection> {
    let found = state.authorize(headers, name, needed)?;
    let options = state.options.clone();
    let locks = state.checkout_locks.repo(&found.repo.path);
    let mut slot = locks.target().await;
    tokio::task::spawn_blocking(move || f(found.repo, found.email, options, &mut slot))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// `GET /.grit/checkouts/<repo>`.
pub(crate) async fn server_info(
    State(state): State<Arc<ServerState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let result = with_repo(&state, &headers, &name, |served, options| {
        let repo = open(&served, &options)?;
        let (branch, head) = target(&repo)?;
        Ok(CheckoutServerInfo {
            name: served.name,
            branch,
            head: head.to_hex(),
        })
    })
    .await;
    match result {
        Ok(info) => Json(info).into_response(),
        Err(resp) => *resp,
    }
}

/// `POST /.grit/checkouts/<repo>`: make a checkout at the target branch's tip.
pub(crate) async fn create(
    State(state): State<Arc<ServerState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Json(request): Json<CheckoutRequest>,
) -> Response<Body> {
    let result = with_access(
        &state,
        &headers,
        &name,
        Access::Read,
        move |served, _, options, locks| {
            let repo = open(&served, &options)?;
            create_blocking(&repo, &served, locks, &request)
        },
    )
    .await;
    match result {
        Ok(info) => Json(info).into_response(),
        Err(resp) => *resp,
    }
}

fn create_blocking(
    repo: &Repository,
    served: &ServedRepository,
    locks: &RepoLocks,
    request: &CheckoutRequest,
) -> Result<CheckoutInfo, Rejection> {
    let paths = normalize_paths(&request.paths)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
    let (branch, head) = target(repo)?;
    let snapshot = make_snapshot(repo, head, &paths, None).map_err(|e| match e {
        CheckoutError::MissingPath { .. } | CheckoutError::InvalidPath { .. } => {
            error(StatusCode::BAD_REQUEST, e.to_string())
        }
        other => internal(&other),
    })?;
    let store = CheckoutStore::for_repository(repo);
    std::fs::create_dir_all(store.dir().join("bundles")).map_err(|e| internal(&e))?;
    let bundle = bundle_path(&store, &snapshot.commit.to_hex());
    if !bundle.is_file() {
        build_bundle(repo, snapshot.commit, &branch, &bundle).map_err(|e| internal(&e))?;
    }
    let bundle_bytes = std::fs::metadata(&bundle).map(|m| m.len()).unwrap_or(0);
    let work = request
        .work
        .as_deref()
        .map(|wanted| reserve_work_branch(repo, locks, wanted))
        .transpose()?;
    let mut random = [0u8; 10];
    getrandom::getrandom(&mut random).map_err(|e| internal(&e))?;
    let id = hex::encode(random);
    let mut record = store
        .record(&id)
        .ok_or_else(|| internal(&format!("made an invalid checkout id {id}")))?;
    grit_lib::refs::write_ref(repo.git_dir(), &target_ref(&id), &snapshot.commit)
        .map_err(|e| internal(&e))?;
    let info = CheckoutInfo {
        id: id.clone(),
        name: served.name.clone(),
        branch,
        paths,
        base: head.to_hex(),
        root: snapshot.commit.to_hex(),
        root_object: String::from_utf8_lossy(&snapshot.object).into_owned(),
        tree: snapshot.tree.to_hex(),
        roots: vec![SnapshotRoot {
            commit: snapshot.commit.to_hex(),
            base: head.to_hex(),
        }],
        files: snapshot.files,
        bytes: snapshot.bytes,
        bundle_bytes,
        work,
    };
    record
        .save(&StoredCheckout::new(info.clone()))
        .map_err(|e| internal(&e))?;
    eprintln!(
        "checkout: {id} of {} for {}: {} files at {} → {}",
        served.name,
        request.client.as_deref().unwrap_or("a client"),
        info.files,
        &info.base[..12],
        &info.root[..12],
    );
    Ok(info)
}

/// Give out a work-branch name for a checkout of `repo`: `wanted`, numbered
/// past the branches the repository has and the names given out before, and
/// never given out again.
///
/// # Errors
///
/// `400` for a name that is not a valid branch name, `500` when the
/// reservation can't be written.
pub(crate) fn reserve_work_branch(
    repo: &Repository,
    locks: &RepoLocks,
    wanted: &str,
) -> Result<String, Rejection> {
    let mut names = locks.branches();
    let reserved = CheckoutStore::for_repository(repo)
        .reserve_work_branch(repo, wanted, names.start(wanted))
        .map_err(|e| match e {
            CheckoutError::InvalidBranchName(_) => error(StatusCode::BAD_REQUEST, e.to_string()),
            other => internal(&other),
        })?;
    names.reached(wanted, reserved.next);
    Ok(reserved.name)
}

fn bundle_path(store: &CheckoutStore, commit: &str) -> std::path::PathBuf {
    store.dir().join("bundles").join(format!("{commit}.bundle"))
}

/// Write the bundle of `commit` (with the branch's name in its header).
fn build_bundle(
    repo: &Repository,
    commit: ObjectId,
    branch: &str,
    path: &Path,
) -> std::io::Result<()> {
    let spec = BundleSpec {
        include: vec![(commit, format!("refs/heads/{branch}"))],
        exclude: Vec::new(),
        filter: None,
    };
    let tmp = path.with_extension("bundle.tmp");
    {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        grit_lib::bundle::write_bundle(repo, &spec, &mut out)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn no_checkout(id: &str) -> Rejection {
    error(StatusCode::NOT_FOUND, format!("no checkout {id}"))
}

/// Read checkout `id`'s record, for a request that only reads it.
fn load(repo: &Repository, id: &str) -> Result<StoredCheckout, Rejection> {
    let record = CheckoutStore::for_repository(repo)
        .record(id)
        .ok_or_else(|| no_checkout(id))?;
    record
        .load()
        .map_err(|e| internal(&e))?
        .ok_or_else(|| no_checkout(id))
}

/// Checkout `id`'s record from the slot the request holds, and what it says.
fn load_held<'a>(
    slot: &'a mut CheckoutSlot,
    repo: &Repository,
    id: &str,
) -> Result<(&'a mut CheckoutRecord, StoredCheckout), Rejection> {
    let record = slot
        .record(&CheckoutStore::for_repository(repo), id)
        .ok_or_else(|| no_checkout(id))?;
    let stored = record
        .load()
        .map_err(|e| internal(&e))?
        .ok_or_else(|| no_checkout(id))?;
    Ok((record, stored))
}

/// `GET /.grit/checkouts/<repo>/<id>`: the checkout's record, and where the
/// branch it was made from is now, so a client can tell whether a refresh
/// would move without making a snapshot.
pub(crate) async fn show(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let result = with_repo(&state, &headers, &name, move |served, options| {
        let repo = open(&served, &options)?;
        let info = load(&repo, &id)?.info;
        let tip = branch_tip(&repo, &info.branch).ok().map(|t| t.to_hex());
        Ok(CheckoutState { info, tip })
    })
    .await;
    match result {
        Ok(info) => Json(info).into_response(),
        Err(resp) => *resp,
    }
}

/// `GET /.grit/checkouts/<repo>/<id>/bundle`: the snapshot, resumable with `Range`.
pub(crate) async fn bundle(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let result = with_repo(&state, &headers, &name, move |served, options| {
        let repo = open(&served, &options)?;
        let info = load(&repo, &id)?.info;
        let path = bundle_path(&CheckoutStore::for_repository(&repo), &info.root);
        if !path.is_file() {
            let commit = ObjectId::from_hex(&info.root)
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            build_bundle(&repo, commit, &info.branch, &path)
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        Ok((path, info.root))
    })
    .await;
    match result {
        Ok((path, root)) => {
            crate::smart_http::serve_file(
                path,
                &format!("\"{root}\""),
                "application/x-git-bundle",
                &headers,
            )
            .await
        }
        Err(resp) => *resp,
    }
}

fn respond<T: serde::Serialize>(result: Result<T, Rejection>) -> Response<Body> {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(resp) => *resp,
    }
}

fn internal(e: &dyn std::fmt::Display) -> Rejection {
    error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// The signature of a server-made commit, made now: the request's author,
/// or the signed-in user. Anything in the request that would break the
/// commit's header (`<`, `>`, newlines) is cleaned out.
fn signature(author: Option<&Ident>, email: &str) -> Result<Signature, Rejection> {
    let user = Ident {
        name: email.to_owned(),
        email: email.to_owned(),
    };
    author
        .unwrap_or(&user)
        .signature(time::OffsetDateTime::now_utc())
        .map_err(|e| internal(&e))
}

/// The answer to a change of the target branch the library refused, in words.
fn target_refused(refusal: &TargetRefusal) -> Rejection {
    let list = |paths: &[String]| paths.join(", ");
    let (status, message) = match refusal {
        TargetRefusal::InvalidName { name } => (
            StatusCode::BAD_REQUEST,
            format!("{name} is not a valid branch name"),
        ),
        TargetRefusal::IsTarget { target } => (
            StatusCode::BAD_REQUEST,
            format!("{target} is the server's target branch; name the branch to merge into it"),
        ),
        TargetRefusal::NoSuchBranch { name } => (
            StatusCode::NOT_FOUND,
            format!("the server has no branch {name}; check it in first"),
        ),
        TargetRefusal::Unborn { target } => (
            StatusCode::CONFLICT,
            format!("{target} has no commits yet"),
        ),
        TargetRefusal::Unrelated { branch, target } => (
            StatusCode::CONFLICT,
            format!(
                "{branch} shares no history with {target}, so it can't be merged into it; check your work in from a snapshot checkout (`grit checkin`) or rebase {branch} onto {target}, then integrate again"
            ),
        ),
        TargetRefusal::Conflict {
            branch,
            target,
            paths,
        } => (
            StatusCode::CONFLICT,
            format!(
                "{branch} conflicts with {target} in {}; rebase it onto the newest {target} and try again",
                list(paths)
            ),
        ),
        TargetRefusal::InvalidSliceName { slice } => (
            StatusCode::BAD_REQUEST,
            format!("{slice} is not a valid slice name: use letters, digits, `-`, `_` and `.`"),
        ),
        TargetRefusal::SliceWithoutPaths { .. } => (
            StatusCode::BAD_REQUEST,
            "a slice needs at least one path".to_owned(),
        ),
        TargetRefusal::MissingPaths { target, paths } => (
            StatusCode::BAD_REQUEST,
            format!("{} not in {target}", list(paths)),
        ),
        TargetRefusal::SliceExists { slice } => (
            StatusCode::CONFLICT,
            format!("slice {slice} already exists"),
        ),
        TargetRefusal::NoSuchSlice { slice } => (
            StatusCode::NOT_FOUND,
            format!("there is no slice {slice}"),
        ),
        TargetRefusal::WorktreeBlocked { target, paths } => (
            StatusCode::CONFLICT,
            format!(
                "{target} is checked out in the server's worktree, where uncommitted changes to {} would be overwritten; commit or stash them there and try again",
                list(paths)
            ),
        ),
        TargetRefusal::Moved { target } => (
            StatusCode::CONFLICT,
            format!("{target} kept moving while it was being changed; try again"),
        ),
    };
    error(status, message)
}

/// The log line's tail for a change of the target: whether the served
/// worktree moved with it, and whether something outside the server had
/// moved the target since the server last did.
fn target_change_note(slot: &mut TargetSlot, change: &TargetChange) -> String {
    let mut note = String::new();
    if change.worktree_moved {
        note.push_str("; the server's worktree moved with it");
    }
    if let Some(last) = slot.changed(change) {
        note.push_str(&format!(
            "; it had moved since the server left it at {}",
            &last.to_hex()[..12]
        ));
    }
    note
}

/// `POST /.grit/checkouts/<repo>/<id>/update`: a snapshot of the newest tip
/// of the branch the checkout was made from, on top of the checkout's last one.
pub(crate) async fn update(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    respond(
        with_checkout(
            &state,
            &headers,
            &name,
            id.clone(),
            Access::Read,
            move |served, _, options, _, slot| {
                let repo = open(&served, &options)?;
                let (record, mut stored) = load_held(slot, &repo, &id)?;
                let info = &mut stored.info;
                let head = branch_tip(&repo, &info.branch).map_err(|_| {
                    error(
                        StatusCode::CONFLICT,
                        format!(
                            "the server has no branch {} any more, which this checkout was made from",
                            info.branch
                        ),
                    )
                })?;
                if info.base == head.to_hex() {
                    return Ok(stored.info);
                }
                let parent = ObjectId::from_hex(&info.root).map_err(|e| internal(&e))?;
                let snapshot =
                    make_snapshot(&repo, head, &info.paths, Some(parent)).map_err(|e| match e {
                        CheckoutError::MissingPath { .. } => error(
                            StatusCode::CONFLICT,
                            format!("{e}; the checkout's paths are gone from {}", info.branch),
                        ),
                        other => internal(&other),
                    })?;
                // The checkout's own ref, written only while its lock is held.
                grit_lib::refs::write_ref(repo.git_dir(), &target_ref(&id), &snapshot.commit)
                    .map_err(|e| internal(&e))?;
                info.base = head.to_hex();
                info.root = snapshot.commit.to_hex();
                info.root_object = String::from_utf8_lossy(&snapshot.object).into_owned();
                info.tree = snapshot.tree.to_hex();
                info.files = snapshot.files;
                info.bytes = snapshot.bytes;
                info.roots.push(SnapshotRoot {
                    commit: info.root.clone(),
                    base: info.base.clone(),
                });
                record.save(&stored).map_err(|e| internal(&e))?;
                let info = stored.info;
                eprintln!(
                    "checkout: {id} of {} refreshed to {} → {}",
                    served.name,
                    &info.base[..12],
                    &info.root[..12]
                );
                Ok(info)
            },
        )
        .await,
    )
}

/// `POST /.grit/checkouts/<repo>/<id>/work`: reserve a fresh work branch for
/// the checkout, as making one does, for a client whose branch was just
/// integrated and that moves on to a new one.
pub(crate) async fn work_branch(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<WorkBranchRequest>,
) -> Response<Body> {
    respond(
        with_checkout(
            &state,
            &headers,
            &name,
            id.clone(),
            Access::Read,
            move |served, _, options, locks, slot| {
                let repo = open(&served, &options)?;
                load_held(slot, &repo, &id)?;
                let work = reserve_work_branch(&repo, locks, &request.work)?;
                eprintln!(
                    "checkout: {id} of {} moves on to work branch {work}",
                    served.name
                );
                Ok(WorkBranchResponse { work })
            },
        )
        .await,
    )
}

/// `POST /.grit/checkouts/<repo>/<id>/checkin`: graft pushed branches onto
/// the real history and set the server's branches.
pub(crate) async fn checkin(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<CheckinRequest>,
) -> Response<Body> {
    let result = with_checkout(
        &state,
        &headers,
        &name,
        id.clone(),
        Access::Write,
        move |served, email, options, locks, slot| {
            let repo = open(&served, &options)?;
            checkin_blocking(&repo, &served, &email, locks, slot, &id, &request)
        },
    )
    .await;
    if result.is_ok() {
        state.pushed();
    }
    respond(result)
}

fn checkin_blocking(
    repo: &Repository,
    served: &ServedRepository,
    email: &str,
    locks: &RepoLocks,
    slot: &mut CheckoutSlot,
    id: &str,
    request: &CheckinRequest,
) -> Result<CheckinResponse, Rejection> {
    let (record, mut stored) = load_held(slot, repo, id)?;
    let before = stored.set_last.clone();
    let target = target_branch(repo);
    // Commits on the target are never dropped by a check-in: the branch the
    // checkout snapshots, and the one HEAD names now if that differs.
    let mut targets: Vec<ObjectId> = [stored.info.branch.as_str(), target.as_str()]
        .into_iter()
        .filter_map(|b| {
            grit_lib::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{b}")).ok()
        })
        .collect();
    targets.dedup();
    let who = signature(None, email)?;
    let reflog = format!("checkin: from checkout {id}");
    let mut results = Vec::new();
    for branch in &request.branches {
        let grafted = match graft_checkin_branch(repo, &stored.info, &target, branch)
            .map_err(|e| internal(&e))?
        {
            Ok(grafted) => grafted,
            Err(refusal) => {
                results.push(refused(served, id, branch, &target, refusal));
                continue;
            }
        };
        let set = {
            let _branches = locks.branches();
            set_checked_in_branch(
                repo,
                &branch.name,
                grafted.tip,
                &targets,
                &stored.info.paths,
                stored.last_set(&branch.name),
                request.force,
                &who,
                &reflog,
            )
            .map_err(|e| internal(&e))?
        };
        let set = match set {
            Ok(set) => set,
            Err(refusal) => {
                results.push(refused(served, id, branch, &target, refusal));
                continue;
            }
        };
        stored.record_set(&branch.name, grafted.tip);
        eprintln!(
            "checkin: {id} of {}: {} {:?} at {} ({} commits{})",
            served.name,
            branch.name,
            set.status,
            &grafted.tip.to_hex()[..12],
            grafted.commits.len(),
            set.rule
                .map(|rule| format!("; {}", rule_words(rule)))
                .unwrap_or_default()
        );
        results.push(CheckinResult {
            name: branch.name.clone(),
            status: set.status,
            grafted: Some(grafted.tip.to_hex()),
            previous: set.previous.map(|p| p.to_hex()),
            commits: grafted.commits.len(),
            rule: set.rule,
            message: None,
            reason: None,
        });
    }
    if stored.set_last != before {
        record.save(&stored).map_err(|e| internal(&e))?;
    }
    Ok(CheckinResponse { results })
}

/// Why an update was allowed, in words, for the server's log.
fn rule_words(rule: CheckinRule) -> &'static str {
    match rule {
        CheckinRule::FastForward => "fast-forward",
        CheckinRule::OwnLastSet => "replaces what this checkout checked in before",
        CheckinRule::KeepsEveryCommit => "rebased, and keeps every commit the server's branch had",
        CheckinRule::Forced => "forced",
    }
}

/// The result for `branch` of a check-in the server refused, with its reason
/// in words.
fn refused(
    served: &ServedRepository,
    id: &str,
    branch: &CheckinBranch,
    target: &str,
    refusal: CheckinRefusal,
) -> CheckinResult {
    eprintln!(
        "checkin: {id} of {}: {} refused ({})",
        served.name,
        branch.name,
        refusal.kind()
    );
    let name = &branch.name;
    let short = |oid: &str| oid.get(..12).unwrap_or(oid).to_owned();
    let mut previous = None;
    let message = match &refusal {
        CheckinRefusal::InvalidName => format!("{name} is not a valid branch name"),
        CheckinRefusal::TargetBranch { target } => format!(
            "{name} is the server's target branch{} and can't be changed directly; put your work on another branch, check that in, and merge it with `grit integrate <branch>`",
            if target == name { String::new() } else { format!(" ({target})") }
        ),
        CheckinRefusal::CaseConflict { existing } => format!(
            "{name} differs only in case from the server's branch {existing}, which the server's file system can't keep apart; check it in under another name"
        ),
        CheckinRefusal::InvalidTip => format!("{:?} is not an object id", branch.raw),
        CheckinRefusal::NotPushed { out_ref } => format!(
            "the server doesn't have {name} at {}; push it to {out_ref} first",
            short(&branch.raw)
        ),
        CheckinRefusal::NotOnSnapshot { commit } => format!(
            "commit {commit} of {name} does not build on this checkout's snapshots; rebase {name} onto the checkout's snapshot (`grit pull`) and check in again"
        ),
        CheckinRefusal::Conflict { commit, path, outside_paths: true } => format!(
            "commit {commit} changes {path}, which is outside this checkout's paths and which {target} holds differently; make that change from a checkout whose paths include {path}"
        ),
        CheckinRefusal::Conflict { commit, path, outside_paths: false } => format!(
            "commit {commit} changes {path}, which {target} holds differently from this checkout's snapshot (it has files there this checkout can't see); make that change from a checkout whose paths include all of {path}"
        ),
        CheckinRefusal::MergeConflict { commit, paths } => format!(
            "merge commit {commit} joins changes that conflict in {}, outside this checkout's paths; resolve them from a checkout whose paths include them, or rebase instead of merging",
            paths.join(", ")
        ),
        CheckinRefusal::Diverged { tip } => {
            previous = Some(tip.clone());
            format!(
                "the server's {name} is at {}, which has commits this checkout doesn't; rebase onto it with `grit pull {name}` and check in again, or check in with --force to replace it",
                short(tip)
            )
        }
    };
    CheckinResult {
        name: name.clone(),
        status: CheckinStatus::Refused,
        grafted: None,
        previous,
        commits: 0,
        rule: None,
        message: Some(message),
        reason: Some(refusal),
    }
}

/// `POST /.grit/checkouts/<repo>/integrate`: merge a branch into the target.
pub(crate) async fn integrate_branch(
    State(state): State<Arc<ServerState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Json(request): Json<IntegrateRequest>,
) -> Response<Body> {
    let result = with_target(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, email, options, slot| {
            let repo = open(&served, &options)?;
            let target_name = target_branch(&repo);
            let who = signature(request.author.as_ref(), &email)?;
            let done = integrate(
                &repo,
                &format!("refs/heads/{target_name}"),
                &request.name,
                &who,
            )
            .map_err(|e| internal(&e))?
            .map_err(|refusal| {
                eprintln!(
                    "integrate: {} into {target_name} of {} refused: {refusal}",
                    request.name, served.name
                );
                target_refused(&refusal)
            })?;
            let change = done.change;
            eprintln!(
                "integrate: {} into {target_name} of {}: {:?} {} → {}{}",
                request.name,
                served.name,
                done.status,
                &change.previous.to_hex()[..12],
                &change.head.to_hex()[..12],
                target_change_note(slot, &change)
            );
            Ok(IntegrateResponse {
                status: done.status,
                name: request.name.clone(),
                target: target_name,
                previous: change.previous.to_hex(),
                head: change.head.to_hex(),
                commits: done.commits,
            })
        },
    )
    .await;
    if result.is_ok() {
        state.pushed();
    }
    respond(result)
}

/// `GET /.grit/checkouts/<repo>/slices`.
pub(crate) async fn list_slices(
    State(state): State<Arc<ServerState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    respond(
        with_repo(&state, &headers, &name, |served, options| {
            let repo = open(&served, &options)?;
            let (target_name, head) = target(&repo)?;
            let slices = read_slices(&repo, head).map_err(|e| internal(&e))?;
            Ok(SlicesResponse {
                target: target_name,
                head: head.to_hex(),
                slices,
            })
        })
        .await,
    )
}

/// `PUT /.grit/checkouts/<repo>/slices/<slice>`: make or change a slice.
pub(crate) async fn put_slice(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, slice)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<SliceRequest>,
) -> Response<Body> {
    let result = with_target(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, email, options, slot| {
            let repo = open(&served, &options)?;
            let paths = normalize_paths(&request.paths)
                .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
            let slice = Slice {
                name: slice,
                description: request.description.clone(),
                paths,
            };
            let edit = if request.create {
                SliceEdit::Create(slice)
            } else {
                SliceEdit::Change(slice)
            };
            let who = signature(request.author.as_ref(), &email)?;
            change_slices(&repo, &served, slot, &edit, &who)
        },
    )
    .await;
    if result.is_ok() {
        state.pushed();
    }
    respond(result)
}

/// `DELETE /.grit/checkouts/<repo>/slices/<slice>`.
pub(crate) async fn delete_slice(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, slice)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let request: SliceDeleteRequest = serde_json::from_slice(&body).unwrap_or_default();
    let result = with_target(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, email, options, slot| {
            let repo = open(&served, &options)?;
            let who = signature(request.author.as_ref(), &email)?;
            change_slices(&repo, &served, slot, &SliceEdit::Delete(slice), &who)
        },
    )
    .await;
    if result.is_ok() {
        state.pushed();
    }
    respond(result)
}

/// Make the slice change `edit` on the target, while holding it.
fn change_slices(
    repo: &Repository,
    served: &ServedRepository,
    slot: &mut TargetSlot,
    edit: &SliceEdit,
    who: &Signature,
) -> Result<SliceChange, Rejection> {
    let target_name = target_branch(repo);
    let SlicesEdited {
        slice,
        message,
        change,
    } = edit_slices(repo, &format!("refs/heads/{target_name}"), edit, who)
        .map_err(|e| internal(&e))?
        .map_err(|refusal| target_refused(&refusal))?;
    if change.head != change.previous {
        eprintln!(
            "slice: {message} on {target_name} of {}: {} → {}{}",
            served.name,
            &change.previous.to_hex()[..12],
            &change.head.to_hex()[..12],
            target_change_note(slot, &change)
        );
    }
    Ok(SliceChange {
        slice,
        target: target_name,
        previous: change.previous.to_hex(),
        head: change.head.to_hex(),
    })
}
