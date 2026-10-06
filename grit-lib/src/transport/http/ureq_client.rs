//! Default [`HttpClient`](super::HttpClient) backed by [`ureq`] (feature `http-ureq`).
//!
//! This is the batteries-included client for embedders who do not want to wire
//! their own HTTP stack. It lifts the core request path from the CLI's
//! `http_client.rs` — a blocking [`ureq::Agent`], the `Git-Protocol` /
//! `User-Agent` / `Content-Type` / `Accept` headers, and reading the response
//! body into a `Vec<u8>` — and adds optional HTTP basic auth driven by a
//! [`CredentialProvider`]: on a `401` it parses `WWW-Authenticate`, fills a
//! credential, and retries once, then `approve`/`reject`s the credential per the
//! retry status (Git's `credential_approve` / `credential_reject`).
//!
//! When authentication is required but cannot be satisfied — no provider wired,
//! the provider cannot supply a usable username/password, an unsupported auth
//! scheme, or rejected credentials — the request fails with the typed
//! [`Error::Auth`] (never a hang), so embedders can detect an auth failure and
//! fall back (e.g. to an interactive/subprocess path) without string-matching.
//!
//! ## Proxy, cookies, and extra headers (config-driven)
//!
//! [`UreqHttpClient::from_config`] reads the Git HTTP config that controls
//! request shaping, lifted from the CLI's `http_client.rs`:
//!
//! * **Proxy** — `http.proxy` (or the `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`
//!   environment, honored by `ureq` itself) routes requests through an HTTP(S)
//!   proxy via [`ureq::Proxy`]. `http://user:pass@host:port` proxy auth is
//!   forwarded by `ureq`.
//! * **Cookies** — `http.cookieFile` is parsed (Netscape and `Set-Cookie:`
//!   formats) and a matching `Cookie:` header is sent on each request; with
//!   `http.saveCookies=true` any `Set-Cookie` from the response is appended back
//!   to the file. Lifts `cookie_header_for_url` / `save_response_cookies`.
//! * **Extra headers** — `http.extraHeader` (optionally URL-scoped via
//!   `http.<url>.extraHeader`) adds arbitrary request headers; an empty value
//!   resets the accumulated list, matching Git. Lifts `extra_headers_for_url`.
//!
//! Deliberately omitted vs. the CLI client (and noted for parity): the
//! absolute-form-through-proxy `HttpForward` path and the SOCKS-over-Unix tunnel
//! (`socks*://localhost/path.sock`) — `ureq`'s built-in SOCKS support is not
//! compiled in here, so a SOCKS proxy URL is rejected with a clear error rather
//! than silently mishandled. Also omitted: custom CA bundles, gzip request
//! bodies, and the `GIT_ASKPASS` / `GIT_TRACE_CURL` plumbing. Embedders that need
//! those should implement [`HttpClient`](super::HttpClient) over their own stack.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;

use crate::config::{self, ConfigSet};
use crate::credentials::{self, Credential, CredentialProvider};
use crate::error::{Error, Result};

use super::{
    effective_info_refs_url_after_redirect, http_origin_key, http_origins_match, HttpClient,
};

/// A blocking [`ureq`]-backed [`HttpClient`].
///
/// Build with [`UreqHttpClient::new`] for an unauthenticated client, or
/// [`UreqHttpClient::with_credentials`] to wire a [`CredentialProvider`] for
/// HTTP basic auth on `401`. A default `Git-Protocol` header can be set via
/// [`UreqHttpClient::with_git_protocol`]. To honor `http.proxy`,
/// `http.cookieFile`, and `http.extraHeader`, build from a loaded [`ConfigSet`]
/// with [`UreqHttpClient::from_config`].
#[derive(Clone, Debug)]
struct CachedAuthHeader {
    header: String,
    origin: String,
}

pub struct UreqHttpClient {
    agent: ureq::Agent,
    user_agent: String,
    git_protocol: Option<String>,
    credentials: Option<Box<dyn CredentialProvider + Send + Sync>>,
    /// Cached `Authorization: Basic …` header and the origin it is valid for.
    cached_auth: Mutex<Option<CachedAuthHeader>>,
    /// Origin of the first HTTP request in this session; host-only cookies without
    /// a `Domain` attribute are sent only on this origin.
    hostonly_cookie_scope: Mutex<Option<String>>,
    /// Parsed cookies (from `http.cookieFile`), sent as a `Cookie:` header on
    /// matching URLs. Empty when no cookie file was configured.
    cookies: Vec<CookieSpec>,
    /// The cookie file path (for `http.saveCookies` persistence), if configured.
    cookie_file_path: Option<PathBuf>,
    /// Whether to append `Set-Cookie` response headers back to the cookie file
    /// (`http.saveCookies=true`, only meaningful with a cookie file).
    save_cookies: bool,
    /// Configured `http.extraHeader` rules (optionally URL-scoped).
    extra_headers: Vec<ExtraHeaderRule>,
    /// Merged Git config used for credential URL matching.
    credential_config: ConfigSet,
}

