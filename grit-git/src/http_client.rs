//! Shared HTTP(S) client for smart HTTP transport: `http.proxy`, `GIT_ASKPASS`, and `GIT_TRACE_CURL`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};
use base64::Engine;
use flate2::write::GzEncoder;
use flate2::Compression;
use grit_lib::config::{parse_bool, parse_i64, ConfigSet};
use grit_lib::transport::http::{
    effective_info_refs_url_after_redirect, http_origin_key, http_origins_match,
};
use url::Url;

/// Pre-built ureq agent or SOCKS-over-Unix tunnel for `http.proxy`.
#[derive(Clone)]
pub struct HttpClientContext {
    transport: Transport,
    trace_curl: Option<TraceCurl>,
    proxy_raw: Option<String>,
    proxy_auth_method: ProxyAuthMethod,
    ssl_verify: bool,
    git_protocol_header: Option<String>,
    post_buffer: usize,
    credential_use_http_path: bool,
    credential_username: Option<String>,
    cookies: Vec<CookieSpec>,
    cookie_file_path: Option<PathBuf>,
    save_cookies: bool,
    extra_headers: Vec<ExtraHeaderRule>,
    smart_http_enabled: bool,
    proactive_auth: ProactiveAuth,
    empty_auth: bool,
    auth_entry: Arc<Mutex<Option<CachedHttpAuth>>>,
    /// Origin of the first HTTP request in this client session; host-only cookies
    /// without a `Domain` attribute are sent only on this origin (not after a
    /// cross-authority redirect).
    hostonly_cookie_scope: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
struct CachedHttpAuth {
    credentials: AuthCredentials,
    origin: String,
}

#[derive(Clone)]
enum Transport {
    Ureq(ureq::Agent),
    /// RFC 7230 absolute-form requests through an HTTP proxy (`GET http://host/...`).
    HttpForward {
        proxy_host: String,
        proxy_port: u16,
        proxy_basic: Option<String>,
    },
    SocksUnix {
        socket_path: PathBuf,
    },
}

#[derive(Clone)]
struct TraceCurl {
    path: TraceCurlDest,
    components: String,
    redact: bool,
}

#[derive(Clone)]
enum TraceCurlDest {
    Stderr,
    File(String),
}

#[derive(Clone)]
struct ExtraHeaderRule {
    pattern: Option<String>,
    header: Option<(String, String)>,
}

#[derive(Clone, Debug)]
struct CookieSpec {
    name_value: String,
    domain: Option<String>,
    include_subdomains: bool,
    path: Option<String>,
    secure: bool,
    expires_at: Option<i64>,
    /// When set, a host-only cookie (no `Domain` attribute) is limited to this origin.
    host_only_origin: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProactiveAuth {
    None,
    Basic,
    Auto,
}

#[derive(Clone, Debug)]
enum ProxyAuthMethod {
    AnyAuth,
    Basic,
    Unsupported(String),
}

fn parse_proactive_auth(value: Option<String>) -> ProactiveAuth {
    match value.as_deref().map(str::trim).map(str::to_ascii_lowercase) {
        Some(value) if value == "basic" => ProactiveAuth::Basic,
        Some(value) if value == "auto" => ProactiveAuth::Auto,
        _ => ProactiveAuth::None,
    }
}

fn proxy_auth_method(config: &ConfigSet) -> ProxyAuthMethod {
    let raw = std::env::var("GIT_HTTP_PROXY_AUTHMETHOD")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.get("http.proxyAuthMethod"))
        .unwrap_or_else(|| "anyauth".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "anyauth" => ProxyAuthMethod::AnyAuth,
        "basic" => ProxyAuthMethod::Basic,
        other => ProxyAuthMethod::Unsupported(other.to_string()),
    }
}

fn ensure_supported_proxy_auth_method(method: &ProxyAuthMethod, proxy_url: &Url) -> Result<()> {
    if proxy_url.username().is_empty() {
        return Ok(());
    }
    match method {
        ProxyAuthMethod::AnyAuth | ProxyAuthMethod::Basic => Ok(()),
        ProxyAuthMethod::Unsupported(method) => {
            bail!("unsupported HTTP proxy authentication method '{method}'")
        }
    }
}

impl CookieSpec {
    fn matches_url(&self, url: Option<&Url>, hostonly_scope: Option<&str>) -> bool {
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

/// Validate `http.proxy` from `git clone -c http.proxy=...` before clap runs, so invalid URLs
/// fail with Git-shaped stderr even when other arguments confuse the parser (t5564).
pub fn validate_clone_proxy_from_argv(rest: &[String]) -> Result<()> {
    if let Some(v) = last_command_line_config_value(rest, "http.proxy") {
        validate_proxy_url(&v)?;
    }
    Ok(())
}

fn last_command_line_config_value(rest: &[String], want_key: &str) -> Option<String> {
    let mut out = None;
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "-c" && i + 1 < rest.len() {
            let entry = &rest[i + 1];
            if let Some((k, v)) = entry.split_once('=') {
                if k.trim() == want_key {
                    out = Some(v.trim().to_string());
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

fn parse_protocol_version(value: &str) -> Option<u8> {
    match value.trim() {
        "0" => Some(0),
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

fn resolve_git_protocol_header(config: &ConfigSet) -> Option<String> {
    let version = config
        .get("protocol.version")
        .as_deref()
        .and_then(parse_protocol_version)
        .unwrap_or_else(crate::protocol_wire::effective_client_protocol_version);
    match version {
        0 => None,
        1 => Some("version=1".to_string()),
        _ => Some("version=2".to_string()),
    }
}

impl HttpClientContext {
    /// Build transport from merged Git config (`http.proxy`, etc.).
    pub fn from_config_set(config: &ConfigSet) -> Result<Self> {
        Self::from_config_set_with_proxy_override(config, None)
    }

    /// Build transport from merged Git config with an optional per-remote proxy override.
    pub fn from_config_set_with_proxy_override(
        config: &ConfigSet,
        proxy_override: Option<String>,
    ) -> Result<Self> {
        let trace_curl = trace_curl_from_env();
        let proxy_raw = proxy_override
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config.get("http.proxy"));
        let proxy_auth_method = proxy_auth_method(config);
        let transport = build_transport(config, &proxy_auth_method, proxy_raw.as_deref())?;
        let ssl_verify = ssl_verify_enabled(config);
        let git_protocol_header = resolve_git_protocol_header(config);
        let post_buffer = config
            .get("http.postBuffer")
            .as_deref()
            .and_then(|v| parse_i64(v).ok())
            .filter(|v| *v > 0)
            .map_or(1024 * 1024, |v| usize::try_from(v).unwrap_or(1024 * 1024));
        let credential_use_http_path = config
            .get("credential.useHttpPath")
            .as_deref()
            .map(|v| parse_bool(v).unwrap_or(false))
            .unwrap_or(false);
        let credential_username = config
            .get("credential.username")
            .filter(|s| !s.trim().is_empty());
        let cookie_file_path = config
            .get("http.cookieFile")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let cookies = build_cookie_specs(config)?;
        let save_cookies = cookie_file_path.is_some()
            && config
                .get("http.saveCookies")
                .as_deref()
                .map(|value| parse_bool(value).unwrap_or(false))
                .unwrap_or(false);
        let extra_headers = extra_header_rules_from_config(config);
        let smart_http_enabled = std::env::var("GIT_SMART_HTTP")
            .ok()
            .is_none_or(|v| v.trim() != "0");
        let proactive_auth = parse_proactive_auth(config.get("http.proactiveAuth"));
        let empty_auth = config
            .get("http.emptyAuth")
            .as_deref()
            .map(|value| parse_bool(value).unwrap_or(false))
            .unwrap_or(false);
        Ok(Self {
            transport,
            trace_curl,
            proxy_raw,
            proxy_auth_method,
            ssl_verify,
            git_protocol_header,
            post_buffer,
            credential_use_http_path,
            credential_username,
            cookies,
            cookie_file_path,
            save_cookies,
            extra_headers,
            smart_http_enabled,
            proactive_auth,
            empty_auth,
            auth_entry: Arc::new(Mutex::new(None)),
            hostonly_cookie_scope: Arc::new(Mutex::new(None)),
        })
    }

    /// Drop cached HTTP authorization after redirect re-basing (Git `credential_from_url`).
    pub(crate) fn reset_auth_after_redirect_rebase(&self) {
        self.clear_cached_auth();
    }

    /// Default agent (no proxy, trace from environment only).
    pub fn default_agent() -> Result<Self> {
        Self::from_config_set(&ConfigSet::new())
    }

    /// Whether smart HTTP should be used for discovery/rpc.
    #[must_use]
    pub fn smart_http_enabled(&self) -> bool {
        self.smart_http_enabled
    }

    /// Return the configured `Git-Protocol` request header value for this context.
    ///
    /// Returns `None` when protocol v0 is selected and the header should be suppressed.
    #[must_use]
    pub fn git_protocol_header(&self) -> Option<&str> {
        self.git_protocol_header.as_deref()
    }

    /// Perform GET, returning the response body. Fails on HTTP status >= 400.
    pub fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.get_with_git_protocol(url, self.git_protocol_header.as_deref())
    }

    /// Perform GET with an explicit `Git-Protocol` header override.
    ///
    /// Passing `None` suppresses the header entirely for the request.
    pub fn get_with_git_protocol(
        &self,
        url: &str,
        git_protocol_header: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.get_raw_with_git_protocol(url, git_protocol_header)
            .map(|resp| resp.body)
    }

    /// Like [`get_with_git_protocol`](Self::get_with_git_protocol), but also
    /// returns the final URL the request resolved to after any followed HTTP
    /// redirects (`None` when the transport does not report it). Callers use
    /// this to re-base subsequent smart-HTTP POSTs onto a redirected
    /// `info/refs` location (Git's `http.followRedirects` behavior).
    pub fn get_with_final_url(
        &self,
        url: &str,
        git_protocol_header: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        self.get_raw_with_git_protocol(url, git_protocol_header)
            .map(|resp| (resp.body, resp.final_url))
    }

    fn get_raw_with_git_protocol(
        &self,
        url: &str,
        git_protocol_header: Option<&str>,
    ) -> Result<RawHttpResponse> {
        self.trace_proxy_auth_header();
        self.trace_request_start("GET", url, self.smart_http_enabled);
        if let Some(v) = git_protocol_header {
            self.trace_outgoing_header(&format!("Git-Protocol: {v}"));
        }
        let cookie_header = self.cookie_header_for_url(url);
        self.trace_cookie_header(cookie_header.as_deref());
        let extra_headers = self.extra_headers_for_url(url);
        self.trace_extra_headers(&extra_headers);
        let request_auth = match self.cached_authorization_header_for_url(url) {
            Some(header) => Some(header),
            None => self.proactive_authorization_header(url)?,
        };
        let first = self.http_get_once(url, request_auth.as_deref(), git_protocol_header)?;
        self.save_response_cookies(&first)?;
        self.trace_response_status(first.status, &first.reason);
        if first.status != 401 {
            if let Some(final_url) = first.final_url.as_deref() {
                self.invalidate_auth_on_cross_origin_redirect(url, final_url);
            }
            if first.status >= 400 {
                return Err(http_access_error(url, first.status));
            }
            self.approve_cached_auth_for_url(url);
            return Ok(first);
        }
        let auth_challenges = first.www_authenticate_challenges();
        let auth_url = effective_info_refs_url_after_redirect(url, first.final_url.as_deref());

        let mut auth = self
            .credentials_from_fill(&auth_url, &auth_challenges)?
            .unwrap_or(self.default_auth_for_url(&auth_url)?);
        if auth.needs_basic_prompt() && !self.empty_auth {
            let mut username = auth.username().unwrap_or_default().to_string();
            if username.is_empty() {
                username = self.askpass_username(&auth_url)?;
            }
            let password = self.askpass_password(&auth_url, &username)?;
            auth = AuthCredentials::Basic { username, password };
        }

        let auth_header = auth.authorization_header();
        self.trace_auth_header(&auth_header);
        let retry = self.http_get_once(&auth_url, Some(&auth_header), git_protocol_header)?;
        self.save_response_cookies(&retry)?;
        let mut credential_input = self.credential_input_for_url(&auth_url)?;
        auth.add_to_credential_input(&mut credential_input);
        let mut reject_extras = auth.credential_extras();
        reject_extras.extend(credential_challenge_extras(&auth_challenges));
        self.trace_response_status(retry.status, &retry.reason);
        if retry.status == 401 && auth.should_continue() {
            let next_challenges = retry.www_authenticate_challenges();
            if let Some(next_auth) =
                self.credentials_from_fill_continue(&auth_url, &next_challenges, &auth)?
            {
                let next_auth_header = next_auth.authorization_header();
                self.trace_auth_header(&next_auth_header);
                let retry2 =
                    self.http_get_once(&auth_url, Some(&next_auth_header), git_protocol_header)?;
                self.save_response_cookies(&retry2)?;
                let mut credential_input = self.credential_input_for_url(&auth_url)?;
                next_auth.add_to_credential_input(&mut credential_input);
                let mut reject_extras = next_auth.credential_extras();
                reject_extras.extend(credential_challenge_extras(&next_challenges));
                self.trace_response_status(retry2.status, &retry2.reason);
                if retry2.status >= 400 {
                    let _ = self.run_credential_action("reject", &credential_input, &reject_extras);
                    self.clear_cached_auth();
                    return Err(http_access_error(&auth_url, retry2.status));
                }
                let approve_extras = next_auth.credential_extras();
                let _ = self.run_credential_action("approve", &credential_input, &approve_extras);
                self.store_cached_auth(next_auth, &auth_url);
                return Ok(retry2);
            }
        }
        if retry.status >= 400 {
            let _ = self.run_credential_action("reject", &credential_input, &reject_extras);
            self.clear_cached_auth();
            return Err(http_access_error(&auth_url, retry.status));
        }
        let approve_extras = auth.credential_extras();
        let _ = self.run_credential_action("approve", &credential_input, &approve_extras);
        if retry
            .final_url
            .as_deref()
            .is_none_or(|final_url| http_origins_match(&auth_url, final_url))
        {
            self.store_cached_auth(auth, &auth_url);
        } else {
            self.clear_cached_auth();
        }
        Ok(retry)
    }

    /// Perform POST with given headers, returning the body.
    pub fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        self.post_with_git_protocol(
            url,
            content_type,
            accept,
            body,
            self.git_protocol_header.as_deref(),
        )
    }

    /// Perform POST with an explicit `Git-Protocol` header override.
    ///
    /// Passing `None` suppresses the header entirely for the request.
    pub fn post_with_git_protocol(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol_header: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.trace_proxy_auth_header();
        self.trace_request_start("POST", url, self.smart_http_enabled);
        self.trace_outgoing_header(&format!("Content-Type: {content_type}"));
        self.trace_outgoing_header(&format!("Accept: {accept}"));
        if let Some(v) = git_protocol_header {
            self.trace_outgoing_header(&format!("Git-Protocol: {v}"));
        }
        let cookie_header = self.cookie_header_for_url(url);
        self.trace_cookie_header(cookie_header.as_deref());
        let extra_headers = self.extra_headers_for_url(url);
        self.trace_extra_headers(&extra_headers);
        let (payload, gzip_enabled) = self.encode_post_payload(body)?;
        if gzip_enabled {
            self.trace_outgoing_header("Content-Encoding: gzip");
        }

        let chunked = payload.len() > self.post_buffer;
        if chunked {
            self.trace_outgoing_header("Transfer-Encoding: chunked");
        } else {
            self.trace_outgoing_header(&format!("Content-Length: {}", payload.len()));
        }
        self.trace_rpc_post_size(url, payload.len(), chunked);

        let request_auth = match self.cached_authorization_header_for_url(url) {
            Some(header) => Some(header),
            None => self.proactive_authorization_header(url)?,
        };
        let first = self.http_post_once(
            url,
            content_type,
            accept,
            &payload,
            request_auth.as_deref(),
            gzip_enabled,
            chunked,
            git_protocol_header,
        )?;
        self.save_response_cookies(&first)?;
        self.trace_response_status(first.status, &first.reason);
        if first.status != 401 {
            if let Some(final_url) = first.final_url.as_deref() {
                self.invalidate_auth_on_cross_origin_redirect(url, final_url);
            }
            if first.status >= 400 {
                return Err(http_access_error(url, first.status));
            }
            self.approve_cached_auth_for_url(url);
            return Ok(first.body);
        }
        let auth_challenges = first.www_authenticate_challenges();

        let mut auth = self
            .credentials_from_fill(url, &auth_challenges)?
            .unwrap_or(self.default_auth_for_url(url)?);
        if auth.needs_basic_prompt() && !self.empty_auth {
            let mut username = auth.username().unwrap_or_default().to_string();
            if username.is_empty() {
                username = self.askpass_username(url)?;
            }
            let password = self.askpass_password(url, &username)?;
            auth = AuthCredentials::Basic { username, password };
        }
        let auth_header = auth.authorization_header();
        self.trace_auth_header(&auth_header);

        let retry = self.http_post_once(
            url,
            content_type,
            accept,
            &payload,
            Some(&auth_header),
            gzip_enabled,
            chunked,
            git_protocol_header,
        )?;
        self.save_response_cookies(&retry)?;
        let mut credential_input = self.credential_input_for_url(url)?;
        auth.add_to_credential_input(&mut credential_input);
        let mut reject_extras = auth.credential_extras();
        reject_extras.extend(credential_challenge_extras(&auth_challenges));
        self.trace_response_status(retry.status, &retry.reason);
        if retry.status == 401 && auth.should_continue() {
            let next_challenges = retry.www_authenticate_challenges();
            if let Some(next_auth) =
                self.credentials_from_fill_continue(url, &next_challenges, &auth)?
            {
                let next_auth_header = next_auth.authorization_header();
                self.trace_auth_header(&next_auth_header);
                let retry2 = self.http_post_once(
                    url,
                    content_type,
                    accept,
                    &payload,
                    Some(&next_auth_header),
                    gzip_enabled,
                    chunked,
                    git_protocol_header,
                )?;
                self.save_response_cookies(&retry2)?;
                let mut credential_input = self.credential_input_for_url(url)?;
                next_auth.add_to_credential_input(&mut credential_input);
                let mut reject_extras = next_auth.credential_extras();
                reject_extras.extend(credential_challenge_extras(&next_challenges));
                self.trace_response_status(retry2.status, &retry2.reason);
                if retry2.status >= 400 {
                    let _ = self.run_credential_action("reject", &credential_input, &reject_extras);
                    self.clear_cached_auth();
                    return Err(http_access_error(url, retry2.status));
                }
                let approve_extras = next_auth.credential_extras();
                let _ = self.run_credential_action("approve", &credential_input, &approve_extras);
                self.store_cached_auth(next_auth, url);
                return Ok(retry2.body);
            }
        }
        if retry.status >= 400 {
            let _ = self.run_credential_action("reject", &credential_input, &reject_extras);
            self.clear_cached_auth();
            return Err(http_access_error(url, retry.status));
        }
        let approve_extras = auth.credential_extras();
        let _ = self.run_credential_action("approve", &credential_input, &approve_extras);
        if retry
            .final_url
            .as_deref()
            .is_none_or(|final_url| http_origins_match(url, final_url))
        {
            self.store_cached_auth(auth, url);
        } else {
            self.clear_cached_auth();
        }
        Ok(retry.body)
    }

    fn http_get_once(
        &self,
        url: &str,
        auth_header: Option<&str>,
        git_protocol_header: Option<&str>,
    ) -> Result<RawHttpResponse> {
        let request_url = discovery_url_for_mode(url, self.smart_http_enabled);
        let extra_headers = self.extra_headers_for_url(&request_url);
        let cookie_header = self.cookie_header_for_url(&request_url);
        let env_transport = if self.proxy_raw.is_none() {
            env_proxy_for_url(&request_url)
                .map(|proxy| {
                    build_transport_from_proxy(&proxy, self.ssl_verify, &self.proxy_auth_method)
                })
                .transpose()?
        } else {
            None
        };
        let transport = env_transport.as_ref().unwrap_or(&self.transport);
        match transport {
            Transport::Ureq(agent) => {
                let mut req = agent
                    .get(&request_url)
                    .header("User-Agent", &crate::http_smart::agent_header());
                if let Some(v) = git_protocol_header {
                    req = req.header("Git-Protocol", v);
                }
                if let Some(cookie) = cookie_header.as_deref() {
                    req = req.header("Cookie", cookie);
                }
                if let Some(v) = auth_header {
                    req = req.header("Authorization", v);
                }
                for (name, value) in &extra_headers {
                    req = req.header(name, value);
                }
                // The agent is configured with `http_status_as_error(false)`, so
                // >= 400 responses arrive as `Ok`; only genuine transport errors
                // are `Err`.
                match req.call() {
                    Ok(resp) => raw_response_from_ureq(resp, "GET", true),
                    Err(err) => Err(http_request_error("GET", &request_url, err)),
                }
            }
            Transport::HttpForward {
                proxy_host,
                proxy_port,
                proxy_basic,
            } => {
                let req = build_proxy_get_request(
                    &request_url,
                    proxy_basic.as_deref(),
                    auth_header,
                    git_protocol_header,
                    cookie_header.as_deref(),
                    &extra_headers,
                    self.smart_http_enabled,
                )?;
                http_over_tcp_forward(proxy_host, *proxy_port, &req)
            }
            Transport::SocksUnix { socket_path } => {
                let req = build_get_request(
                    &request_url,
                    auth_header,
                    git_protocol_header,
                    cookie_header.as_deref(),
                    &extra_headers,
                    self.smart_http_enabled,
                )?;
                http_over_socks_unix(socket_path, &request_url, &req)
            }
        }
    }

    fn http_post_once(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        auth_header: Option<&str>,
        gzip_enabled: bool,
        chunked: bool,
        git_protocol_header: Option<&str>,
    ) -> Result<RawHttpResponse> {
        let request_url = discovery_url_for_mode(url, self.smart_http_enabled);
        let extra_headers = self.extra_headers_for_url(&request_url);
        let cookie_header = self.cookie_header_for_url(&request_url);
        let env_transport = if self.proxy_raw.is_none() {
            env_proxy_for_url(&request_url)
                .map(|proxy| {
                    build_transport_from_proxy(&proxy, self.ssl_verify, &self.proxy_auth_method)
                })
                .transpose()?
        } else {
            None
        };
        let transport = env_transport.as_ref().unwrap_or(&self.transport);
        match transport {
            Transport::Ureq(agent) => {
                let mut req = agent
                    .post(&request_url)
                    .header("Content-Type", content_type)
                    .header("Accept", accept)
                    .header("User-Agent", &crate::http_smart::agent_header());
                if let Some(v) = git_protocol_header {
                    req = req.header("Git-Protocol", v);
                }
                if gzip_enabled {
                    req = req.header("Content-Encoding", "gzip");
                }
                if let Some(cookie) = cookie_header.as_deref() {
                    req = req.header("Cookie", cookie);
                }
                if let Some(v) = auth_header {
                    req = req.header("Authorization", v);
                }
                for (name, value) in &extra_headers {
                    req = req.header(name, value);
                }
                // Sending from a reader (unknown length) makes ureq use chunked
                // transfer-encoding; a byte slice sends a fixed Content-Length.
                let send_result = if chunked {
                    let mut cur = std::io::Cursor::new(body);
                    req.send(ureq::SendBody::from_reader(&mut cur))
                } else {
                    req.send(body)
                };
                // `http_status_as_error(false)` => >= 400 arrives as `Ok`.
                match send_result {
                    Ok(resp) => raw_response_from_ureq(resp, "POST", false),
                    Err(err) => Err(http_request_error("POST", &request_url, err)),
                }
            }
            Transport::HttpForward {
                proxy_host,
                proxy_port,
                proxy_basic,
            } => {
                let req = build_proxy_post_request(
                    &request_url,
                    content_type,
                    accept,
                    body,
                    proxy_basic.as_deref(),
                    auth_header,
                    gzip_enabled,
                    chunked,
                    git_protocol_header,
                    cookie_header.as_deref(),
                    &extra_headers,
                    self.smart_http_enabled,
                )?;
                http_over_tcp_forward(proxy_host, *proxy_port, &req)
            }
            Transport::SocksUnix { socket_path } => {
                let req = build_post_request(
                    &request_url,
                    content_type,
                    accept,
                    body,
                    auth_header,
                    gzip_enabled,
                    chunked,
                    git_protocol_header,
                    cookie_header.as_deref(),
                    &extra_headers,
                    self.smart_http_enabled,
                )?;
                http_over_socks_unix(socket_path, &request_url, &req)
            }
        }
    }

    fn cached_authorization_header_for_url(&self, url: &str) -> Option<String> {
        let origin = http_origin_key(url)?;
        let guard = self
            .auth_entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .filter(|entry| entry.origin == origin)
            .map(|entry| entry.credentials.authorization_header())
    }

    fn cached_auth_credentials(&self) -> Option<AuthCredentials> {
        let guard = self
            .auth_entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.as_ref().map(|entry| entry.credentials.clone())
    }

    fn store_cached_auth(&self, auth: AuthCredentials, url: &str) {
        let Some(origin) = http_origin_key(url) else {
            return;
        };
        let mut guard = self
            .auth_entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(CachedHttpAuth {
            credentials: auth,
            origin,
        });
    }

    fn clear_cached_auth(&self) {
        let mut guard = self
            .auth_entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = None;
    }

    fn hostonly_cookie_scope_for_url(&self, url: &str) -> Option<String> {
        let origin = http_origin_key(url)?;
        let mut guard = self
            .hostonly_cookie_scope
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(origin.clone());
        }
        guard.clone()
    }

    fn invalidate_auth_on_cross_origin_redirect(&self, request_url: &str, final_url: &str) {
        if !http_origins_match(request_url, final_url) {
            self.clear_cached_auth();
        }
    }

    fn approve_cached_auth_for_url(&self, url: &str) {
        let Some(auth) = self.cached_auth_credentials() else {
            return;
        };
        let Ok(mut credential_input) = self.credential_input_for_url(url) else {
            return;
        };
        auth.add_to_credential_input(&mut credential_input);
        let approve_extras = auth.credential_extras();
        let _ = self.run_credential_action("approve", &credential_input, &approve_extras);
    }

    fn proactive_authorization_header(&self, url: &str) -> Result<Option<String>> {
        if self.empty_auth {
            let header = AuthCredentials::Basic {
                username: String::new(),
                password: String::new(),
            }
            .authorization_header();
            self.trace_auth_header(&header);
            return Ok(Some(header));
        }
        if self.proactive_auth == ProactiveAuth::None {
            return Ok(None);
        }
        let challenges = match self.proactive_auth {
            ProactiveAuth::Basic => vec!["Basic".to_string()],
            ProactiveAuth::Auto => Vec::new(),
            ProactiveAuth::None => return Ok(None),
        };
        let Some(auth) = self.credentials_from_fill(url, &challenges)? else {
            return Ok(None);
        };
        if auth.needs_basic_prompt() || auth.should_continue() {
            return Ok(None);
        }
        let header = auth.authorization_header();
        self.trace_auth_header(&header);
        self.store_cached_auth(auth, url);
        Ok(Some(header))
    }

    fn encode_post_payload(&self, body: &[u8]) -> Result<(Vec<u8>, bool)> {
        if body.len() <= 1024 {
            return Ok((body.to_vec(), false));
        }
        let mut gz = GzEncoder::new(Vec::new(), Compression::best());
        gz.write_all(body).context("gzip request body")?;
        let payload = gz.finish().context("finalize gzip body")?;
        Ok((payload, true))
    }

    fn trace_auth_header(&self, header: &str) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        if t.redact {
            let scheme = header
                .split_once(' ')
                .map_or("Authorization", |(scheme, _)| scheme);
            t.write_line(&format!(
                "=> Send header: Authorization: {scheme} <redacted>\n"
            ));
        } else {
            t.write_line(&format!("=> Send header: Authorization: {header}\n"));
        }
    }

    fn credential_input_for_url(&self, url: &str) -> Result<BTreeMap<String, String>> {
        let parsed = Url::parse(url).with_context(|| format!("bad URL {url}"))?;
        let mut input = BTreeMap::new();
        // `url=` matches Git and lets helpers (e.g. git-credential-osxkeychain) keychain lookup
        // the same way as `git credential fill`.
        let mut cred_lookup_url = parsed.clone();
        let _ = cred_lookup_url.set_password(None);
        input.insert("url".to_string(), cred_lookup_url.to_string());
        input.insert("protocol".to_string(), parsed.scheme().to_string());
        let host = host_header_value(&parsed);
        input.insert("host".to_string(), host);
        if self.credential_use_http_path {
            if let Some(path) = credential_prompt_path(parsed.path()) {
                input.insert("path".to_string(), path);
            }
        }
        if let Some(user) = self
            .credential_username
            .as_deref()
            .filter(|u| !u.is_empty())
        {
            input.insert("username".to_string(), user.to_string());
        } else if !parsed.username().is_empty() {
            input.insert("username".to_string(), parsed.username().to_string());
        }
        Ok(input)
    }

    fn run_credential_action(
        &self,
        action: &str,
        input: &BTreeMap<String, String>,
        extras: &[(String, String)],
    ) -> Result<BTreeMap<String, String>> {
        let exe = std::env::current_exe().context("resolve current executable for credential")?;
        let mut child = Command::new(exe)
            .arg("credential")
            .arg(action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn credential {action}"))?;
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("credential {action}: missing stdin"))?;
            for (k, v) in extras {
                writeln!(stdin, "{k}={v}")?;
            }
            for (k, v) in input {
                writeln!(stdin, "{k}={v}")?;
            }
            writeln!(stdin)?;
        }
        let out = child
            .wait_with_output()
            .with_context(|| format!("wait credential {action}"))?;
        if !out.status.success() {
            bail!("credential {action} exited with status {}", out.status);
        }
        let mut map = BTreeMap::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        Ok(map)
    }

    fn credentials_from_fill(
        &self,
        url: &str,
        auth_challenges: &[String],
    ) -> Result<Option<AuthCredentials>> {
        let input = self.credential_input_for_url(url)?;
        let extras = credential_fill_extras(auth_challenges);
        let filled = self.run_credential_action("fill", &input, &extras)?;
        if let (Some(authtype), Some(credential)) =
            (filled.get("authtype"), filled.get("credential"))
        {
            if !authtype.is_empty() && !credential.is_empty() {
                return Ok(Some(AuthCredentials::preencoded_from_fields(
                    authtype.clone(),
                    credential.clone(),
                    filled.get("username").cloned(),
                    filled
                        .get("ephemeral")
                        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on")),
                    filled.get("state[]").cloned().into_iter().collect(),
                    filled
                        .get("continue")
                        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on")),
                )));
            }
        }
        let username = filled
            .get("username")
            .cloned()
            .or_else(|| input.get("username").cloned())
            .unwrap_or_default();
        let password = filled.get("password").cloned().unwrap_or_default();
        if username.is_empty() && password.is_empty() {
            return Ok(None);
        }
        Ok(Some(AuthCredentials::Basic { username, password }))
    }

    fn credentials_from_fill_continue(
        &self,
        url: &str,
        auth_challenges: &[String],
        previous: &AuthCredentials,
    ) -> Result<Option<AuthCredentials>> {
        let mut input = self.credential_input_for_url(url)?;
        previous.add_to_fill_input(&mut input);
        let mut extras = credential_fill_extras(auth_challenges);
        extras.extend(previous.state_extras());
        let filled = self.run_credential_action("fill", &input, &extras)?;
        if let (Some(authtype), Some(credential)) =
            (filled.get("authtype"), filled.get("credential"))
        {
            if !authtype.is_empty() && !credential.is_empty() {
                return Ok(Some(AuthCredentials::preencoded_from_fields(
                    authtype.clone(),
                    credential.clone(),
                    filled.get("username").cloned(),
                    filled
                        .get("ephemeral")
                        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on")),
                    filled.get("state[]").cloned().into_iter().collect(),
                    filled
                        .get("continue")
                        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on")),
                )));
            }
        }
        Ok(None)
    }

    fn default_auth_for_url(&self, url: &str) -> Result<AuthCredentials> {
        let input = self.credential_input_for_url(url)?;
        let username = input.get("username").cloned().unwrap_or_default();
        Ok(AuthCredentials::Basic {
            username,
            password: String::new(),
        })
    }

    fn askpass_username(&self, url: &str) -> Result<String> {
        let prompt = format!(
            "Username for '{}': ",
            credential_prompt_origin(url, self.credential_use_http_path)?
        );
        run_askpass(&prompt)
    }

    fn askpass_password(&self, url: &str, username: &str) -> Result<String> {
        let encoded_user: String =
            url::form_urlencoded::byte_serialize(username.as_bytes()).collect();
        let prompt = format!(
            "Password for '{}://{}@{}': ",
            credential_prompt_scheme(url)?,
            encoded_user,
            credential_prompt_host(url, self.credential_use_http_path)?
        );
        run_askpass(&prompt)
    }

    fn trace_request_start(&self, method: &str, url: &str, smart_http_enabled: bool) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        let shown_url = discovery_url_for_mode(url, smart_http_enabled);
        let shown_url = if t.redact {
            scrub_url_credentials(&shown_url)
        } else {
            shown_url
        };
        t.write_line(&format!("=> Send header: {method} {shown_url} HTTP/1.1\n"));
    }

    fn trace_response_status(&self, status: u16, text: &str) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        t.write_line(&format!("<= Recv header: HTTP/1.1 {status} {text}\n"));
    }

