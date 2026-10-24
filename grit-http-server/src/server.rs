//! The multi-repository server behind `grit serve`.
//!
//! [`Server::bind`] opens the [`AuthStore`] and finds the repositories to
//! serve; [`Server::run`] answers requests until the process stops.
//!
//! Every request must carry an access token from the store, either as the
//! password of HTTP basic auth (what Git sends from a credential helper or a
//! `https://user:token@host/…` URL) or as `Authorization: Bearer <token>`.
//! Requests without a valid token get `401` and a basic-auth challenge, so Git
//! and browsers prompt for credentials.
//!
//! Routes, where `<repo>` is a repository name with or without `.git`:
//!
//! | Request | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /` | a token | HTML list of the repositories the user can read |
//! | `GET /.grit/user` | a token | JSON [`ServerIdentity`] of the token's user (what `grit auth <url>` checks) |
//! | `GET /<repo>` | read | HTML page with the branches and recent commits |
//! | `GET /<repo>/info/refs?service=git-upload-pack` | read | fetch discovery (v0, v1, v2) |
//! | `POST /<repo>/git-upload-pack` | read | fetch |
//! | `GET /<repo>/info/refs?service=git-receive-pack` | write | push discovery |
//! | `POST /<repo>/git-receive-pack` | write | push |
//!
//! Repositories the user cannot read answer `404`, the same as missing ones.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{Path as UrlPath, Query, State},
    http::{header, HeaderMap, Response, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::porcelain::branch::list_branches;
use grit_lib::remote::grit_server_auth::{ServerIdentity, IDENTITY_PATH};
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head_for_repository, HeadState};
use grit_protocol::RepositoryOptions;

use crate::smart_http::{self, Service};
use crate::store::{normalize_repository_name, Access, AuthStore, StoreError, Token, User};

/// File name of the database [`default_database_path`] picks.
pub const DATABASE_FILE_NAME: &str = "grit-serve.db";

/// How many commits the repository page lists.
const RECENT_COMMITS: usize = 20;

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
}

struct ServerState {
    repositories: Repositories,
    store: Mutex<AuthStore>,
    options: RepositoryOptions,
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
        Ok(Self {
            listener,
            state: Arc::new(ServerState {
                repositories: Repositories::at(&config.root),
                store: Mutex::new(store),
                options: config.repository_options,
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

fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route(&format!("/{IDENTITY_PATH}"), get(identity))
        .route("/{*path}", get(get_dispatch).post(post_dispatch))
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

fn store_failure(e: &StoreError) -> Response<Body> {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("user database error: {e}\n"),
    )
        .into_response()
}

/// The token a request carries: the basic-auth password (or the user name
/// when the password is empty), or a bearer token.
fn presented_token(headers: &HeaderMap) -> Option<Token> {
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
    fn lock_store(&self) -> std::sync::MutexGuard<'_, AuthStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<User, Rejection> {
        let token = presented_token(headers).ok_or_else(|| Box::new(unauthorized()))?;
        match self.lock_store().authenticate(&token) {
            Ok(Some(user)) => Ok(user),
            Ok(None) => Err(Box::new(unauthorized())),
            Err(e) => Err(Box::new(store_failure(&e))),
        }
    }

    /// Resolve `name` for a request that needs `needed` access.
    fn authorize(
        &self,
        headers: &HeaderMap,
        name: &str,
        needed: Access,
    ) -> Result<Authorized, Rejection> {
        let user = self.authenticate(headers)?;
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
                    format!("{} has read-only access to {}\n", user.name, repo.name),
                )
                    .into_response(),
            ));
        }
        Ok(Authorized { user, repo, access })
    }
}

/// An error response for a request that may not proceed (boxed: responses are large).
type Rejection = Box<Response<Body>>;

/// A request [`ServerState::authorize`] let through.
struct Authorized {
    user: User,
    repo: ServedRepository,
    access: Access,
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
    let Some(name) = path.strip_suffix("/info/refs") else {
        return repository_page(&state, path.trim_end_matches('/'), &headers).await;
    };
    let Some(service) = query.service.as_deref().and_then(Service::from_query) else {
        return (StatusCode::FORBIDDEN, "only smart HTTP is supported\n").into_response();
    };
    let repo = match state.authorize(&headers, name, needed_access(service)) {
        Ok(found) => found.repo,
        Err(resp) => return *resp,
    };
    let protocol_version = smart_http::protocol_version_from_headers(&headers);
    smart_http::info_refs(repo.path, service, protocol_version, state.options.clone()).await
}