impl UreqHttpClient {
    /// A client with default timeouts and no credential provider.
    #[must_use]
    pub fn new() -> Self {
        Self::with_agent(default_agent(None))
    }

    fn with_agent(agent: ureq::Agent) -> Self {
        Self {
            agent,
            user_agent: default_user_agent(),
            git_protocol: None,
            credentials: None,
            cached_auth: Mutex::new(None),
            hostonly_cookie_scope: Mutex::new(None),
            cookies: Vec::new(),
            cookie_file_path: None,
            save_cookies: false,
            extra_headers: Vec::new(),
            credential_config: ConfigSet::default(),
        }
    }

    /// A client that uses `provider` to fill HTTP basic-auth credentials when
    /// the server returns `401 Unauthorized`.
    #[must_use]
    pub fn with_credentials(provider: Box<dyn CredentialProvider + Send + Sync>) -> Self {
        let mut c = Self::new();
        c.credentials = Some(provider);
        c
    }

    /// Build a client from merged Git config, honoring the request-shaping HTTP
    /// settings: `http.proxy` (HTTP/HTTPS proxy), `http.cookieFile` +
    /// `http.saveCookies` (cookie jar), and `http.extraHeader` (custom request
    /// headers). The protocol header (`protocol.version`) is *not* applied here;
    /// set it explicitly with [`UreqHttpClient::with_git_protocol`] if desired.
    ///
    /// # Errors
    ///
    /// Returns an error if `http.proxy` names a proxy scheme that this client
    /// cannot honor (e.g. a SOCKS proxy, which `ureq`'s built-in SOCKS support is
    /// not compiled in for here), or the cookie file cannot be read.
    pub fn from_config(config: &ConfigSet) -> Result<Self> {
        let proxy = build_proxy(config)?;
        let agent = default_agent(proxy);
        let mut client = Self::with_agent(agent);

        // Cookies: parse the cookie file (if any) and decide whether to persist.
        client.cookie_file_path = config
            .get("http.cookieFile")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        client.cookies = build_cookie_specs(client.cookie_file_path.as_deref())?;
        client.save_cookies = client.cookie_file_path.is_some()
            && config
                .get_bool("http.saveCookies")
                .and_then(std::result::Result::ok)
                .unwrap_or(false);

        // Extra headers (`http.extraHeader`, optionally URL-scoped).
        client.extra_headers = extra_header_rules_from_config(config);
        client.credential_config = config.clone();

        Ok(client)
    }

    /// Wire a [`CredentialProvider`] for HTTP basic auth on `401` (consuming
    /// self), so a [`from_config`](UreqHttpClient::from_config)-built client can
    /// also authenticate.
    #[must_use]
    pub fn with_credential_provider(
        mut self,
        provider: Box<dyn CredentialProvider + Send + Sync>,
    ) -> Self {
        self.credentials = Some(provider);
        self
    }

    /// Set a default `Git-Protocol` request-header value (e.g. `version=2`).
    #[must_use]
    pub fn with_git_protocol(mut self, value: impl Into<String>) -> Self {
        self.git_protocol = Some(value.into());
        self
    }

    /// Override the `User-Agent` header.
    #[must_use]
    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    fn cached_auth_header_for_url(&self, url: &str) -> Option<String> {
        let origin = http_origin_key(url)?;
        let guard = self.cached_auth.lock().ok()?;
        guard
            .as_ref()
            .filter(|entry| entry.origin == origin)
            .map(|entry| entry.header.clone())
    }

    fn store_auth_header(&self, header: String, url: &str) {
        if let Some(origin) = http_origin_key(url) {
            if let Ok(mut g) = self.cached_auth.lock() {
                *g = Some(CachedAuthHeader { header, origin });
            }
        }
    }

    fn clear_auth_header(&self) {
        if let Ok(mut g) = self.cached_auth.lock() {
            *g = None;
        }
    }

    /// Seed cached basic auth for a URL origin (integration tests only).
    #[doc(hidden)]
    pub fn seed_cached_basic_auth(&self, url: &str, username: &str, password: &str) {
        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        self.store_auth_header(format!("Basic {encoded}"), url);
    }