    fn trace_rpc_post_size(&self, url: &str, len: usize, chunked: bool) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        let service = if url.ends_with("/git-receive-pack") {
            Some("git-receive-pack")
        } else if url.ends_with("/git-upload-pack") {
            Some("git-upload-pack")
        } else {
            None
        };
        let Some(service) = service else {
            return;
        };
        if chunked {
            t.write_line(&format!("== Info: POST {service} (chunked)\n"));
        } else {
            t.write_line(&format!("== Info: POST {service} ({len} bytes)\n"));
        }
    }

    fn trace_outgoing_header(&self, line: &str) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        t.write_line(&format!("=> Send header: {line}\n"));
    }

    fn cookie_header_for_url(&self, url: &str) -> Option<String> {
        if self.cookies.is_empty() {
            return None;
        }
        let scope = self.hostonly_cookie_scope_for_url(url);
        let parsed = Url::parse(url).ok();
        let parts = self
            .cookies
            .iter()
            .filter(|cookie| cookie.matches_url(parsed.as_ref(), scope.as_deref()))
            .map(|cookie| cookie.name_value.clone())
            .collect::<Vec<_>>();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    fn trace_cookie_header(&self, cookie: Option<&str>) {
        let Some(cookie) = cookie else {
            return;
        };
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        if t.redact {
            let redacted = redact_cookie_header(cookie);
            t.write_line(&format!("=> Send header: Cookie: {redacted}\n"));
        } else {
            t.write_line(&format!("=> Send header: Cookie: {cookie}\n"));
        }
    }

    fn extra_headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        for rule in &self.extra_headers {
            let matches = rule
                .pattern
                .as_deref()
                .is_none_or(|pattern| grit_lib::config::url_matches(pattern, url));
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

    fn trace_extra_headers(&self, extra_headers: &[(String, String)]) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        for (name, value) in extra_headers {
            let rendered = if t.redact && header_should_redact(name) {
                format!("{name}: <redacted>")
            } else {
                format!("{name}: {value}")
            };
            t.write_line(&format!("=> Send header: {rendered}\n"));
        }
    }

    fn save_response_cookies(&self, response: &RawHttpResponse) -> Result<()> {
        if !self.save_cookies {
            return Ok(());
        }
        let Some(path) = self.cookie_file_path.as_ref() else {
            return Ok(());
        };
        let values = response
            .headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if values.is_empty() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create cookie directory {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open cookie file {}", path.display()))?;
        for value in values {
            writeln!(file, "Set-Cookie: {value}")?;
        }
        Ok(())
    }

    fn trace_proxy_auth_header(&self) {
        let Some(ref t) = self.trace_curl else {
            return;
        };
        if !trace_component_enabled(&t.components, "http") {
            return;
        }
        let Some(ref raw) = self.proxy_raw else {
            return;
        };
        let with_scheme = if raw.contains("://") {
            raw.clone()
        } else {
            format!("http://{raw}")
        };
        let Ok(parsed) = Url::parse(&with_scheme) else {
            return;
        };
        if parsed.scheme().to_ascii_lowercase().starts_with("socks") {
            return;
        }
        if parsed.username().is_empty() {
            return;
        }
        let line = if t.redact {
            "Proxy-Authorization: Basic <redacted>".to_string()
        } else if let Some(pass) = parsed.password() {
            let cred = format!("{}:{}", parsed.username(), pass);
            format!(
                "Proxy-Authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode(cred.as_bytes())
            )
        } else {
            "Proxy-Authorization: Basic <redacted>".to_string()
        };
        t.write_line(&format!("=> Send header: {line}\n"));
    }
}

impl TraceCurl {
    fn write_line(&self, line: &str) {
        match &self.path {
            TraceCurlDest::Stderr => {
                let mut l = std::io::stderr().lock();
                let _ = l.write_all(line.as_bytes());
                let _ = l.flush();
            }
            TraceCurlDest::File(p) => {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
                    let _ = f.write_all(line.as_bytes());
                    let _ = f.flush();
                    let _ = f.sync_all();
                }
            }
        }
    }
}

