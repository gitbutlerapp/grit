//! Snapshot checkouts for `grit serve` ([`grit_lib::remote::snapshot_checkout`]).
//!
//! These live at their own endpoint, apart from the Git smart-HTTP paths, so
//! `grit checkout` can tell a server that offers them from one that doesn't:
//!
//! | Request | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /.grit/checkouts/<repo>` | read | JSON [`CheckoutServerInfo`] |
//! | `POST /.grit/checkouts/<repo>` | read | make a checkout: JSON [`CheckoutInfo`] |
//! | `GET /.grit/checkouts/<repo>/<id>` | read | JSON [`CheckoutInfo`] |
//! | `GET /.grit/checkouts/<repo>/<id>/bundle` | read | the snapshot as a bundle, with `Range` |
//!
//! A checkout is a snapshot of the repository's target branch (where `HEAD`
//! points) cut down to the paths asked for. Snapshots are deterministic, so
//! checkouts of the same paths at the same commit share one, and its bundle is
//! built once. The server keeps each checkout in
//! `<git-dir>/grit-serve/checkouts/<id>.json` and points
//! `refs/checkouts/<id>/target` at its snapshot, which keeps the snapshot's
//! objects alive.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use grit_lib::bundle::BundleSpec;
use grit_lib::objects::ObjectId;
use grit_lib::remote::snapshot_checkout::{
    commit_slices, decide_checkin, graft, integrate, make_snapshot, missing_paths, normalize_paths,
    out_ref, read_slices, target_ref, valid_branch_name, valid_slice_name, CheckinDecision,
    CheckinRequest, CheckinResponse, CheckinResult, CheckinStatus, CheckoutError,
    CheckoutErrorBody, CheckoutInfo, CheckoutRequest, CheckoutServerInfo, Ident, IntegrateRequest,
    IntegrateResponse, IntegrateStatus, Integrated, Signature, Slice, SliceChange,
    SliceDeleteRequest, SliceRequest, SlicesResponse, SnapshotRoot,
};
use grit_lib::repo::Repository;
use grit_protocol::RepositoryOptions;

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

fn state_dir(repo: &Repository) -> PathBuf {
    repo.git_dir().join("grit-serve").join("checkouts")
}