    fn hostonly_cookie_scope_for_url(&self, url: &str) -> Option<String> {
        let origin = http_origin_key(url)?;
        let mut guard = self.hostonly_cookie_scope.lock().ok()?;
        if guard.is_none() {
            *guard = Some(origin.clone());
        }
        guard.clone()
    }

    fn invalidate_auth_on_cross_origin_redirect(&self, request_url: &str, final_url: &str) {
        if !http_origins_match(request_url, final_url) {
            self.clear_auth_header();
        }
    }

    /// The `Cookie:` header value to send for `url`, or `None` when no cookie
    /// matches. Lifts the CLI's `cookie_header_for_url`.
    fn cookie_header_for_url(&self, url: &str) -> Option<String> {
        if self.cookies.is_empty() {
            return None;
        }
        let scope = self.hostonly_cookie_scope_for_url(url);
        let parsed = url::Url::parse(url).ok();
        let parts = self
            .cookies
            .iter()
            .filter(|cookie| cookie.matches_url(parsed.as_ref(), scope.as_deref()))
            .map(|cookie| cookie.name_value.clone())
            .collect::<Vec<_>>();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// The ordered `(name, value)` extra headers to send for `url`. Lifts the
    /// CLI's `extra_headers_for_url`: an unscoped rule always applies, a
    /// URL-scoped rule applies on a URL-match, and an empty value resets the
    /// accumulated list (Git's `http.extraHeader=` reset).
    fn extra_headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        for rule in &self.extra_headers {
            let matches = rule
                .pattern
                .as_deref()
                .is_none_or(|pattern| config::url_matches(pattern, url));
            if !matches {
                continue;
            }
            match &rule.header {
                Some(header) => headers.push(header.clone()),
                None => headers.clear(),
            }
        }
        headers
    }

    /// Persist any `Set-Cookie` from a response back to the cookie file
    /// (`http.saveCookies`). Lifts the CLI's `save_response_cookies`: appends one
    /// `Set-Cookie: <value>` line per cookie, in Git's cookie-file format.
    fn save_response_cookies(&self, set_cookies: &[String]) {
        if !self.save_cookies {
            return;
        }
        let Some(path) = self.cookie_file_path.as_ref() else {
            return;
        };
        let values: Vec<&str> = set_cookies
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .collect();
        if values.is_empty() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write as _;
            for value in values {
                let _ = writeln!(file, "Set-Cookie: {value}");
            }
        }
    }
}

impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// A raw HTTP response captured from ureq (success or error status).
struct RawResponse {
    status: u16,
    www_authenticate: Vec<String>,
    set_cookie: Vec<String>,
    body: Vec<u8>,
    /// The final URL after any redirects ureq followed (from `Response::get_url`).
    final_url: String,
}

fn default_user_agent() -> String {
    format!("grit-lib/{}", env!("CARGO_PKG_VERSION"))
}

/// Build a [`ureq::Agent`] with default Git-shaped timeouts and an optional proxy.
fn default_agent(proxy: Option<ureq::Proxy>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(600)))
        // Return >= 400 responses as `Ok` so the auth-retry logic can inspect the
        // status and `WWW-Authenticate` headers rather than treating them as hard
        // transport errors (ureq's default surfaces them as `Error::StatusCode`).
        .http_status_as_error(false)
        .proxy(proxy)
        .build()
        .new_agent()
}

/// Resolve `http.proxy` into a [`ureq::Proxy`], or `None` for the direct path.
///
/// `ureq` itself honors the `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`
/// environment when no explicit proxy is set on the agent, so we only build a
/// proxy object for the explicit `http.proxy` config. SOCKS proxies (and the
/// `socks*://localhost/path.sock` Unix-socket form) are rejected here: `ureq`'s
/// SOCKS support is not compiled in for this build, so honoring them would
/// silently fail at request time — fail loudly instead. Lifts the relevant parts
/// of the CLI's `build_transport` / `build_transport_from_proxy`.
fn build_proxy(config: &ConfigSet) -> Result<Option<ureq::Proxy>> {
    let Some(raw) = config.get("http.proxy") else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        // An empty `http.proxy` disables proxying (Git's "no proxy" override);
        // do NOT fall back to the environment.
        return Ok(None);
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let scheme = with_scheme
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    if scheme.starts_with("socks") {
        return Err(Error::Message(format!(
            "http.proxy '{raw}': SOCKS proxies are not supported by the default ureq HTTP client; \
             implement a custom HttpClient or use an HTTP proxy"
        )));
    }
    if scheme != "http" && scheme != "https" {
        return Err(Error::Message(format!(
            "http.proxy '{raw}': unsupported proxy scheme '{scheme}'"
        )));
    }
    let proxy = ureq::Proxy::new(&with_scheme)
        .map_err(|e| Error::Message(format!("invalid http.proxy '{raw}': {e}")))?;
    Ok(Some(proxy))
}

