//! Minimal Git smart HTTP server powered by grit.
//!
//! Implements the four endpoints required for git clone/push over HTTP:
//!   GET  /:repo/info/refs?service=git-upload-pack
//!   GET  /:repo/info/refs?service=git-receive-pack
//!   POST /:repo/git-upload-pack
//!   POST /:repo/git-receive-pack

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use grit_http_server::limits::{parse_duration, serve_with_shutdown, ServeLimits};
use grit_http_server::smart_http::{self, Service};

#[derive(Parser)]
#[command(
    name = "grit-http-server",
    about = "Git smart HTTP server powered by grit"
)]
struct Args {
    /// Root directory containing bare repositories
    #[arg(short, long, default_value = ".")]
    root: PathBuf,

    /// Address to bind to
    #[arg(short, long, default_value = "127.0.0.1:9418")]
    bind: String,

    /// Require HTTP basic auth: `user:pass`. When set, every request must carry a
    /// matching `Authorization: Basic <base64(user:pass)>` header; otherwise the
    /// server replies `401 Unauthorized` with `WWW-Authenticate: Basic realm="git"`.
    /// Used to exercise the credential / 401-retry path end-to-end.
    #[arg(long, value_name = "USER:PASS")]
    require_auth: Option<String>,

    /// Append every received request's headers to this file (one `name: value`
    /// per line, a blank line between requests). Lets a test assert that a
    /// configured `Cookie` / `http.extraHeader` actually reached the server.
    #[arg(long, value_name = "FILE")]
    log_headers: Option<PathBuf>,

    /// Emit this `Set-Cookie` header value on every response. Lets a test exercise
    /// the client's `http.saveCookies` persistence path end-to-end.
    #[arg(long, value_name = "NAME=VALUE")]
    set_cookie: Option<String>,

    /// Maximum time to wait for complete request headers.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "30s",
        value_parser = parse_duration
    )]
    header_timeout: std::time::Duration,

    /// Close connections that stop sending request body chunks for this long.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "60s",
        value_parser = parse_duration
    )]
    idle_timeout: std::time::Duration,

    /// Concurrent smart-HTTP Git requests before new ones get HTTP 503.
    #[arg(long, value_name = "N", default_value = "128")]
    max_git_requests: usize,
}

#[derive(Clone)]
struct AppState {
    root: PathBuf,
    idle_timeout: std::time::Duration,
    /// Repository open/config options captured from the server process at startup.
    repository_options: grit_protocol::RepositoryOptions,
    /// The expected `Authorization: Basic …` header value when basic auth is
    /// required (`Some` iff `--require-auth` was given), precomputed once.
    expected_auth: Option<String>,
    /// Where to append received request headers (`--log-headers`), if set.
    log_headers: Option<PathBuf>,
    /// A `Set-Cookie` value to emit on every response (`--set-cookie`), if set.
    set_cookie: Option<String>,
}

impl AppState {
    /// Append a request's headers to the `--log-headers` file (if configured),
    /// one `name: value` line per header, terminated by a blank line. Best-effort:
    /// any I/O error is ignored so logging never affects the served response.
    fn log_request_headers(&self, headers: &HeaderMap) {
        let Some(path) = self.log_headers.as_ref() else {
            return;
        };
        use std::io::Write as _;
        let mut buf = String::new();
        for (name, value) in headers {
            let v = value.to_str().unwrap_or("<non-utf8>");
            buf.push_str(name.as_str());
            buf.push_str(": ");
            buf.push_str(v);
            buf.push('\n');
        }
        buf.push('\n');
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(buf.as_bytes());
        }
    }
}

impl AppState {
    /// `Ok(())` when the request is authorized (or auth is not required); `Err`
    /// is a ready-to-return `401` challenge response otherwise.
    fn check_auth(&self, headers: &HeaderMap) -> Result<(), Box<Response>> {
        let Some(expected) = &self.expected_auth else {
            return Ok(());
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if presented == Some(expected.as_str()) {
            return Ok(());
        }
        Err(Box::new(unauthorized()))
    }
}

/// A `401 Unauthorized` response advertising HTTP basic auth, matching what a
/// real Git server returns so the client's `WWW-Authenticate` parsing + 401
/// retry path is exercised.
fn unauthorized() -> Response {
    axum::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"git\"")
        .body(axum::body::Body::from("authentication required"))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

/// Build the expected `Authorization: Basic …` header value from a
/// `user:pass` string.
fn expected_basic_auth(user_pass: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(user_pass.as_bytes());
    format!("Basic {encoded}")
}

impl AppState {
    fn repo_path(&self, repo: &str) -> Result<PathBuf, Box<Response>> {
        // Sanitize: reject path traversal
        if repo.contains("..") {
            return Err(Box::new(
                (StatusCode::BAD_REQUEST, "invalid repository path").into_response(),
            ));
        }
        let path = self.root.join(repo);
        grit_protocol::validate_repo_path(&path)
            .map_err(|_| Box::new((StatusCode::NOT_FOUND, "repository not found").into_response()))
    }