/// Capture a ureq 3 response into a [`RawHttpResponse`], reading the body.
///
/// The body is read unbounded (git packs can be large). `with_final_url` records
/// the post-redirect URL — GET discovery uses it to re-base subsequent POSTs;
/// POST callers pass `false`, matching prior behavior.
fn raw_response_from_ureq(
    resp: ureq::http::Response<ureq::Body>,
    what: &str,
    with_final_url: bool,
) -> Result<RawHttpResponse> {
    use ureq::ResponseExt as _;
    let status = resp.status().as_u16();
    // http::Response carries no server reason phrase; use the canonical text.
    let reason = resp.status().canonical_reason().unwrap_or("").to_string();
    let headers = response_headers(&resp);
    let final_url = with_final_url.then(|| resp.get_uri().to_string());
    let mut body = Vec::new();
    resp.into_body()
        .into_reader()
        .read_to_end(&mut body)
        .with_context(|| format!("read {what} body"))?;
    Ok(RawHttpResponse {
        status,
        reason,
        headers,
        body,
        final_url,
    })
}

fn response_headers(resp: &ureq::http::Response<ureq::Body>) -> Vec<(String, String)> {
    resp.headers()
        .keys()
        .flat_map(|name| {
            let key = name.as_str().to_ascii_lowercase();
            resp.headers()
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .map(move |value| (key.clone(), value.to_string()))
        })
        .collect()
}

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