/// Decompose a request URL into a target [`Credential`].
///
/// Git omits the path from HTTP credentials unless `credential.useHttpPath`
/// applies. That lets a host-level token stored for `https://github.com` satisfy
/// requests to any repository on that host, which is how helpers such as macOS
/// `osxkeychain` are normally populated.
fn credential_for_url(url: &str, config: &ConfigSet) -> Option<Credential> {
    let parsed = url::Url::parse(url).ok()?;
    let protocol = parsed.scheme().to_string();
    let host = parsed.host_str()?.to_string();
    let host = if let Some(port) = parsed.port() {
        format!("{host}:{port}")
    } else {
        host
    };
    let path = credentials::use_http_path(config, Some(url))
        .then(|| parsed.path().trim_start_matches('/').to_string())
        .filter(|path| !path.is_empty());
    Some(Credential {
        protocol: Some(protocol),
        host: Some(host),
        path,
        url: Some(url.to_string()),
        ..Default::default()
    })
}

/// Build the `Authorization: Basic …` header value for `cred`.
fn basic_auth_header(cred: &Credential) -> Option<String> {
    let user = cred.username.as_deref()?;
    let pass = cred.password.as_deref().unwrap_or("");
    let raw = format!("{user}:{pass}");
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
    Some(format!("Basic {encoded}"))
}

/// True when the server's `WWW-Authenticate` challenges include a Basic scheme
/// (or none at all — be permissive and try basic).
fn offers_basic(challenges: &[String]) -> bool {
    challenges.is_empty()
        || challenges
            .iter()
            .any(|c| c.trim_start().to_ascii_lowercase().starts_with("basic"))
}

impl UreqHttpClient {
    fn do_get(
        &self,
        url: &str,
        git_protocol: Option<&str>,
        auth: Option<&str>,
    ) -> Result<RawResponse> {
        let mut req = self.agent.get(url).header("User-Agent", &self.user_agent);
        if let Some(v) = git_protocol {
            req = req.header("Git-Protocol", v);
        }
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        if let Some(cookie) = self.cookie_header_for_url(url) {
            req = req.header("Cookie", &cookie);
        }
        for (name, value) in self.extra_headers_for_url(url) {
            req = req.header(&name, &value);
        }
        finish(req.call())
    }

    fn do_post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
        auth: Option<&str>,
    ) -> Result<RawResponse> {
        let mut req = self
            .agent
            .post(url)
            .header("Content-Type", content_type)
            .header("Accept", accept)
            .header("User-Agent", &self.user_agent);
        if let Some(v) = git_protocol {
            req = req.header("Git-Protocol", v);
        }
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        if let Some(cookie) = self.cookie_header_for_url(url) {
            req = req.header("Cookie", &cookie);
        }
        for (name, value) in self.extra_headers_for_url(url) {
            req = req.header(&name, &value);
        }
        finish(req.send(body))
    }

    /// Run `attempt` (a GET or POST closure) with auth-retry on 401, returning
    /// the response body and the final URL after any followed redirects.
    fn with_auth_retry<F>(&self, url: &str, attempt: F) -> Result<(Vec<u8>, String)>
    where
        F: Fn(&str, Option<&str>) -> Result<RawResponse>,
    {
        let initial_auth = self.cached_auth_header_for_url(url);
        let first = attempt(url, initial_auth.as_deref())?;
        if first.status != 401 {
            self.invalidate_auth_on_cross_origin_redirect(url, &first.final_url);
            self.save_response_cookies(&first.set_cookie);
            return finalize_status(url, first.status, first.body, first.final_url);
        }
        // 401: need credentials. Without a provider, surface a typed auth error
        // (so embedders can detect it and e.g. fall back to a subprocess path)
        // rather than a generic message — and never block.
        let Some(provider) = self.credentials.as_ref() else {
            self.clear_auth_header();
            return Err(Error::Auth(format!(
                "{url}: server requires authentication (401) but no credential provider is configured"
            )));
        };
        if !offers_basic(&first.www_authenticate) {
            return Err(Error::Auth(format!(
                "{url}: server requires an unsupported auth scheme: {:?}",
                first.www_authenticate
            )));
        }
        let auth_url = effective_info_refs_url_after_redirect(url, Some(first.final_url.as_str()));
        let Some(input) = credential_for_url(&auth_url, &self.credential_config) else {
            return Err(Error::Auth(format!(
                "{auth_url}: server requires authentication (401) but the URL could not be decomposed into credential fields"
            )));
        };
        // A provider that cannot supply a credential (no configured helper, or a
        // non-interactive failure) reports it as a typed auth error rather than a
        // generic message, and crucially never blocks on a TTY.
        let cred = provider
            .fill(&input)
            .map_err(|e| Error::Auth(format!("{auth_url}: could not obtain credentials: {e}")))?;
        let Some(header) = basic_auth_header(&cred) else {
            return Err(Error::Auth(format!(
                "{auth_url}: credential helper returned no usable username/password"
            )));
        };
        let retry = attempt(&auth_url, Some(&header))?;
        if retry.status == 401 {
            // Credentials were supplied but rejected: erase them and surface a
            // typed auth error.
            let _ = provider.reject(&cred);
            self.clear_auth_header();
            return Err(Error::Auth(format!(
                "{url}: supplied credentials were rejected (401)"
            )));
        }
        if retry.status >= 400 {
            let _ = provider.reject(&cred);
            self.clear_auth_header();
            return Err(http_status_error(url, retry.status));
        }
        // Success: approve + cache the working auth header for this origin only.
        let _ = provider.approve(&cred);
        if http_origins_match(&auth_url, &retry.final_url) {
            self.store_auth_header(header, &auth_url);
        } else {
            self.clear_auth_header();
        }
        self.save_response_cookies(&retry.set_cookie);
        Ok((retry.body, retry.final_url))
    }
}

