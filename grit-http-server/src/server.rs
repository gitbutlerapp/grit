//! The multi-repository server behind `grit serve`.
//!
//! [`Server::bind`] opens the [`AuthStore`] and finds the repositories to
//! serve; [`Server::run`] answers requests until the process stops.
//!
//! Git requests carry an access token from the store, either as the password
//! of HTTP basic auth (what Git sends from a credential helper or a
//! `https://user:token@host/…` URL) or as `Authorization: Bearer <token>`.
//! Git requests without a valid token get `401` and a basic-auth challenge.
//! People sign in to the web pages ([`crate::web`]) with their email address
//! and password instead, and get tokens by approving a `grit auth <url>`
//! sign-in there ([`crate::login`]).
//!
//! Routes, where `<repo>` is a repository name with or without `.git`:
//!
//! | Request | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /.grit/user` | a token | JSON [`ServerIdentity`] of the token's user |
//! | `POST /.grit/login` | nothing | JSON [`LoginStart`]: begin a command-line sign-in |
//! | `POST /.grit/login/poll` | the device code | JSON [`LoginPoll`]: the token, once approved |
//! | `GET /<repo>/info/refs?service=git-upload-pack` | read | fetch discovery (v0, v1, v2) |
//! | `POST /<repo>/git-upload-pack` | read | fetch |
//! | `GET /<repo>/info/refs?service=git-receive-pack` | write | push discovery |
//! | `POST /<repo>/git-receive-pack` | write | push |
//! | `GET /<repo>/bundles/<file>` | read | a clone bundle, with `Range` ([`crate::bundles`]) |
//! | `/<repo>/push…` (TUS) | write | resumable push ([`crate::uploads`]) |
//! | `/.grit/checkouts/<repo>…` | read | snapshot checkouts ([`crate::checkouts`]) |
//! | `/`, `/<repo>`, `/.grit/…` pages | a session | see [`crate::web`] |
//!
//! Repositories the user cannot read answer `404`, the same as missing ones.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::time::Instant;

use axum::{
    body::Body,
    extract::{Path as UrlPath, Query, State},
    http::{header, HeaderMap, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use grit_lib::remote::grit_server_auth::{
    LoginGrant, LoginPoll, LoginPollRequest, LoginStart, ServerIdentity, AUTHORIZE_PATH,
    IDENTITY_PATH, LOGIN_PATH, LOGIN_POLL_PATH,
};
use grit_protocol::RepositoryOptions;

use crate::bundles::{BundleScheduler, BundleSettings};
use crate::checkout_locks::CheckoutLocks;
use crate::login::{LoginError, PendingLogins, PollOutcome};
use crate::smart_http::{self, Service};
use crate::store::{
    normalize_repository_name, Access, AuthStore, HashedPassword, StoreError, Token, User,
};
use crate::uploads::{UploadLocks, UploadPath};
use crate::web;

/// File name of the database [`default_database_path`] picks.
pub const DATABASE_FILE_NAME: &str = "grit-serve.db";

/// Errors from starting or running the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Binding the address or serving connections failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Opening the user database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The database has no users yet, so nobody could sign in.
    #[error("the server has no users yet")]
    NotSetUp,
}

/// A repository the server serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedRepository {
    /// Name in URLs and grants: the directory name without a `.git` suffix.
    pub name: String,
    /// The repository directory (bare repository or working tree).
    pub path: PathBuf,
}

/// The repositories under a server root.
#[derive(Debug, Clone)]
pub enum Repositories {
    /// The root is itself a repository; it is the only one served.
    Single(ServedRepository),
    /// The root is a directory whose child directories are repositories.
    /// Children are looked up on each request, so new ones appear without a restart.
    Directory(PathBuf),
}