fn header_should_redact(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("cookie")
}

struct RawHttpResponse {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The final URL the request resolved to after any HTTP redirects the
    /// transport followed (ureq follows redirects on GET). `None` when the
    /// transport does not report it (the manual proxy/SOCKS paths). Used to
    /// re-base subsequent smart-HTTP POSTs onto a redirected `info/refs`
    /// location, matching Git's `http.followRedirects` behavior.
    final_url: Option<String>,
}

impl RawHttpResponse {
    fn www_authenticate_challenges(&self) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("www-authenticate"))
            .map(|(_, value)| value.clone())
            .collect()
    }
}

#[derive(Clone)]
enum AuthCredentials {
    Basic {
        username: String,
        password: String,
    },
    Preencoded {
        authtype: String,
        credential: String,
        username: Option<String>,
        ephemeral: bool,
        state: Vec<String>,
        continue_auth: bool,
    },
}

impl AuthCredentials {
    fn preencoded_from_fields(
        authtype: String,
        credential: String,
        username: Option<String>,
        ephemeral: bool,
        state: Vec<String>,
        continue_auth: bool,
    ) -> Self {
        Self::Preencoded {
            authtype,
            credential,
            username,
            ephemeral,
            state,
            continue_auth,
        }
    }

    fn authorization_header(&self) -> String {
        match self {
            Self::Basic { username, password } => {
                let cred = format!("{username}:{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(cred.as_bytes());
                format!("Basic {encoded}")
            }
            Self::Preencoded {
                authtype,
                credential,
                ..
            } => format!("{authtype} {credential}"),
        }
    }

    fn username(&self) -> Option<&str> {
        match self {
            Self::Basic { username, .. } => Some(username),
            Self::Preencoded { username, .. } => username.as_deref(),
        }
    }

    fn should_continue(&self) -> bool {
        matches!(
            self,
            Self::Preencoded {
                continue_auth: true,
                ..
            }
        )
    }

    fn needs_basic_prompt(&self) -> bool {
        matches!(self, Self::Basic { username, password } if username.is_empty() || password.is_empty())
    }

    fn credential_extras(&self) -> Vec<(String, String)> {
        match self {
            Self::Basic { .. } => Vec::new(),
            Self::Preencoded { state, .. } => {
                let mut out = vec![("capability[]".to_string(), "authtype".to_string())];
                if !state.is_empty() {
                    out.push(("capability[]".to_string(), "state".to_string()));
                    out.extend(
                        state
                            .iter()
                            .map(|value| ("state[]".to_string(), value.clone())),
                    );
                }
                out
            }
        }
    }

    fn state_extras(&self) -> Vec<(String, String)> {
        match self {
            Self::Preencoded { state, .. } => state
                .iter()
                .map(|value| ("state[]".to_string(), value.clone()))
                .collect(),
            Self::Basic { .. } => Vec::new(),
        }
    }

    fn add_to_fill_input(&self, input: &mut BTreeMap<String, String>) {
        match self {
            Self::Preencoded {
                authtype, username, ..
            } => {
                input.insert("authtype".to_string(), authtype.clone());
                if let Some(username) = username {
                    input.insert("username".to_string(), username.clone());
                }
            }
            Self::Basic { username, .. } => {
                if !username.is_empty() {
                    input.insert("username".to_string(), username.clone());
                }
            }
        }
    }

    fn add_to_credential_input(&self, input: &mut BTreeMap<String, String>) {
        match self {
            Self::Basic { username, password } => {
                input.insert("username".to_string(), username.clone());
                input.insert("password".to_string(), password.clone());
            }
            Self::Preencoded {
                authtype,
                credential,
                username,
                ephemeral,
                ..
            } => {
                input.insert("authtype".to_string(), authtype.clone());
                input.insert("credential".to_string(), credential.clone());
                if let Some(username) = username {
                    input.insert("username".to_string(), username.clone());
                }
                if *ephemeral {
                    input.insert("ephemeral".to_string(), "1".to_string());
                }
            }
        }
    }
}

fn http_over_tcp_forward(host: &str, port: u16, req: &[u8]) -> Result<RawHttpResponse> {
    let mut sock = TcpStream::connect((host, port))
        .with_context(|| format!("connect to proxy {host}:{port}"))?;
    let _ = sock.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(120)));
    sock.write_all(req).context("write to proxy")?;
    sock.flush()?;
    read_http_response(&mut sock)
}