impl HttpClient for UreqHttpClient {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> Result<Vec<u8>> {
        let gp = git_protocol.or(self.git_protocol.as_deref());
        self.with_auth_retry(url, |target, auth| self.do_get(target, gp, auth))
            .map(|(body, _)| body)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Vec<u8>> {
        let gp = git_protocol.or(self.git_protocol.as_deref());
        self.with_auth_retry(url, |target, auth| {
            self.do_post(target, content_type, accept, body, gp, auth)
        })
        .map(|(body, _)| body)
    }

    fn get_with_final_url(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let gp = git_protocol.or(self.git_protocol.as_deref());
        self.with_auth_retry(url, |target, auth| self.do_get(target, gp, auth))
            .map(|(body, final_url)| (body, Some(final_url)))
    }

    fn git_protocol_header(&self) -> Option<&str> {
        self.git_protocol.as_deref()
    }

    fn reset_auth_after_redirect_rebase(&self) {
        self.clear_auth_header();
    }
}

/// Convert a ureq call result into a [`RawResponse`], reading the body.
///
/// The agent is configured with `http_status_as_error(false)`, so >= 400
/// responses arrive as `Ok` and are mapped into a `RawResponse` (letting the
/// auth-retry logic inspect the status and `WWW-Authenticate` headers rather
/// than treating them as hard errors). Only genuine transport failures are `Err`.
fn finish(
    result: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<RawResponse> {
    match result {
        Ok(resp) => Ok(read_response(resp)),
        Err(e) => Err(Error::Message(format!("http transport error: {e}"))),
    }
}

fn read_response(resp: ureq::http::Response<ureq::Body>) -> RawResponse {
    use ureq::ResponseExt as _;
    let status = resp.status().as_u16();
    let final_url = resp.get_uri().to_string();
    let www_authenticate = header_values(resp.headers(), "WWW-Authenticate");
    let set_cookie = header_values(resp.headers(), "Set-Cookie");
    let mut body = Vec::new();
    // Read the body (unbounded; git packs can be large). An error leaves `body`
    // as whatever was read.
    let _ = resp.into_body().into_reader().read_to_end(&mut body);
    RawResponse {
        status,
        www_authenticate,
        set_cookie,
        body,
        final_url,
    }
}

/// Collect all values for a header as owned UTF-8 strings (skipping any that
/// aren't valid UTF-8).
fn header_values(headers: &ureq::http::HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .collect()
}

fn finalize_status(
    url: &str,
    status: u16,
    body: Vec<u8>,
    final_url: String,
) -> Result<(Vec<u8>, String)> {
    if status >= 400 {
        return Err(http_status_error(url, status));
    }
    Ok((body, final_url))
}

fn http_status_error(url: &str, status: u16) -> Error {
    Error::Message(format!("HTTP {status} from {url}"))
}

// ---------------------------------------------------------------------------
// Extra headers (`http.extraHeader`) — lifted from the CLI's http_client.rs.
// ---------------------------------------------------------------------------

/// One `http.extraHeader` rule: an optional URL scope (the config subsection),
/// and the header to add (or `None` for an empty value, which resets the list).
#[derive(Clone)]
struct ExtraHeaderRule {
    pattern: Option<String>,
    header: Option<(String, String)>,
}

/// Collect `http.extraHeader` (and `http.<url>.extraHeader`) rules in config
/// order. Lifts the CLI's `extra_header_rules_from_config`.
fn extra_header_rules_from_config(config: &ConfigSet) -> Vec<ExtraHeaderRule> {
    let mut rules = Vec::new();
    for entry in config.entries() {
        let Some((pattern, variable)) = parse_http_config_key(&entry.key) else {
            continue;
        };
        if !variable.eq_ignore_ascii_case("extraheader") {
            continue;
        }
        let pattern = pattern.map(ToOwned::to_owned);
        let Some(raw) = entry.value.as_deref() else {
            rules.push(ExtraHeaderRule {
                pattern,
                header: None,
            });
            continue;
        };
        if raw.trim().is_empty() {
            rules.push(ExtraHeaderRule {
                pattern,
                header: None,
            });
            continue;
        }
        if let Some((name, value)) = raw.split_once(':') {
            let name = name.trim();
            if !name.is_empty() {
                rules.push(ExtraHeaderRule {
                    pattern,
                    header: Some((name.to_string(), value.trim_start().to_string())),
                });
            }
        }
    }
    rules
}

/// Split an `http[.<subsection>].<variable>` config key into `(subsection,
/// variable)`. Lifts the CLI's `parse_http_config_key`.
fn parse_http_config_key(key: &str) -> Option<(Option<&str>, &str)> {
    let first_dot = key.find('.')?;
    let section = &key[..first_dot];
    if !section.eq_ignore_ascii_case("http") {
        return None;
    }
    let rest = &key[first_dot + 1..];
    if let Some(last_dot) = rest.rfind('.') {
        let subsection = &rest[..last_dot];
        let variable = &rest[last_dot + 1..];
        if subsection.is_empty() || variable.is_empty() {
            None
        } else {
            Some((Some(subsection), variable))
        }
    } else if rest.is_empty() {
        None
    } else {
        Some((None, rest))
    }
}

// ---------------------------------------------------------------------------
// Cookies (`http.cookieFile` / `http.saveCookies`) — lifted from http_client.rs.
// ---------------------------------------------------------------------------

/// A single parsed cookie that may be sent on matching requests.
#[derive(Clone, Debug)]
struct CookieSpec {
    name_value: String,
    domain: Option<String>,
    include_subdomains: bool,
    path: Option<String>,
    secure: bool,
    expires_at: Option<i64>,
    host_only_origin: Option<String>,
}

impl CookieSpec {
    /// Whether this cookie should be sent on a request to `url`. Lifts the CLI's
    /// `CookieSpec::matches_url`: honors expiry, `secure` (https-only), the
    /// domain (with optional subdomain inclusion), and the path prefix.
    fn matches_url(&self, url: Option<&url::Url>, hostonly_scope: Option<&str>) -> bool {
        if self.is_expired() {
            return false;
        }
        let Some(url) = url else {
            return false;
        };
        if self.secure && url.scheme() != "https" {
            return false;
        }
        if self.domain.is_none() {
            let bind = self.host_only_origin.as_deref().or(hostonly_scope);
            if let Some(origin) = bind {
                if !http_origins_match(origin, url.as_str()) {
                    return false;
                }
            }
        } else if let Some(domain) = self.domain.as_deref() {
            let Some(host) = url.host_str() else {
                return false;
            };
            if self.include_subdomains {
                if host != domain && !host.ends_with(&format!(".{domain}")) {
                    return false;
                }
            } else if host != domain {
                return false;
            }
        }
        if let Some(path) = self.path.as_deref() {
            if !url.path().starts_with(path) {
                return false;
            }
        }
        true
    }

    fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expiry| time::OffsetDateTime::now_utc().unix_timestamp() >= expiry)
    }
}