impl Repositories {
    /// Decide how to serve `root`: as one repository, or as a directory of them.
    #[must_use]
    pub fn at(root: &Path) -> Self {
        if grit_protocol::validate_repo_path(root).is_ok() {
            let name = root
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| normalize_repository_name(n).ok())
                .unwrap_or("repository")
                .to_owned();
            return Self::Single(ServedRepository {
                name,
                path: root.to_path_buf(),
            });
        }
        Self::Directory(root.to_path_buf())
    }

    /// Find the repository called `name` (`.git` suffix optional).
    #[must_use]
    pub fn find(&self, name: &str) -> Option<ServedRepository> {
        let name = normalize_repository_name(name).ok()?;
        match self {
            Self::Single(repo) => (repo.name == name).then(|| repo.clone()),
            Self::Directory(root) => [name.to_owned(), format!("{name}.git")]
                .into_iter()
                .map(|dir| root.join(dir))
                .find(|path| grit_protocol::validate_repo_path(path).is_ok())
                .map(|path| ServedRepository {
                    name: name.to_owned(),
                    path,
                }),
        }
    }

    /// Every served repository, sorted by name.
    #[must_use]
    pub fn list(&self) -> Vec<ServedRepository> {
        let root = match self {
            Self::Single(repo) => return vec![repo.clone()],
            Self::Directory(root) => root,
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        let mut repos: Vec<_> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let name = normalize_repository_name(file_name.to_str()?).ok()?;
                let path = entry.path();
                grit_protocol::validate_repo_path(&path).ok()?;
                Some(ServedRepository {
                    name: name.to_owned(),
                    path,
                })
            })
            .collect();
        repos.sort_by(|a, b| a.name.cmp(&b.name));
        repos.dedup_by(|a, b| a.name == b.name);
        repos
    }
}

/// Where the user database lives when none is given: inside the Git directory
/// when `root` is a repository (so it never shows up as an untracked file),
/// otherwise in `root` itself.
#[must_use]
pub fn default_database_path(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.join("HEAD").is_file() {
        return dot_git.join(DATABASE_FILE_NAME);
    }
    root.join(DATABASE_FILE_NAME)
}

/// What [`Server::bind`] needs.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// A repository, or a directory of repositories.
    pub root: PathBuf,
    /// The user database (see [`default_database_path`]).
    pub database: PathBuf,
    /// Environment used to load global and system config for each repository.
    pub repository_options: RepositoryOptions,
    /// When clone bundles are built ([`crate::bundles`]).
    pub bundles: BundleSettings,
}

pub(crate) struct ServerState {
    pub(crate) repositories: Repositories,
    store: Mutex<AuthStore>,
    logins: Mutex<PendingLogins>,
    pub(crate) options: RepositoryOptions,
    /// Checked when a sign-in names no user, so it takes as long as a real one.
    pub(crate) stand_in_password: HashedPassword,
    bundles: BundleSettings,
    /// Running once [`Server::run`] started it.
    scheduler: Mutex<Option<BundleScheduler>>,
    uploads: UploadLocks,
    /// What snapshot-checkout requests take turns on.
    pub(crate) checkout_locks: CheckoutLocks,
}

/// A bound, ready-to-run server.
pub struct Server {
    listener: std::net::TcpListener,
    state: Arc<ServerState>,
}

impl Server {
    /// Open the database, find the repositories and bind `address`.
    ///
    /// # Parameters
    ///
    /// - `config`: what to serve and where the users are.
    /// - `address`: `host:port` to listen on; port `0` picks a free port.
    ///
    /// # Errors
    ///
    /// [`ServerError::Store`] when the database cannot be opened,
    /// [`ServerError::NotSetUp`] when it has no users, and
    /// [`ServerError::Io`] when the address cannot be bound.
    pub fn bind(config: ServeConfig, address: &str) -> Result<Self, ServerError> {
        let store = AuthStore::open(&config.database)?;
        if !store.has_users()? {
            return Err(ServerError::NotSetUp);
        }
        let listener = std::net::TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let stand_in_password = HashedPassword::new(Token::generate()?.secret())?;
        Ok(Self {
            listener,
            state: Arc::new(ServerState {
                repositories: Repositories::at(&config.root),
                store: Mutex::new(store),
                logins: Mutex::new(PendingLogins::default()),
                options: config.repository_options,
                stand_in_password,
                bundles: config.bundles,
                scheduler: Mutex::new(None),
                uploads: UploadLocks::default(),
                checkout_locks: CheckoutLocks::default(),
            }),
        })
    }

    /// The address the server listens on.
    ///
    /// # Errors
    ///
    /// The operating system error when the socket address is unavailable.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The repositories served right now.
    #[must_use]
    pub fn repositories(&self) -> Vec<ServedRepository> {
        self.state.repositories.list()
    }