fn build_proxy_get_request(
    target_url: &str,
    proxy_basic: Option<&str>,
    auth_header: Option<&str>,
    git_protocol_header: Option<&str>,
    cookie_header: Option<&str>,
    extra_headers: &[(String, String)],
    smart_http_enabled: bool,
) -> Result<Vec<u8>> {
    let parsed = Url::parse(target_url).with_context(|| format!("bad URL {target_url}"))?;
    let host = host_header_value(&parsed);
    let request_url = discovery_url_for_mode(target_url, smart_http_enabled);
    let mut s = format!(
        "GET {request_url} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: {}\r\n\
         Connection: close\r\n\
         Accept: */*\r\n",
        crate::http_smart::agent_header()
    );
    if let Some(v) = git_protocol_header {
        s.push_str(&format!("Git-Protocol: {v}\r\n"));
    }
    if let Some(b) = proxy_basic {
        s.push_str(&format!("Proxy-Authorization: Basic {b}\r\n"));
    }
    if let Some(a) = auth_header {
        s.push_str(&format!("Authorization: {a}\r\n"));
    }
    if let Some(cookie) = cookie_header {
        s.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    for (name, value) in extra_headers {
        s.push_str(&format!("{name}: {value}\r\n"));
    }
    s.push_str("\r\n");
    Ok(s.into_bytes())
}

fn build_proxy_post_request(
    target_url: &str,
    content_type: &str,
    accept: &str,
    body: &[u8],
    proxy_basic: Option<&str>,
    auth_header: Option<&str>,
    gzip_enabled: bool,
    chunked: bool,
    git_protocol_header: Option<&str>,
    cookie_header: Option<&str>,
    extra_headers: &[(String, String)],
    smart_http_enabled: bool,
) -> Result<Vec<u8>> {
    let parsed = Url::parse(target_url).with_context(|| format!("bad URL {target_url}"))?;
    let host = host_header_value(&parsed);
    let request_url = discovery_url_for_mode(target_url, smart_http_enabled);
    let mut head = format!(
        "POST {request_url} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: {content_type}\r\n\
         Accept: {accept}\r\n\
         User-Agent: {}\r\n\
         Connection: close\r\n",
        crate::http_smart::agent_header()
    );
    if let Some(v) = git_protocol_header {
        head.push_str(&format!("Git-Protocol: {v}\r\n"));
    }
    if let Some(b) = proxy_basic {
        head.push_str(&format!("Proxy-Authorization: Basic {b}\r\n"));
    }
    if let Some(a) = auth_header {
        head.push_str(&format!("Authorization: {a}\r\n"));
    }
    if let Some(cookie) = cookie_header {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if gzip_enabled {
        head.push_str("Content-Encoding: gzip\r\n");
    }
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    if chunked {
        append_chunked_body(&mut out, body);
    } else {
        out.extend_from_slice(body);
    }
    Ok(out)
}

fn build_get_request(
    url: &str,
    auth_header: Option<&str>,
    git_protocol_header: Option<&str>,
    cookie_header: Option<&str>,
    extra_headers: &[(String, String)],
    smart_http_enabled: bool,
) -> Result<Vec<u8>> {
    let parsed = Url::parse(url).with_context(|| format!("bad URL {url}"))?;
    let path_q = url_path_and_query(&parsed);
    let host = host_header_value(&parsed);
    let request_path_q = discovery_url_for_mode(&path_q, smart_http_enabled);
    let mut s = format!(
        "GET {request_path_q} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {}\r\nConnection: close\r\nAccept: */*\r\n\r\n",
        crate::http_smart::agent_header()
    );
    if let Some(v) = git_protocol_header {
        let marker = "\r\n\r\n";
        if let Some(pos) = s.find(marker) {
            s.insert_str(pos, &format!("\r\nGit-Protocol: {v}"));
        }
    }
    if let Some(a) = auth_header {
        let marker = "\r\n\r\n";
        if let Some(pos) = s.find(marker) {
            s.insert_str(pos, &format!("\r\nAuthorization: {a}"));
        }
    }
    if let Some(cookie) = cookie_header {
        let marker = "\r\n\r\n";
        if let Some(pos) = s.find(marker) {
            s.insert_str(pos, &format!("\r\nCookie: {cookie}"));
        }
    }
    for (name, value) in extra_headers {
        let marker = "\r\n\r\n";
        if let Some(pos) = s.find(marker) {
            s.insert_str(pos, &format!("\r\n{name}: {value}"));
        }
    }
    Ok(s.into_bytes())
}

fn build_post_request(
    url: &str,
    content_type: &str,
    accept: &str,
    body: &[u8],
    auth_header: Option<&str>,
    gzip_enabled: bool,
    chunked: bool,
    git_protocol_header: Option<&str>,
    cookie_header: Option<&str>,
    extra_headers: &[(String, String)],
    smart_http_enabled: bool,
) -> Result<Vec<u8>> {
    let parsed = Url::parse(url).with_context(|| format!("bad URL {url}"))?;
    let path_q = url_path_and_query(&parsed);
    let host = host_header_value(&parsed);
    let request_path_q = discovery_url_for_mode(&path_q, smart_http_enabled);
    let mut head = format!(
        "POST {request_path_q} HTTP/1.1\r\nHost: {host}\r\nContent-Type: {content_type}\r\nAccept: {accept}\r\nUser-Agent: {}\r\nConnection: close\r\n\r\n",
        crate::http_smart::agent_header()
    );
    if let Some(v) = git_protocol_header {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, &format!("\r\nGit-Protocol: {v}"));
        }
    }
    if let Some(a) = auth_header {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, &format!("\r\nAuthorization: {a}"));
        }
    }
    if let Some(cookie) = cookie_header {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, &format!("\r\nCookie: {cookie}"));
        }
    }
    for (name, value) in extra_headers {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, &format!("\r\n{name}: {value}"));
        }
    }
    if gzip_enabled {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, "\r\nContent-Encoding: gzip");
        }
    }
    if chunked {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, "\r\nTransfer-Encoding: chunked");
        }
    } else {
        let marker = "\r\n\r\n";
        if let Some(pos) = head.find(marker) {
            head.insert_str(pos, &format!("\r\nContent-Length: {}", body.len()));
        }
    }
    let mut out = head.into_bytes();
    if chunked {
        append_chunked_body(&mut out, body);
    } else {
        out.extend_from_slice(body);
    }
    Ok(out)
}