/// Parse the configured cookie file into [`CookieSpec`]s. Lifts the CLI's
/// `build_cookie_specs` / `read_cookie_file_lines`.
fn build_cookie_specs(path: Option<&std::path::Path>) -> Result<Vec<CookieSpec>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read_to_string(path)
        .map_err(|e| Error::Message(format!("read cookie file '{}': {e}", path.display())))?;
    let mut out = Vec::new();
    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(spec) = parse_cookie_spec(trimmed) {
            out.push(spec);
        }
    }
    Ok(out)
}

fn parse_cookie_spec(line: &str) -> Option<CookieSpec> {
    parse_netscape_cookie(line).or_else(|| parse_header_cookie(line))
}

/// Parse a tab-separated Netscape `cookies.txt` line. Lifts the CLI's
/// `parse_netscape_cookie`.
fn parse_netscape_cookie(line: &str) -> Option<CookieSpec> {
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() < 7 {
        return None;
    }
    let domain = cols[0].trim().trim_start_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    let include_subdomains =
        cols[1].trim().eq_ignore_ascii_case("TRUE") || cols[0].starts_with('.');
    let path = cols[2].trim();
    let secure = cols[3].trim().eq_ignore_ascii_case("TRUE");
    let expires_at = cols[4].trim().parse::<i64>().ok().filter(|v| *v > 0);
    let name = cols[5].trim();
    let value = cols[6].trim();
    if name.is_empty() {
        return None;
    }
    Some(CookieSpec {
        name_value: format!("{name}={value}"),
        domain: Some(domain),
        include_subdomains,
        path: (!path.is_empty()).then(|| path.to_string()),
        secure,
        expires_at,
        host_only_origin: None,
    })
}