async fn post_dispatch(
    State(state): State<Arc<ServerState>>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let Some((name, service)) = Service::strip_rpc_suffix(&path) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    let repo = match state.authorize(&headers, name, needed_access(service)) {
        Ok(found) => found.repo,
        Err(resp) => return *resp,
    };
    let protocol_version = smart_http::protocol_version_from_headers(&headers);
    smart_http::rpc(
        repo.path,
        service,
        protocol_version,
        &headers,
        body,
        state.options.clone(),
    )
    .await
}

/// `http://<Host>` for clone URLs on the pages.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

/// The token's user as JSON, so a client can check a token before storing it.
async fn identity(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response<Body> {
    let user = match state.authenticate(&headers) {
        Ok(user) => user,
        Err(resp) => return *resp,
    };
    axum::Json(ServerIdentity {
        name: user.name,
        admin: user.admin,
    })
    .into_response()
}

async fn index(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response<Body> {
    let user = match state.authenticate(&headers) {
        Ok(user) => user,
        Err(resp) => return *resp,
    };
    let base = base_url(&headers);
    let mut rows = String::new();
    {
        let store = state.lock_store();
        for repo in state.repositories.list() {
            let access = match store.access(&user, &repo.name) {
                Ok(Some(access)) => access,
                Ok(None) => continue,
                Err(e) => return store_failure(&e),
            };
            let name = escape_html(&repo.name);
            rows.push_str(&format!(
                "<tr><td><a href=\"/{name}\">{name}</a></td><td><span class=\"badge\">{}</span></td>\
                 <td><code>git clone {}/{name}.git</code></td></tr>\n",
                access.as_str(),
                escape_html(&base),
            ));
        }
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=\"3\">No repositories you can read.</td></tr>\n");
    }
    page(
        "Repositories",
        &user,
        &format!(
            "<h1>Repositories</h1>\n<table>\n<tr><th>Name</th><th>Access</th><th>Clone</th></tr>\n{rows}</table>\n"
        ),
    )
}

/// What the repository page shows, read off the request thread.
struct RepositorySummary {
    head: Option<String>,
    branches: Vec<(String, String, bool)>,
    commits: Vec<CommitLine>,
}

struct CommitLine {
    oid: String,
    subject: String,
    author: String,
    date: String,
}

async fn repository_page(state: &ServerState, name: &str, headers: &HeaderMap) -> Response<Body> {
    let Authorized { user, repo, access } = match state.authorize(headers, name, Access::Read) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    let path = repo.path.clone();
    let options = state.options.clone();
    let summary = match tokio::task::spawn_blocking(move || summarize(&path, &options)).await {
        Ok(Ok(summary)) => summary,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read repository: {e}\n"),
            )
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    };

    let title = escape_html(&repo.name);
    let mut body = format!(
        "<p><a href=\"/\">Repositories</a></p>\n<h1>{title} <span class=\"badge\">{}</span></h1>\n\
         <pre>git clone {}/{title}.git</pre>\n",
        access.as_str(),
        escape_html(&base_url(headers)),
    );
    body.push_str("<h2>Branches</h2>\n");
    if summary.branches.is_empty() {
        body.push_str("<p>No branches yet. Push one to get started.</p>\n");
    } else {
        body.push_str("<table>\n");
        for (branch, oid, current) in &summary.branches {
            let marker = if *current {
                " <span class=\"badge\">HEAD</span>"
            } else {
                ""
            };
            body.push_str(&format!(
                "<tr><td>{}{marker}</td><td><code>{oid}</code></td></tr>\n",
                escape_html(branch)
            ));
        }
        body.push_str("</table>\n");
    }
    if let Some(head) = &summary.head {
        body.push_str(&format!(
            "<h2>Recent commits on {}</h2>\n",
            escape_html(head)
        ));
        body.push_str("<table>\n");
        for commit in &summary.commits {
            body.push_str(&format!(
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                commit.oid,
                escape_html(&commit.subject),
                escape_html(&commit.author),
                commit.date,
            ));
        }
        body.push_str("</table>\n");
    }
    page(&repo.name, &user, &body)
}