    /// Add the `--set-cookie` header to a successful response, if configured.
    fn with_cookie(&self, mut response: Response) -> Response {
        if !response.status().is_success() {
            return response;
        }
        if let Some(value) = self
            .set_cookie
            .as_deref()
            .and_then(|v| HeaderValue::from_str(v).ok())
        {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        response
    }
}

#[derive(serde::Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let root = std::fs::canonicalize(&args.root)?;
    eprintln!("Serving repositories from: {}", root.display());

    let expected_auth = args.require_auth.as_deref().map(expected_basic_auth);
    if expected_auth.is_some() {
        eprintln!("HTTP basic auth required (--require-auth)");
    }

    if args.log_headers.is_some() {
        eprintln!("Logging request headers (--log-headers)");
    }

    let repository_options = grit_protocol::repository_options_from_process();
    let state = Arc::new(AppState {
        root,
        idle_timeout: args.idle_timeout,
        repository_options,
        expected_auth,
        log_headers: args.log_headers,
        set_cookie: args.set_cookie,
    });

    let app = Router::new()
        .route("/{*repo}", get(info_refs_dispatch).post(rpc_dispatch))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    eprintln!("Listening on {}", args.bind);
    let limits = ServeLimits {
        header_timeout: args.header_timeout,
        idle_timeout: args.idle_timeout,
        max_git_requests: args.max_git_requests,
    };
    serve_with_shutdown(
        app,
        listener,
        limits,
        grit_http_server::server::Shutdown::default(),
        || {},
    )
    .await?;

    Ok(())
}

// Dispatch GET requests — only info/refs is valid
async fn info_refs_dispatch(
    State(state): State<Arc<AppState>>,
    Path(full_path): Path<String>,
    Query(query): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response {
    state.log_request_headers(&headers);
    let Some(repo) = full_path.strip_suffix("/info/refs") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if repo.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing repository").into_response();
    }
    if let Err(resp) = state.check_auth(&headers) {
        return *resp;
    }
    let Some(service) = query.service.as_deref().and_then(Service::from_query) else {
        return (StatusCode::BAD_REQUEST, "unsupported service").into_response();
    };
    let repo_path = match state.repo_path(repo) {
        Ok(p) => p,
        Err(e) => return *e,
    };
    let protocol_version = smart_http::protocol_version_from_headers(&headers);
    let response = smart_http::info_refs_holding(
        repo_path,
        service,
        protocol_version,
        state.repository_options.clone(),
        grit_protocol::Extensions::default(),
        state.idle_timeout,
        (),
    )
    .await;
    state.with_cookie(response)
}

// Dispatch POST requests to upload-pack or receive-pack. The body is taken raw
// (not as `Bytes`) so each service decides how to read it: buffered and capped
// for fetch requests, streamed without a cap for pushes.
async fn rpc_dispatch(
    State(state): State<Arc<AppState>>,
    Path(full_path): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    state.log_request_headers(&headers);
    if let Err(resp) = state.check_auth(&headers) {
        return finish_with_discarded_body(body, *resp).await;
    }
    let Some((repo, service)) = Service::strip_rpc_suffix(&full_path) else {
        return finish_with_discarded_body(
            body,
            (StatusCode::NOT_FOUND, "not found").into_response(),
        )
        .await;
    };
    if repo.is_empty() {
        return finish_with_discarded_body(
            body,
            (StatusCode::BAD_REQUEST, "missing repository").into_response(),
        )
        .await;
    }
    let repo_path = match state.repo_path(repo) {
        Ok(p) => p,
        Err(e) => return finish_with_discarded_body(body, *e).await,
    };
    let protocol_version = smart_http::protocol_version_from_headers(&headers);
    let (response, _follow_up) = smart_http::rpc_holding(
        repo_path,
        service,
        protocol_version,
        &headers,
        body,
        state.repository_options.clone(),
        grit_protocol::Extensions::default(),
        grit_protocol::ServeSession::none(),
        state.idle_timeout,
        (),
    )
    .await;
    state.with_cookie(response)
}

async fn finish_with_discarded_body(body: Body, response: Response) -> Response {
    let outcome = grit_http_server::request_body::discard_request_body(body).await;
    grit_http_server::request_body::apply_discard_outcome(response, outcome)
}