fn append_chunked_body(out: &mut Vec<u8>, body: &[u8]) {
    if body.is_empty() {
        out.extend_from_slice(b"0\r\n\r\n");
        return;
    }
    const CHUNK: usize = 16 * 1024;
    let mut offset = 0usize;
    while offset < body.len() {
        let end = std::cmp::min(offset + CHUNK, body.len());
        let size = end - offset;
        out.extend_from_slice(format!("{size:x}\r\n").as_bytes());
        out.extend_from_slice(&body[offset..end]);
        out.extend_from_slice(b"\r\n");
        offset = end;
    }
    out.extend_from_slice(b"0\r\n\r\n");
}

fn credential_fill_extras(auth_challenges: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(2 + auth_challenges.len());
    out.push(("capability[]".to_string(), "authtype".to_string()));
    out.push(("capability[]".to_string(), "state".to_string()));
    out.extend(credential_challenge_extras(auth_challenges));
    out
}

fn credential_challenge_extras(auth_challenges: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(auth_challenges.len());
    for challenge in auth_challenges {
        out.push(("wwwauth[]".to_string(), challenge.clone()));
    }
    out
}

fn url_path_and_query(url: &Url) -> String {
    let mut p = url.path().to_string();
    if p.is_empty() {
        p.push('/');
    }
    if let Some(q) = url.query() {
        p.push('?');
        p.push_str(q);
    }
    p
}

fn host_header_value(url: &Url) -> String {
    let host = url.host_str().unwrap_or("localhost");
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

fn resolve_target_ipv4(url: &Url) -> Result<std::net::Ipv4Addr> {
    let host = url.host_str().context("URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}")
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}"))?
        .find(|a| matches!(a, std::net::SocketAddr::V4(_)))
        .context("no IPv4 address for host (SOCKS4 requires IPv4)")?;
    match addr {
        std::net::SocketAddr::V4(v4) => Ok(*v4.ip()),
        _ => bail!("expected IPv4"),
    }
}