    /// Serve requests until the process stops. Blocks the calling thread.
    ///
    /// # Errors
    ///
    /// [`ServerError::Io`] when the async runtime cannot start or the
    /// listener fails.
    pub fn run(self) -> Result<(), ServerError> {
        self.start_bundles();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(self.listener)?;
            axum::serve(listener, router(self.state)).await
        })?;
        Ok(())
    }
}

impl Server {
    /// Start the thread that builds clone bundles, unless bundles are off.
    fn start_bundles(&self) {
        if !self.state.bundles.enabled {
            return;
        }
        let repositories = self.state.repositories.clone();
        let scheduler = BundleScheduler::start(
            self.state.bundles,
            self.state.options.clone(),
            move || repositories.list().into_iter().map(|r| r.path).collect(),
            |path, manifest| {
                eprintln!(
                    "bundle: built {} ({} bytes) for {}",
                    manifest.file,
                    manifest.size,
                    path.display()
                );
            },
            |path, e| eprintln!("bundle: cannot bundle {}: {e}", path.display()),
        );
        *self
            .state
            .scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(scheduler);
    }
}

fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(web::index))
        .route(&format!("/{IDENTITY_PATH}"), get(identity))
        .route(&format!("/{LOGIN_PATH}"), post(login_start))
        .route(&format!("/{LOGIN_POLL_PATH}"), post(login_poll))
        .route(
            &format!("/{AUTHORIZE_PATH}"),
            get(web::authorize_page).post(web::authorize_submit),
        )
        .route(
            web::SIGNIN_PATH,
            get(web::signin_page).post(web::signin_submit),
        )
        .route(web::SIGNOUT_PATH, post(web::signout))
        .route(
            web::PASSWORD_PATH,
            get(web::password_page).post(web::password_submit),
        )
        .route(web::USERS_PATH, get(web::users_page).post(web::add_user))
        .route(web::GRANTS_PATH, post(web::grant))
        .route(
            "/.grit/checkouts/{repo}",
            get(crate::checkouts::server_info).post(crate::checkouts::create),
        )
        .route("/.grit/checkouts/{repo}/{id}", get(crate::checkouts::show))
        .route(
            "/.grit/checkouts/{repo}/{id}/checkin",
            post(crate::checkouts::checkin),
        )
        .route(
            "/.grit/checkouts/{repo}/{id}/update",
            post(crate::checkouts::update),
        )
        .route(
            "/.grit/checkouts/{repo}/integrate",
            post(crate::checkouts::integrate_branch),
        )
        .route(
            "/.grit/checkouts/{repo}/slices",
            get(crate::checkouts::list_slices),
        )
        .route(
            "/.grit/checkouts/{repo}/slices/{slice}",
            axum::routing::put(crate::checkouts::put_slice).delete(crate::checkouts::delete_slice),
        )
        .route(
            "/.grit/checkouts/{repo}/{id}/bundle",
            get(crate::checkouts::bundle),
        )
        .route(
            "/{*path}",
            get(get_dispatch)
                .post(post_dispatch)
                .head(head_dispatch)
                .patch(patch_dispatch)
                .options(options_dispatch),
        )
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

fn unauthorized() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"grit\"")
        .body(Body::from("an access token is required\n"))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

fn not_found() -> Response<Body> {
    (StatusCode::NOT_FOUND, "repository not found\n").into_response()
}

pub(crate) fn store_failure(e: &StoreError) -> Response<Body> {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("user database error: {e}\n"),
    )
        .into_response()
}

/// The token a request carries: the basic-auth password (or the user name
/// when the password is empty), or a bearer token.
pub(crate) fn presented_token(headers: &HeaderMap) -> Option<Token> {
    use base64::Engine as _;

    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, credentials) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(Token::from_secret(credentials.trim()));
    }
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(credentials.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, password) = decoded.split_once(':').unwrap_or((&decoded, ""));
    let secret = if password.is_empty() { user } else { password };
    (!secret.is_empty()).then(|| Token::from_secret(secret))
}