fn short_hex(oid: &grit_lib::objects::ObjectId) -> String {
    let hex = oid.to_hex();
    hex.get(..7).unwrap_or(&hex).to_owned()
}

/// Read the branches and the first-parent history of `HEAD`.
fn summarize(
    path: &Path,
    options: &RepositoryOptions,
) -> grit_lib::error::Result<RepositorySummary> {
    let repo = Repository::open(path, &options.clone().for_serving())?;
    let branches = list_branches(&repo)?
        .branches
        .into_iter()
        .map(|b| (b.name, short_hex(&b.oid), b.current))
        .collect();
    let (head, mut next) = match resolve_head_for_repository(&repo)? {
        HeadState::Branch {
            short_name, oid, ..
        } => (Some(short_name), oid),
        HeadState::Detached { oid } => (Some("HEAD".to_owned()), Some(oid)),
        HeadState::Invalid => (None, None),
    };
    let mut commits = Vec::new();
    while let Some(oid) = next.take() {
        if commits.len() == RECENT_COMMITS {
            break;
        }
        let object = repo.odb().read(&oid)?;
        if object.kind != ObjectKind::Commit {
            break;
        }
        let commit = parse_commit(&object.data)?;
        let (author, date) = split_signature(&commit.author);
        commits.push(CommitLine {
            oid: short_hex(&oid),
            subject: commit.message.lines().next().unwrap_or_default().to_owned(),
            author,
            date,
        });
        next = commit.parents.first().copied();
    }
    Ok(RepositorySummary {
        head: head.filter(|_| !commits.is_empty()),
        branches,
        commits,
    })
}

/// Split `Name <email> 1712345678 +0200` into the name and a `YYYY-MM-DD` date.
fn split_signature(signature: &str) -> (String, String) {
    let (name, rest) = signature.split_once(" <").unwrap_or((signature, ""));
    let date = rest
        .rsplit_once("> ")
        .and_then(|(_, when)| when.split_whitespace().next())
        .and_then(|seconds| seconds.parse::<i64>().ok())
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .map(|when| when.date().to_string())
        .unwrap_or_default();
    (name.to_owned(), date)
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = "
:root { color-scheme: light dark; --fg: #1f2328; --muted: #59636e; --line: #d1d9e0; --bg: #ffffff; --chip: #eef1f4; }
@media (prefers-color-scheme: dark) { :root { --fg: #e6edf3; --muted: #9198a1; --line: #3d444d; --bg: #0d1117; --chip: #212830; } }
body { font: 15px/1.5 system-ui, sans-serif; color: var(--fg); background: var(--bg); max-width: 960px; margin: 0 auto; padding: 16px; }
header { display: flex; justify-content: space-between; color: var(--muted); border-bottom: 1px solid var(--line); padding-bottom: 8px; }
a { color: inherit; }
table { border-collapse: collapse; width: 100%; }
td, th { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--line); vertical-align: top; }
pre, code { font: 13px ui-monospace, monospace; }
pre { background: var(--chip); padding: 8px 12px; overflow-x: auto; }
.badge { font-size: 12px; background: var(--chip); border-radius: 10px; padding: 1px 8px; color: var(--muted); }
";

fn page(title: &str, user: &User, body: &str) -> Response<Body> {
    let role = if user.admin { " (admin)" } else { "" };
    Html(format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{} · grit</title>\n<style>{STYLE}</style>\n</head>\n<body>\n\
         <header><strong>grit serve</strong><span>{}{role}</span></header>\n{body}</body>\n</html>\n",
        escape_html(title),
        escape_html(&user.name),
    ))
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
    fn signatures_split_into_name_and_date() {
        assert_eq!(
            split_signature("A U Thor <a@example.com> 1112911993 +0200"),
            ("A U Thor".to_owned(), "2005-04-07".to_owned())
        );
        assert_eq!(split_signature("odd"), ("odd".to_owned(), String::new()));
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            escape_html("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
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