#[cfg(unix)]
fn http_over_socks_unix(
    socket_path: &Path,
    target_url: &str,
    http_bytes: &[u8],
) -> Result<RawHttpResponse> {
    let url = Url::parse(target_url).with_context(|| format!("bad URL {target_url}"))?;
    let ip = resolve_target_ipv4(&url)?;
    let port = url
        .port_or_known_default()
        .context("URL missing port for SOCKS target")?;

    let mut sock = UnixStream::connect(socket_path)
        .with_context(|| format!("connect SOCKS unix socket {}", socket_path.display()))?;
    let _ = sock.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(120)));

    let mut req = Vec::with_capacity(9 + 1);
    req.push(4u8);
    req.push(1);
    req.extend_from_slice(&port.to_be_bytes());
    req.extend_from_slice(&ip.octets());
    req.push(0);

    sock.write_all(&req).context("SOCKS4 request")?;
    let mut reply = [0u8; 8];
    sock.read_exact(&mut reply).context("SOCKS4 reply")?;
    if reply[1] != 0x5a {
        bail!("SOCKS4 connection failed (reply {})", reply[1]);
    }

    trace_socks_granted_after_handshake();

    sock.write_all(http_bytes).context("write HTTP request")?;
    sock.flush()?;

    read_http_response(&mut sock)
}

#[cfg(not(unix))]
fn http_over_socks_unix(
    _socket_path: &Path,
    _target_url: &str,
    _http_bytes: &[u8],
) -> Result<RawHttpResponse> {
    bail!("SOCKS proxy over Unix socket is not supported on this platform")
}

fn read_http_response(r: &mut impl Read) -> Result<RawHttpResponse> {
    let mut reader = BufReader::new(r);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).context("read status")?;
    let status_line = status_line.trim_end_matches(['\r', '\n']);
    let mut parts = status_line.split_whitespace();
    let _http = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .context("bad HTTP status line")?;
    let reason = parts.collect::<Vec<_>>().join(" ");

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).context("read header")?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = headers.last_mut() {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let mut body = Vec::new();
    if let Some(cl) = headers.iter().find(|(k, _)| k == "content-length") {
        let len: usize = cl.1.parse().context("content-length")?;
        body.resize(len, 0);
        reader.read_exact(&mut body).context("read body")?;
    } else if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).context("chunk size")?;
            let size_line = size_line.trim_end_matches(['\r', '\n']);
            let chunk_len = usize::from_str_radix(size_line.trim(), 16)
                .map_err(|_| anyhow::anyhow!("bad chunk size"))?;
            if chunk_len == 0 {
                let mut crlf = [0u8; 2];
                let _ = reader.read_exact(&mut crlf);
                break;
            }
            let mut chunk = vec![0u8; chunk_len];
            reader.read_exact(&mut chunk).context("chunk data")?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).context("chunk crlf")?;
        }
    } else {
        reader
            .read_to_end(&mut body)
            .context("read body until EOF")?;
    }

    Ok(RawHttpResponse {
        status,
        reason,
        headers,
        body,
        final_url: None,
    })
}

fn trace_socks_granted_after_handshake() {
    let Some(t) = trace_curl_from_env() else {
        return;
    };
    t.write_line("== Info: SOCKS4 request granted\n");
}

fn read_response_body(mut reader: impl Read, context: &'static str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).context(context)?;
    Ok(out)
}

fn build_cookie_specs(config: &ConfigSet) -> Result<Vec<CookieSpec>> {
    let Some(path_raw) = config.get("http.cookieFile") else {
        return Ok(Vec::new());
    };
    let path_raw = path_raw.trim();
    if path_raw.is_empty() {
        return Ok(Vec::new());
    }
    let lines = read_cookie_file_lines(path_raw)?;
    Ok(lines
        .iter()
        .filter_map(|line| parse_cookie_spec(line))
        .collect())
}

fn read_cookie_file_lines(path_raw: &str) -> Result<Vec<String>> {
    let path = PathBuf::from(path_raw);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("read cookie file '{}'", path.display()))?;
    let mut out = Vec::new();
    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        out.push(trimmed.to_string());
    }
    Ok(out)
}

fn parse_cookie_spec(line: &str) -> Option<CookieSpec> {
    parse_netscape_cookie(line).or_else(|| parse_header_cookie(line))
}

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

fn redact_cookie_header(cookie: &str) -> String {
    let mut out = Vec::new();
    for part in cookie.split(';') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((k, _)) = p.split_once('=') {
            out.push(format!("{}=<redacted>", k.trim()));
        } else {
            out.push(p.to_string());
        }
    }
    out.join("; ")
}

fn discovery_url_for_mode(url: &str, smart_http_enabled: bool) -> String {
    if smart_http_enabled {
        return url.to_string();
    }
    if let Some((prefix, _)) = url.split_once("/info/refs?service=") {
        return format!("{prefix}/info/refs");
    }
    if let Some(prefix) = url.strip_suffix("/git-upload-pack") {
        return format!("{prefix}/info/refs");
    }
    if let Some(prefix) = url.strip_suffix("/git-receive-pack") {
        return format!("{prefix}/info/refs");
    }
    url.to_string()
}

fn trace_component_enabled(components: &str, want: &str) -> bool {
    let c = components.trim();
    if c.is_empty() {
        return true;
    }
    c.split(|ch: char| ch == ':' || ch == ',' || ch.is_whitespace())
        .any(|p| p.eq_ignore_ascii_case(want))
}

fn trace_curl_from_env() -> Option<TraceCurl> {
    let raw = std::env::var("GIT_TRACE_CURL")
        .ok()
        .or_else(|| std::env::var("GIT_CURL_VERBOSE").ok())?;
    let raw = raw.trim();
    if raw.is_empty() || raw == "0" || raw.eq_ignore_ascii_case("false") {
        return None;
    }
    let path = if raw == "1" || raw.eq_ignore_ascii_case("true") {
        TraceCurlDest::Stderr
    } else {
        TraceCurlDest::File(raw.to_string())
    };
    let components = std::env::var("GIT_TRACE_CURL_COMPONENTS").unwrap_or_default();
    let redact = std::env::var("GIT_TRACE_REDACT").ok().as_deref() != Some("0");
    Some(TraceCurl {
        path,
        components,
        redact,
    })
}

fn env_proxy_for_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if no_proxy_matches(&parsed) {
        return None;
    }
    let scheme = parsed.scheme();
    let candidates: &[&str] = match scheme {
        "https" => &["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"],
        "http" => &["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"],
        _ => &["all_proxy", "ALL_PROXY"],
    };
    candidates.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

fn no_proxy_matches(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let port = url.port_or_known_default();
    let raw = std::env::var("no_proxy")
        .or_else(|_| std::env::var("NO_PROXY"))
        .unwrap_or_default();
    raw.split(',').map(str::trim).any(|entry| {
        if entry.is_empty() {
            return false;
        }
        if entry == "*" {
            return true;
        }
        let entry = entry.to_ascii_lowercase();
        if let Some((entry_host, entry_port)) = entry.rsplit_once(':') {
            if entry_port.chars().all(|c| c.is_ascii_digit())
                && entry_port.parse::<u16>().ok() != port
            {
                return false;
            }
            return no_proxy_host_matches(&host, entry_host);
        }
        no_proxy_host_matches(&host, &entry)
    })
}

fn no_proxy_host_matches(host: &str, entry: &str) -> bool {
    let entry = entry.trim_start_matches('.');
    host == entry || host.ends_with(&format!(".{entry}"))
}

pub(crate) fn scrub_url_credentials(url: &str) -> String {
    if let Ok(mut parsed) = Url::parse(url) {
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        return parsed.to_string();
    }
    url.to_string()
}

fn http_access_error(url: &str, status: u16) -> anyhow::Error {
    let url = scrub_url_credentials(url);
    anyhow::anyhow!("unable to access '{url}': The requested URL returned error: {status}")
}

fn http_request_error(method: &str, url: &str, err: impl std::fmt::Display) -> anyhow::Error {
    let safe_url = scrub_url_credentials(url);
    let mut message = err.to_string();
    if safe_url != url {
        message = message.replace(url, &safe_url);
    }
    anyhow::anyhow!("{method} {safe_url}: {message}")
}