impl ServerState {
    pub(crate) fn lock_store(&self) -> std::sync::MutexGuard<'_, AuthStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn lock_logins(&self) -> std::sync::MutexGuard<'_, PendingLogins> {
        self.logins.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Check bundles again soon, after a push changed a repository.
    pub(crate) fn pushed(&self) {
        if let Some(scheduler) = self
            .scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            scheduler.wake();
        }
    }

    /// What `repo`'s advertisements offer beyond fetch and push: its clone
    /// bundle for upload-pack, the resumable push endpoint for receive-pack.
    fn extensions(
        &self,
        repo: &ServedRepository,
        service: Service,
        headers: &HeaderMap,
    ) -> grit_protocol::Extensions {
        match service {
            Service::ReceivePack => grit_protocol::Extensions {
                push_bundle: Some(crate::uploads::PUSH_BUNDLE_PATH.to_owned()),
                ..Default::default()
            },
            Service::UploadPack => {
                let list = self
                    .bundles
                    .enabled
                    .then(|| crate::bundles::current(&repo.path))
                    .flatten()
                    .zip(request_base_url(headers))
                    .map(|(manifest, base)| {
                        crate::bundles::bundle_list(&manifest, &base, &repo.name)
                    });
                grit_protocol::Extensions {
                    bundle_list: list,
                    ..Default::default()
                }
            }
        }
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<User, Rejection> {
        let token = presented_token(headers).ok_or_else(|| Box::new(unauthorized()))?;
        match self.lock_store().authenticate(&token) {
            Ok(Some(user)) => Ok(user),
            Ok(None) => Err(Box::new(unauthorized())),
            Err(e) => Err(Box::new(store_failure(&e))),
        }
    }

    /// Resolve `name` for a Git request that needs `needed` access.
    pub(crate) fn authorize(
        &self,
        headers: &HeaderMap,
        name: &str,
        needed: Access,
    ) -> Result<Authorized, Rejection> {
        self.authorize_user(self.authenticate(headers)?, name, needed)
    }

    /// Resolve `name` for `user`, who needs `needed` access.
    pub(crate) fn authorize_user(
        &self,
        user: User,
        name: &str,
        needed: Access,
    ) -> Result<Authorized, Rejection> {
        let name = normalize_repository_name(name).map_err(|_| Box::new(not_found()))?;
        let access = match self.lock_store().access(&user, name) {
            Ok(Some(access)) => access,
            Ok(None) => return Err(Box::new(not_found())),
            Err(e) => return Err(Box::new(store_failure(&e))),
        };
        let repo = self
            .repositories
            .find(name)
            .ok_or_else(|| Box::new(not_found()))?;
        if access < needed {
            return Err(Box::new(
                (
                    StatusCode::FORBIDDEN,
                    format!("{} has read-only access to {}\n", user.email, repo.name),
                )
                    .into_response(),
            ));
        }
        Ok(Authorized {
            repo,
            access,
            user_id: user.id,
            email: user.email,
        })
    }
}

/// An error response for a request that may not proceed (boxed: responses are large).
type Rejection = Box<Response<Body>>;

/// A request [`ServerState::authorize`] let through.
pub(crate) struct Authorized {
    pub(crate) repo: ServedRepository,
    pub(crate) access: Access,
    pub(crate) user_id: i64,
    /// The signed-in user's email address.
    pub(crate) email: String,
}

/// `scheme://host` the client used to reach the server, from `Host` (or the
/// `X-Forwarded-*` headers a reverse proxy adds).
fn request_base_url(headers: &HeaderMap) -> Option<String> {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let host = value("x-forwarded-host").or_else(|| value("host"))?;
    if host.contains(['/', '@', ' ']) {
        return None;
    }
    let scheme = value("x-forwarded-proto")
        .filter(|p| p == "https" || p == "http")
        .unwrap_or_else(|| "http".to_owned());
    Some(format!("{scheme}://{host}"))
}

fn needed_access(service: Service) -> Access {
    match service {
        Service::UploadPack => Access::Read,
        Service::ReceivePack => Access::Write,
    }
}

async fn get_dispatch(
    State(state): State<Arc<ServerState>>,
    UrlPath(path): UrlPath<String>,
    Query(query): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some((name, file)) = bundle_path(&path) {
        return bundle_file(&state, name, file, &headers).await;
    }
    let Some(name) = path.strip_suffix("/info/refs") else {
        return web::repository_page(&state, path.trim_end_matches('/'), &headers).await;
    };
    let Some(service) = query.service.as_deref().and_then(Service::from_query) else {
        return (StatusCode::FORBIDDEN, "only smart HTTP is supported\n").into_response();
    };
    let repo = match state.authorize(&headers, name, needed_access(service)) {
        Ok(found) => found.repo,
        Err(resp) => return *resp,
    };
    let protocol_version = smart_http::protocol_version_from_headers(&headers);
    let extensions = state.extensions(&repo, service, &headers);
    smart_http::info_refs(
        repo.path,
        service,
        protocol_version,
        state.options.clone(),
        extensions,
    )
    .await
}

/// `<repo>/bundles/<file>` split into the repository name and the file.
fn bundle_path(path: &str) -> Option<(&str, &str)> {
    let (rest, file) = path.rsplit_once('/')?;
    let name = rest.strip_suffix(&format!("/{}", crate::bundles::BUNDLES_PATH))?;
    file.ends_with(".bundle").then_some((name, file))
}

/// Serve a clone bundle (resumable with `Range`).
async fn bundle_file(
    state: &ServerState,
    name: &str,
    file: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    let repo = match state.authorize(headers, name, Access::Read) {
        Ok(found) => found.repo,
        Err(resp) => return *resp,
    };
    let Some(path) = crate::bundles::served_file(&repo.path, file) else {
        return (StatusCode::NOT_FOUND, "no such bundle\n").into_response();
    };
    smart_http::serve_file(
        path,
        &format!("\"{file}\""),
        "application/x-git-bundle",
        headers,
    )
    .await
}

/// Authorize a resumable-push request: write access to the repository.
fn upload_target(
    state: &ServerState,
    headers: &HeaderMap,
    name: &str,
) -> Result<Authorized, Rejection> {
    state.authorize(headers, name, Access::Write)
}

async fn head_dispatch(
    State(state): State<Arc<ServerState>>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let Some((name, UploadPath::Upload(id))) = crate::uploads::parse_path(&path) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    match upload_target(&state, &headers, name) {
        Ok(found) => crate::uploads::head(&found.repo.path, id, found.user_id),
        Err(resp) => *resp,
    }
}

async fn patch_dispatch(
    State(state): State<Arc<ServerState>>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let Some((name, UploadPath::Upload(id))) = crate::uploads::parse_path(&path) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    match upload_target(&state, &headers, name) {
        Ok(found) => {
            crate::uploads::patch(
                &found.repo.path,
                id,
                found.user_id,
                &state.uploads,
                &headers,
                body,
            )
            .await
        }
        Err(resp) => *resp,
    }
}

async fn options_dispatch(UrlPath(path): UrlPath<String>) -> Response<Body> {
    match crate::uploads::parse_path(&path) {
        Some((_, UploadPath::Endpoint | UploadPath::Upload(_))) => crate::uploads::options(),
        _ => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}

async fn post_dispatch(
    State(state): State<Arc<ServerState>>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    if let Some((name, upload)) = crate::uploads::parse_path(&path) {
        let found = match upload_target(&state, &headers, name) {
            Ok(found) => found,
            Err(resp) => return *resp,
        };
        return match upload {
            UploadPath::Endpoint => {
                crate::uploads::create(&found.repo.path, &found.repo.name, found.user_id, &headers)
                    .await
            }
            UploadPath::Receive(id) => {
                crate::uploads::receive(
                    found.repo.path,
                    id,
                    found.user_id,
                    &state.uploads,
                    body,
                    state.options.clone(),
                    || state.pushed(),
                )
                .await
            }
            UploadPath::Upload(_) => {
                (StatusCode::METHOD_NOT_ALLOWED, "use PATCH\n").into_response()
            }
        };
    }
    let Some((name, service)) = Service::strip_rpc_suffix(&path) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    let repo = match state.authorize(&headers, name, needed_access(service)) {
        Ok(found) => found.repo,
        Err(resp) => return *resp,
    };
    let protocol_version = smart_http::protocol_version_from_headers(&headers);
    let extensions = state.extensions(&repo, service, &headers);
    let response = smart_http::rpc(
        repo.path,
        service,
        protocol_version,
        &headers,
        body,
        state.options.clone(),
        extensions,
    )
    .await;
    if service == Service::ReceivePack && response.status().is_success() {
        state.pushed();
    }
    response
}

/// The token's user as JSON, so a client can check a token before storing it.
async fn identity(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response<Body> {
    let user = match state.authenticate(&headers) {
        Ok(user) => user,
        Err(resp) => return *resp,
    };
    Json(ServerIdentity {
        email: user.email,
        admin: user.admin,
    })
    .into_response()
}

/// Begin a command-line sign-in.
async fn login_start(State(state): State<Arc<ServerState>>) -> Response<Body> {
    let started: Result<LoginStart, LoginError> = state.lock_logins().start(Instant::now());
    match started {
        Ok(login) => Json(login).into_response(),
        Err(e @ LoginError::TooMany) => {
            (StatusCode::SERVICE_UNAVAILABLE, format!("{e}\n")).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    }
}

/// Answer a command-line poll; once the user approved, create their token.
async fn login_poll(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<LoginPollRequest>,
) -> Response<Body> {
    let outcome = state
        .lock_logins()
        .poll(&request.device_code, Instant::now());
    if let Some(answer) = outcome.without_grant() {
        return Json(answer).into_response();
    }
    let PollOutcome::Approved(user) = outcome else {
        return Json(LoginPoll::Expired).into_response();
    };
    let token = match Token::generate() {
        Ok(token) => token,
        Err(e) => return store_failure(&e),
    };
    if let Err(e) = state.lock_store().add_token(&user, &token) {
        return store_failure(&e);
    }
    Json(LoginPoll::Authorized(LoginGrant {
        email: user.email,
        token: token.secret().to_owned(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn tokens_come_from_basic_or_bearer_auth() {
        use base64::Engine as _;
        let encode = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let token_of = |value: String| {
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.insert(header::AUTHORIZATION, value);
            }
            presented_token(&headers).map(|t| t.secret().to_owned())
        };
        assert_eq!(
            token_of(format!("Basic {}", encode("bob:grit_secret"))).as_deref(),
            Some("grit_secret")
        );
        assert_eq!(
            token_of(format!("Basic {}", encode("grit_secret:"))).as_deref(),
            Some("grit_secret")
        );
        assert_eq!(
            token_of("Bearer grit_secret".to_owned()).as_deref(),
            Some("grit_secret")
        );
        assert_eq!(token_of(format!("Basic {}", encode(":"))), None);
        assert_eq!(token_of("Digest abc".to_owned()), None);
        assert!(presented_token(&HeaderMap::new()).is_none());
    }

    #[test]
    fn database_defaults_inside_a_working_tree_git_dir() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        };
        assert_eq!(
            default_database_path(dir.path()),
            dir.path().join(DATABASE_FILE_NAME)
        );
        let git_dir = dir.path().join(".git");
        assert!(std::fs::create_dir(&git_dir).is_ok());
        assert!(std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").is_ok());
        assert_eq!(
            default_database_path(dir.path()),
            git_dir.join(DATABASE_FILE_NAME)
        );
    }

    #[test]
    fn directory_roots_find_children_with_or_without_dot_git() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        };
        for repo in ["alpha.git", "beta"] {
            let path = dir.path().join(repo);
            assert!(std::fs::create_dir(&path).is_ok());
            assert!(std::fs::write(path.join("HEAD"), "ref: refs/heads/main\n").is_ok());
        }
        assert!(std::fs::create_dir(dir.path().join("not-a-repo")).is_ok());

        let repos = Repositories::at(dir.path());
        let names: Vec<_> = repos.list().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(
            repos.find("alpha").map(|r| r.path),
            Some(dir.path().join("alpha.git"))
        );
        assert!(repos.find("beta.git").is_some());
        assert!(repos.find("not-a-repo").is_none());
        assert!(repos.find("../alpha").is_none());

        let single = Repositories::at(&dir.path().join("alpha.git"));
        assert!(matches!(&single, Repositories::Single(r) if r.name == "alpha"));
        assert!(single.find("alpha.git").is_some());
        assert!(single.find("beta").is_none());
    }
}
