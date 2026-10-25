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
    make_snapshot, normalize_paths, CheckoutError, CheckoutErrorBody, CheckoutInfo,
    CheckoutRequest, CheckoutServerInfo, SnapshotRoot,
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
    grit_lib::refs::write_ref(
        repo.git_dir(),
        &format!("refs/checkouts/{id}/target"),
        &snapshot.commit,
    )
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