/// Parse a `Set-Cookie:`-style header line. Lifts the CLI's `parse_header_cookie`.
fn parse_header_cookie(line: &str) -> Option<CookieSpec> {
    let raw = line
        .strip_prefix("Set-Cookie:")
        .or_else(|| line.strip_prefix("set-cookie:"))
        .unwrap_or(line)
        .trim();
    let mut parts = raw.split(';').map(str::trim);
    let name_value = parts.next()?.to_string();
    if !name_value.contains('=') {
        return None;
    }
    let mut cookie = CookieSpec {
        name_value,
        domain: None,
        include_subdomains: false,
        path: None,
        secure: false,
        expires_at: None,
        host_only_origin: None,
    };
    for attr in parts {
        if attr.eq_ignore_ascii_case("secure") {
            cookie.secure = true;
            continue;
        }
        if let Some((key, value)) = attr.split_once('=') {
            if key.eq_ignore_ascii_case("domain") {
                let domain = value.trim().trim_start_matches('.').to_ascii_lowercase();
                if !domain.is_empty() {
                    cookie.include_subdomains = true;
                    cookie.domain = Some(domain);
                }
            } else if key.eq_ignore_ascii_case("path") {
                let path = value.trim();
                if !path.is_empty() {
                    cookie.path = Some(path.to_string());
                }
            }
        }
    }
    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netscape_cookie_parses_and_matches() {
        let line = "example.com\tTRUE\t/\tFALSE\t0\tSID\tabc123";
        let spec = parse_cookie_spec(line).expect("parse netscape cookie");
        assert_eq!(spec.name_value, "SID=abc123");
        let url = url::Url::parse("http://example.com/repo.git/info/refs").unwrap();
        assert!(spec.matches_url(Some(&url), None));
        // Different host does not match.
        let other = url::Url::parse("http://other.test/").unwrap();
        assert!(!spec.matches_url(Some(&other), None));
    }

    #[test]
    fn secure_cookie_only_matches_https() {
        let line = "example.com\tTRUE\t/\tTRUE\t0\tS\tv";
        let spec = parse_cookie_spec(line).expect("parse");
        let http = url::Url::parse("http://example.com/").unwrap();
        let https = url::Url::parse("https://example.com/").unwrap();
        assert!(
            !spec.matches_url(Some(&http), None),
            "secure cookie must skip http"
        );
        assert!(
            spec.matches_url(Some(&https), None),
            "secure cookie matches https"
        );
    }

    #[test]
    fn host_only_cookie_file_cookie_respects_session_origin() {
        let line = "Set-Cookie: Foo=1";
        let spec = parse_cookie_spec(line).expect("parse header cookie");
        let gateway = url::Url::parse("http://127.0.0.1:8001/gateway/repo.git/info/refs").unwrap();
        let target = url::Url::parse("http://127.0.0.1:8002/secure/repo.git/info/refs").unwrap();
        let scope = "http://127.0.0.1:8001";
        assert!(spec.matches_url(Some(&gateway), Some(scope)));
        assert!(!spec.matches_url(Some(&target), Some(scope)));
    }

    #[test]
    fn header_cookie_parses_domain_and_path() {
        let line = "Set-Cookie: token=zzz; Domain=example.com; Path=/git; Secure";
        let spec = parse_cookie_spec(line).expect("parse header cookie");
        assert_eq!(spec.name_value, "token=zzz");
        assert_eq!(spec.domain.as_deref(), Some("example.com"));
        assert_eq!(spec.path.as_deref(), Some("/git"));
        assert!(spec.secure);
        // Subdomain inclusion (Domain attribute set).
        let sub = url::Url::parse("https://api.example.com/git/info/refs").unwrap();
        assert!(spec.matches_url(Some(&sub), None));
        // Path that does not match the prefix is rejected.
        let wrong_path = url::Url::parse("https://example.com/other").unwrap();
        assert!(!spec.matches_url(Some(&wrong_path), None));
    }

    #[test]
    fn expired_cookie_does_not_match() {
        let spec = CookieSpec {
            name_value: "x=1".to_owned(),
            domain: Some("example.com".to_owned()),
            include_subdomains: false,
            path: None,
            secure: false,
            expires_at: Some(1), // 1970 — long expired
            host_only_origin: None,
        };
        let url = url::Url::parse("http://example.com/").unwrap();
        assert!(!spec.matches_url(Some(&url), None));
    }

    #[test]
    fn extra_header_rules_apply_scoped_and_unscoped() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("http.extraHeader", "X-Global: g")
            .unwrap();
        cfg.add_command_override("http.https://example.com.extraHeader", "X-Scoped: s")
            .unwrap();
        let client = UreqHttpClient::from_config(&cfg).expect("from_config");
        let scoped = client.extra_headers_for_url("https://example.com/repo.git/info/refs");
        assert!(
            scoped.iter().any(|(n, v)| n == "X-Global" && v == "g"),
            "unscoped header should always apply: {scoped:?}"
        );
        assert!(
            scoped.iter().any(|(n, v)| n == "X-Scoped" && v == "s"),
            "URL-scoped header should apply on match: {scoped:?}"
        );
        // A non-matching URL gets only the global header.
        let off = client.extra_headers_for_url("https://other.test/repo.git/info/refs");
        assert!(off.iter().any(|(n, _)| n == "X-Global"));
        assert!(!off.iter().any(|(n, _)| n == "X-Scoped"));
    }

    #[test]
    fn empty_extra_header_resets_list() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("http.extraHeader", "X-One: 1")
            .unwrap();
        cfg.add_command_override("http.extraHeader", "").unwrap();
        cfg.add_command_override("http.extraHeader", "X-Two: 2")
            .unwrap();
        let client = UreqHttpClient::from_config(&cfg).expect("from_config");
        let h = client.extra_headers_for_url("https://example.com/");
        assert!(
            !h.iter().any(|(n, _)| n == "X-One"),
            "empty value must reset earlier headers: {h:?}"
        );
        assert!(h.iter().any(|(n, v)| n == "X-Two" && v == "2"));
    }

    #[test]
    fn socks_proxy_is_rejected_clearly() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("http.proxy", "socks5://localhost:1080")
            .unwrap();
        match UreqHttpClient::from_config(&cfg) {
            Ok(_) => panic!("SOCKS proxy must be rejected, not silently accepted"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("SOCKS"), "expected a clear SOCKS error: {msg}");
            }
        }
    }

    #[test]
    fn http_proxy_config_builds() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("http.proxy", "http://127.0.0.1:3128")
            .unwrap();
        // Building a client with an HTTP proxy must succeed (it just configures
        // the ureq agent; no connection is attempted here).
        UreqHttpClient::from_config(&cfg).expect("http proxy config builds");
    }

    #[test]
    fn empty_proxy_disables_proxying() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("http.proxy", "").unwrap();
        let proxy = build_proxy(&cfg).expect("empty proxy ok");
        assert!(proxy.is_none(), "empty http.proxy must disable proxying");
    }

    #[test]
    fn credentials_omit_http_path_by_default() {
        let cfg = ConfigSet::new();
        let cred = credential_for_url(
            "https://github.com/owner/repo.git/info/refs?service=git-upload-pack",
            &cfg,
        )
        .expect("credential target");

        assert_eq!(cred.protocol.as_deref(), Some("https"));
        assert_eq!(cred.host.as_deref(), Some("github.com"));
        assert_eq!(cred.path, None);
    }

    #[test]
    fn credentials_include_http_path_when_configured() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("credential.useHttpPath", "true")
            .unwrap();
        let cred = credential_for_url(
            "https://github.com/owner/repo.git/info/refs?service=git-upload-pack",
            &cfg,
        )
        .expect("credential target");

        assert_eq!(cred.path.as_deref(), Some("owner/repo.git/info/refs"));
    }

    #[test]
    fn credentials_include_http_path_when_url_scoped() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("credential.https://github.com/owner.useHttpPath", "true")
            .unwrap();
        let matching = credential_for_url(
            "https://github.com/owner/repo.git/info/refs?service=git-upload-pack",
            &cfg,
        )
        .expect("credential target");
        let other = credential_for_url(
            "https://github.com/elsewhere/repo.git/info/refs?service=git-upload-pack",
            &cfg,
        )
        .expect("credential target");

        assert_eq!(matching.path.as_deref(), Some("owner/repo.git/info/refs"));
        assert_eq!(other.path, None);
    }
}