/// Ids are 20 lowercase hex digits the server made.
fn valid_id(id: &str) -> bool {
    id.len() == 20
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn open(repo: &ServedRepository, options: &RepositoryOptions) -> Result<Repository, Rejection> {
    Repository::open(&repo.path, &options.clone().for_serving())
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// The target branch's short name and tip.
fn target(repo: &Repository) -> Result<(String, ObjectId), Rejection> {
    let head = grit_lib::refs::read_symbolic_ref(repo.git_dir(), "HEAD")
        .ok()
        .flatten()
        .unwrap_or_else(|| "refs/heads/main".to_owned());
    let branch = head.strip_prefix("refs/heads/").unwrap_or(&head).to_owned();
    match grit_lib::refs::resolve_ref(repo.git_dir(), &head) {
        Ok(tip) => Ok((branch, tip)),
        Err(_) => Err(error(
            StatusCode::CONFLICT,
            format!("{branch} has no commits yet, so there is nothing to check out"),
        )),
    }
}

/// Run `f` for a request on `name` that needs `needed` access, with the
/// signed-in user's email.
async fn with_access<T: Send + 'static>(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    name: &str,
    needed: Access,
    f: impl FnOnce(ServedRepository, String, RepositoryOptions) -> Result<T, Rejection> + Send + 'static,
) -> Result<T, Rejection> {
    let found = state.authorize(headers, name, needed)?;
    let options = state.options.clone();
    tokio::task::spawn_blocking(move || f(found.repo, found.email, options))
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
    let result = with_repo(&state, &headers, &name, move |served, options| {
        let repo = open(&served, &options)?;
        create_blocking(&repo, &served, &request)
    })
    .await;
    match result {
        Ok(info) => Json(info).into_response(),
        Err(resp) => *resp,
    }
}

fn create_blocking(
    repo: &Repository,
    served: &ServedRepository,
    request: &CheckoutRequest,
) -> Result<CheckoutInfo, Rejection> {
    let internal =
        |e: &dyn std::fmt::Display| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let paths = normalize_paths(&request.paths)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
    let (branch, head) = target(repo)?;
    let snapshot = make_snapshot(repo, head, &paths, None).map_err(|e| match e {
        CheckoutError::MissingPath { .. } | CheckoutError::InvalidPath { .. } => {
            error(StatusCode::BAD_REQUEST, e.to_string())
        }
        other => internal(&other),
    })?;
    let dir = state_dir(repo);
    std::fs::create_dir_all(dir.join("bundles")).map_err(|e| internal(&e))?;
    let bundle = bundle_path(repo, &snapshot.commit.to_hex());
    if !bundle.is_file() {
        build_bundle(repo, snapshot.commit, &branch, &bundle).map_err(|e| internal(&e))?;
    }
    let bundle_bytes = std::fs::metadata(&bundle).map(|m| m.len()).unwrap_or(0);
    let mut random = [0u8; 10];
    getrandom::getrandom(&mut random).map_err(|e| internal(&e))?;
    let id = hex::encode(random);
    grit_lib::refs::write_ref(repo.git_dir(), &target_ref(&id), &snapshot.commit)
        .map_err(|e| internal(&e))?;
    let work = request.work.as_deref().map(|w| free_branch(repo, w));
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
    let bytes = serde_json::to_vec_pretty(&info).map_err(|e| internal(&e))?;
    std::fs::write(dir.join(format!("{id}.json")), bytes).map_err(|e| internal(&e))?;
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

fn bundle_path(repo: &Repository, commit: &str) -> PathBuf {
    state_dir(repo)
        .join("bundles")
        .join(format!("{commit}.bundle"))
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

/// `name`, or the first `name-2`, `name-3`, … the repository has no branch by.
fn free_branch(repo: &Repository, name: &str) -> String {
    let taken =
        |n: &str| grit_lib::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{n}")).is_ok();
    if !taken(name) {
        return name.to_owned();
    }
    (2..)
        .map(|i| format!("{name}-{i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| name.to_owned())
}

fn load(repo: &Repository, id: &str) -> Result<CheckoutInfo, Rejection> {
    let missing = || error(StatusCode::NOT_FOUND, format!("no checkout {id}"));
    if !valid_id(id) {
        return Err(missing());
    }
    let bytes = std::fs::read(state_dir(repo).join(format!("{id}.json"))).map_err(|_| missing())?;
    serde_json::from_slice(&bytes).map_err(|_| missing())
}

/// `GET /.grit/checkouts/<repo>/<id>`.
pub(crate) async fn show(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let result = with_repo(&state, &headers, &name, move |served, options| {
        load(&open(&served, &options)?, &id)
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
        let info = load(&repo, &id)?;
        let path = bundle_path(&repo, &info.root);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_ids_are_server_made_hex() {
        assert!(valid_id("0123456789abcdef0123"));
        assert!(!valid_id("0123456789ABCDEF0123"));
        assert!(!valid_id("../../etc/passwd...."));
        assert!(!valid_id("abc"));
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

fn save(repo: &Repository, info: &CheckoutInfo) -> Result<(), Rejection> {
    let bytes = serde_json::to_vec_pretty(info).map_err(|e| internal(&e))?;
    let path = state_dir(repo).join(format!("{}.json", info.id));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| internal(&e))?;
    std::fs::rename(&tmp, &path).map_err(|e| internal(&e))
}

/// The branches each checkout set last, so it may replace its own work.
type LastSet = std::collections::BTreeMap<String, String>;

fn last_set_path(repo: &Repository, id: &str) -> PathBuf {
    state_dir(repo).join(format!("{id}.branches.json"))
}

fn read_last_set(repo: &Repository, id: &str) -> LastSet {
    std::fs::read(last_set_path(repo, id))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The commit signature for a server-made commit: the request's author, or
/// the signed-in user.
fn signature(author: Option<&Ident>, email: &str) -> Signature {
    let ident = match author {
        Some(a) => format!("{} <{}>", a.name, a.email),
        None => format!("{email} <{email}>"),
    };
    Signature {
        ident,
        when: time::OffsetDateTime::now_utc().unix_timestamp(),
    }
}

/// `POST /.grit/checkouts/<repo>/<id>/update`: a snapshot of the newest target
/// on top of the checkout's last one.
pub(crate) async fn update(
    State(state): State<Arc<ServerState>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    respond(
        with_repo(&state, &headers, &name, move |served, options| {
            let repo = open(&served, &options)?;
            let mut info = load(&repo, &id)?;
            let (_, head) = target(&repo)?;
            if info.base == head.to_hex() {
                return Ok(info);
            }
            let parent = ObjectId::from_hex(&info.root).map_err(|e| internal(&e))?;
            let snapshot =
                make_snapshot(&repo, head, &info.paths, Some(parent)).map_err(|e| match e {
                    CheckoutError::MissingPath { .. } => error(
                        StatusCode::CONFLICT,
                        format!("{e}; the checkout's paths are gone from the target"),
                    ),
                    other => internal(&other),
                })?;
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
            save(&repo, &info)?;
            eprintln!(
                "checkout: {id} of {} refreshed to {} → {}",
                served.name,
                &info.base[..12],
                &info.root[..12]
            );
            Ok(info)
        })
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
    let result = with_access(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, _, options| {
            let repo = open(&served, &options)?;
            checkin_blocking(&repo, &served, &id, &request)
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
    id: &str,
    request: &CheckinRequest,
) -> Result<CheckinResponse, Rejection> {
    let info = load(repo, id)?;
    let mut last = read_last_set(repo, id);
    let mut results = Vec::new();
    for branch in &request.branches {
        let refused = |message: String| CheckinResult {
            name: branch.name.clone(),
            status: CheckinStatus::Refused,
            grafted: None,
            previous: None,
            commits: 0,
            message: Some(message),
        };
        let refname = format!("refs/heads/{}", branch.name);
        if !valid_branch_name(&branch.name) {
            results.push(refused(format!(
                "{} is not a valid branch name",
                branch.name
            )));
            continue;
        }
        if branch.name == info.branch {
            results.push(refused(format!(
                "{} is the server's target branch and can't be changed directly; put your work on another branch, check that in, and merge it with `grit integrate <branch>`",
                info.branch
            )));
            continue;
        }
        let pushed = grit_lib::refs::resolve_ref(repo.git_dir(), &out_ref(id, &branch.name)).ok();
        let Some(raw) = pushed.filter(|p| p.to_hex() == branch.raw) else {
            results.push(refused(format!(
                "the server doesn't have {} at {}; push it to {} first",
                branch.name,
                &branch.raw[..branch.raw.len().min(12)],
                out_ref(id, &branch.name)
            )));
            continue;
        };
        let grafted = match graft(repo, &info.roots, raw).map_err(|e| internal(&e))? {
            Ok(g) => g,
            Err(e) => {
                results.push(refused(e.to_string()));
                continue;
            }
        };
        let previous = grit_lib::refs::resolve_ref(repo.git_dir(), &refname).ok();
        let set_last = last
            .get(&branch.name)
            .and_then(|s| ObjectId::from_hex(s).ok());
        let decision = decide_checkin(repo, previous, grafted.tip, set_last, request.force)
            .map_err(|e| internal(&e))?;
        let status = match decision {
            CheckinDecision::Unchanged => CheckinStatus::Unchanged,
            CheckinDecision::Diverged => {
                results.push(CheckinResult {
                    previous: previous.map(|p| p.to_hex()),
                    ..refused(format!(
                        "the server's {} has commits yours doesn't; run `grit pull` to rebase onto them, or check in with --force to replace it",
                        branch.name
                    ))
                });
                continue;
            }
            CheckinDecision::Create | CheckinDecision::Update => {
                let written = match previous {
                    Some(p) => {
                        grit_lib::refs::write_ref_cas(repo.git_dir(), &refname, &grafted.tip, p)
                    }
                    None => grit_lib::refs::write_ref(repo.git_dir(), &refname, &grafted.tip),
                };
                if let Err(e) = written {
                    results.push(refused(format!(
                        "{} moved while it was being set: {e}",
                        branch.name
                    )));
                    continue;
                }
                last.insert(branch.name.clone(), grafted.tip.to_hex());
                if previous.is_some() {
                    CheckinStatus::Updated
                } else {
                    CheckinStatus::Created
                }
            }
        };
        eprintln!(
            "checkin: {id} of {}: {} {:?} at {} ({} commits)",
            served.name,
            branch.name,
            status,
            &grafted.tip.to_hex()[..12],
            grafted.commits.len()
        );
        results.push(CheckinResult {
            name: branch.name.clone(),
            status,
            grafted: Some(grafted.tip.to_hex()),
            previous: previous.map(|p| p.to_hex()),
            commits: grafted.commits.len(),
            message: None,
        });
    }
    let bytes = serde_json::to_vec_pretty(&last).map_err(|e| internal(&e))?;
    std::fs::write(last_set_path(repo, id), bytes).map_err(|e| internal(&e))?;
    Ok(CheckinResponse { results })
}

/// `POST /.grit/checkouts/<repo>/integrate`: merge a branch into the target.
pub(crate) async fn integrate_branch(
    State(state): State<Arc<ServerState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Json(request): Json<IntegrateRequest>,
) -> Response<Body> {
    let result = with_access(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, email, options| {
            let repo = open(&served, &options)?;
            let (target_name, previous) = target(&repo)?;
            let refname = format!("refs/heads/{}", request.name);
            let branch = grit_lib::refs::resolve_ref(repo.git_dir(), &refname).map_err(|_| {
                error(
                    StatusCode::NOT_FOUND,
                    format!(
                        "the server has no branch {}; check it in first",
                        request.name
                    ),
                )
            })?;
            let who = signature(request.author.as_ref(), &email);
            let target_ref = format!("refs/heads/{target_name}");
            let done = integrate(&repo, &target_ref, previous, branch, &request.name, &who)
                .map_err(|e| internal(&e))?
                .map_err(|e| error(StatusCode::CONFLICT, e.to_string()))?;
            let (status, head, commits) = match done {
                Integrated::AlreadyIntegrated => (IntegrateStatus::AlreadyIntegrated, previous, 0),
                Integrated::Merged { head, commits } => (IntegrateStatus::Merged, head, commits),
            };
            eprintln!(
                "integrate: {} into {target_name} of {}: {:?} {} → {}",
                request.name,
                served.name,
                status,
                &previous.to_hex()[..12],
                &head.to_hex()[..12]
            );
            Ok(IntegrateResponse {
                status,
                name: request.name.clone(),
                target: target_name,
                previous: previous.to_hex(),
                head: head.to_hex(),
                commits,
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
    let result = with_access(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, email, options| {
            let repo = open(&served, &options)?;
            if !valid_slice_name(&slice) {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "{slice} is not a valid slice name: use letters, digits, `-`, `_` and `.`"
                    ),
                ));
            }
            let paths = normalize_paths(&request.paths)
                .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
            if paths.is_empty() {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "a slice needs at least one path",
                ));
            }
            let (target_name, previous) = target(&repo)?;
            let missing = missing_paths(&repo, previous, &paths).map_err(|e| internal(&e))?;
            if !missing.is_empty() {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    format!("{} not in {target_name}", missing.join(", ")),
                ));
            }
            let mut slices = read_slices(&repo, previous).map_err(|e| internal(&e))?;
            let existing = slices.iter().position(|s| s.name == slice);
            let new = Slice {
                name: slice.clone(),
                description: request
                    .description
                    .clone()
                    .or_else(|| existing.and_then(|i| slices[i].description.clone())),
                paths,
            };
            let message = match (existing, request.create) {
                (Some(_), true) => {
                    return Err(error(
                        StatusCode::CONFLICT,
                        format!("slice {slice} already exists"),
                    ))
                }
                (None, false) => {
                    return Err(error(
                        StatusCode::NOT_FOUND,
                        format!("there is no slice {slice}"),
                    ))
                }
                (Some(i), false) => {
                    slices[i] = new.clone();
                    format!("Change slice {slice}")
                }
                (None, true) => {
                    slices.push(new.clone());
                    format!("Add slice {slice}")
                }
            };
            let who = signature(request.author.as_ref(), &email);
            let head = commit_slices(
                &repo,
                &format!("refs/heads/{target_name}"),
                previous,
                &slices,
                &who,
                &message,
            )
            .map_err(|e| internal(&e))?
            .map_err(|e| error(StatusCode::CONFLICT, e.to_string()))?;
            Ok(SliceChange {
                slice: Some(new),
                target: target_name,
                previous: previous.to_hex(),
                head: head.to_hex(),
            })
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
    let result = with_access(
        &state,
        &headers,
        &name,
        Access::Write,
        move |served, email, options| {
            let repo = open(&served, &options)?;
            let (target_name, previous) = target(&repo)?;
            let mut slices = read_slices(&repo, previous).map_err(|e| internal(&e))?;
            let Some(i) = slices.iter().position(|s| s.name == slice) else {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("there is no slice {slice}"),
                ));
            };
            slices.remove(i);
            let who = signature(request.author.as_ref(), &email);
            let head = commit_slices(
                &repo,
                &format!("refs/heads/{target_name}"),
                previous,
                &slices,
                &who,
                &format!("Remove slice {slice}"),
            )
            .map_err(|e| internal(&e))?
            .map_err(|e| error(StatusCode::CONFLICT, e.to_string()))?;
            Ok(SliceChange {
                slice: None,
                target: target_name,
                previous: previous.to_hex(),
                head: head.to_hex(),
            })
        },
    )
    .await;
    if result.is_ok() {
        state.pushed();
    }
    respond(result)
}