fn ssl_verify_enabled(config: &ConfigSet) -> bool {
    if std::env::var("GIT_SSL_NO_VERIFY")
        .ok()
        .is_some_and(|value| {
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
    {
        return false;
    }
    config
        .get("http.sslVerify")
        .as_deref()
        .map(|value| parse_bool(value).unwrap_or(true))
        .unwrap_or(true)
}

fn ureq_agent(ssl_verify: bool, proxy: Option<ureq::Proxy>) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        // Surface >= 400 responses as `Ok` so the auth-retry logic can read the
        // status, body, and `WWW-Authenticate` headers itself.
        .http_status_as_error(false)
        .proxy(proxy);
    if !ssl_verify {
        // `http.sslVerify=false` / `GIT_SSL_NO_VERIFY`: opt out of certificate
        // verification, matching Git's documented escape hatch. ureq's built-in
        // `disable_verification` replaces a hand-rolled rustls verifier.
        builder = builder.tls_config(
            ureq::tls::TlsConfig::builder()
                .disable_verification(true)
                .build(),
        );
    }
    builder.build().new_agent()
}

fn build_transport(
    config: &ConfigSet,
    proxy_auth_method: &ProxyAuthMethod,
    proxy_raw: Option<&str>,
) -> Result<Transport> {
    let ssl_verify = ssl_verify_enabled(config);
    let Some(raw_proxy) = proxy_raw else {
        return Ok(Transport::Ureq(ureq_agent(ssl_verify, None)));
    };
    let raw_proxy = raw_proxy.trim();
    if raw_proxy.is_empty() {
        return Ok(Transport::Ureq(ureq_agent(ssl_verify, None)));
    }
    build_transport_from_proxy(raw_proxy, ssl_verify, proxy_auth_method)
}

fn build_transport_from_proxy(
    raw_proxy: &str,
    ssl_verify: bool,
    proxy_auth_method: &ProxyAuthMethod,
) -> Result<Transport> {
    validate_proxy_url(raw_proxy)?;
    let with_scheme = if raw_proxy.contains("://") {
        raw_proxy.to_string()
    } else {
        format!("http://{raw_proxy}")
    };
    let parsed =
        Url::parse(&with_scheme).map_err(|_| anyhow::anyhow!("Invalid proxy URL '{raw_proxy}'"))?;

    if let Some(path) = socks_unix_proxy_socket(raw_proxy, &parsed) {
        return Ok(Transport::SocksUnix { socket_path: path });
    }

    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme == "http" {
        let mut p = parsed.clone();
        fill_proxy_password_via_askpass(&mut p)?;
        ensure_supported_proxy_auth_method(proxy_auth_method, &p)?;
        let proxy_host = p
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid proxy URL '{raw_proxy}'"))?
            .to_string();
        let proxy_port = p.port_or_known_default().unwrap_or(80);
        let proxy_basic = proxy_basic_token(&p)?;
        return Ok(Transport::HttpForward {
            proxy_host,
            proxy_port,
            proxy_basic,
        });
    }

    let proxy_url = normalize_proxy_url_for_ureq(raw_proxy, &parsed)?;
    let parsed_proxy_url =
        Url::parse(&proxy_url).map_err(|_| anyhow::anyhow!("Invalid proxy URL '{raw_proxy}'"))?;
    ensure_supported_proxy_auth_method(proxy_auth_method, &parsed_proxy_url)?;
    let proxy =
        ureq::Proxy::new(&proxy_url).with_context(|| format!("invalid proxy URL '{raw_proxy}'"))?;
    Ok(Transport::Ureq(ureq_agent(ssl_verify, Some(proxy))))
}

fn proxy_basic_token(url: &Url) -> Result<Option<String>> {
    if url.username().is_empty() {
        return Ok(None);
    }
    let pass = url.password().unwrap_or("");
    let cred = format!("{}:{}", url.username(), pass);
    Ok(Some(
        base64::engine::general_purpose::STANDARD.encode(cred.as_bytes()),
    ))
}

/// `socks*://localhost/abs/path.sock` style proxy (Git uses a path after localhost).
///
/// Important: `url::Url::path()` applies percent-decoding, which breaks double-encoded
/// test paths like `%2530.sock` → must decode exactly once from the raw string (t5564).
fn socks_unix_proxy_socket(raw_proxy: &str, url: &Url) -> Option<PathBuf> {
    let scheme = url.scheme().to_ascii_lowercase();
    if !scheme.starts_with("socks") {
        return None;
    }
    let host = url.host_str()?;
    if !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let lower = raw_proxy.to_ascii_lowercase();
    let key = "localhost";
    let idx = lower.find(key)?;
    let after_host = &raw_proxy[idx + key.len()..];
    if after_host.starts_with(':') {
        return None;
    }
    if !after_host.starts_with('/') {
        return None;
    }
    if after_host.len() <= 1 {
        return None;
    }
    Some(PathBuf::from(percent_decode_path(after_host)))
}

fn percent_decode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let a = bytes[i + 1];
            let b = bytes[i + 2];
            if let (Some(h1), Some(h2)) = (from_hex(a), from_hex(b)) {
                out.push(char::from(h1 * 16 + h2));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Git-style checks from `http.c` (paths only for SOCKS; host must be localhost).
fn validate_proxy_url(raw: &str) -> Result<()> {
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let parsed =
        Url::parse(&with_scheme).map_err(|_| anyhow::anyhow!("Invalid proxy URL '{raw}'"))?;
    let path = parsed.path();
    let has_extra_path = path.len() > 1;
    if has_extra_path {
        let scheme = parsed.scheme().to_ascii_lowercase();
        if !scheme.starts_with("socks") {
            bail!("Invalid proxy URL '{raw}': only SOCKS proxies support paths");
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid proxy URL '{raw}'"))?;
        if !host.eq_ignore_ascii_case("localhost") {
            bail!("Invalid proxy URL '{raw}': host must be localhost if a path is present");
        }
    }
    Ok(())
}

fn normalize_proxy_url_for_ureq(raw: &str, parsed: &Url) -> Result<String> {
    if socks_unix_proxy_socket(raw, parsed).is_some() {
        bail!("internal: SOCKS unix proxy should not use ureq");
    }
    let mut url = parsed.clone();
    fill_proxy_password_via_askpass(&mut url)?;
    Ok(url.to_string())
}

fn fill_proxy_password_via_askpass(url: &mut Url) -> Result<()> {
    if url.password().is_some() {
        return Ok(());
    }
    let user = url.username();
    if user.is_empty() {
        return Ok(());
    }
    let askpass = match std::env::var("GIT_ASKPASS") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => return Ok(()),
    };
    let display = {
        let mut u = url.clone();
        let _ = u.set_password(None);
        let mut s = u.to_string();
        // Match Git/credential helper prompts: no trailing slash for host:port-only URLs (t5564).
        if u.path() == "/" || u.path().is_empty() {
            while s.ends_with('/') {
                s.pop();
            }
        }
        s
    };
    let prompt = format!("Password for '{display}': ");
    let cache = PROXY_ASKPASS_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(pass) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&display)
        .cloned()
    {
        url.set_password(Some(&pass))
            .map_err(|_| anyhow::anyhow!("could not set proxy password in URL"))?;
        return Ok(());
    }
    let out = Command::new(&askpass)
        .arg(&prompt)
        .output()
        .with_context(|| format!("run GIT_ASKPASS ({askpass})"))?;
    if !out.status.success() {
        bail!("failed to get proxy password from askpass");
    }
    let pass = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if pass.is_empty() {
        bail!("askpass returned an empty proxy password");
    }
    url.set_password(Some(&pass))
        .map_err(|_| anyhow::anyhow!("could not set proxy password in URL"))?;
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(display, pass);
    Ok(())
}

static PROXY_ASKPASS_CACHE: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();

fn run_askpass(prompt: &str) -> Result<String> {
    let askpass = std::env::var("GIT_ASKPASS").unwrap_or_default();
    if askpass.trim().is_empty() {
        bail!("failed to get credentials: GIT_ASKPASS is not set");
    }
    let out = Command::new(&askpass)
        .arg(prompt)
        .output()
        .with_context(|| format!("run GIT_ASKPASS ({askpass})"))?;
    if !out.status.success() {
        bail!("askpass failed");
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        bail!("askpass returned empty value");
    }
    Ok(value)
}

fn credential_prompt_origin(url: &str, use_http_path: bool) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("bad URL {url}"))?;
    let scheme = parsed.scheme();
    let host = host_header_value(&parsed);
    if use_http_path {
        if let Some(path) = credential_prompt_path(parsed.path()) {
            return Ok(format!("{scheme}://{host}/{path}"));
        }
    }
    Ok(format!("{scheme}://{host}"))
}

fn credential_prompt_scheme(url: &str) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("bad URL {url}"))?;
    Ok(parsed.scheme().to_string())
}

fn credential_prompt_host(url: &str, use_http_path: bool) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("bad URL {url}"))?;
    let host = host_header_value(&parsed);
    if use_http_path {
        if let Some(path) = credential_prompt_path(parsed.path()) {
            return Ok(format!("{host}/{path}"));
        }
    }
    Ok(host)
}

/// Path segment shown in Git-compatible askpass prompts when `credential.useHttpPath` applies.
fn credential_prompt_path(raw_path: &str) -> Option<String> {
    let mut path = raw_path.trim_start_matches('/');
    if let Some(stripped) = path.strip_suffix("info/refs") {
        path = stripped.trim_end_matches('/');
    }
    (!path.is_empty()).then(|| path.to_string())
}
