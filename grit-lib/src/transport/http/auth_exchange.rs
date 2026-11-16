//! Multi-leg HTTP authentication on a pinned connection (NTLM and Negotiate).

use base64::Engine;
use url::Url;

use crate::config::ConfigSet;
use crate::credentials::{Credential, CredentialProvider};
use crate::error::{AuthFailure, Error, Result};
use crate::remote::RemoteError;

use super::auth::{
    self, ntlm_challenge_blob, parse_authenticate_headers, AuthScheme, EmptyAuthMode,
    ProxyAuthMethod,
};
use super::negotiate;
use super::ntlm::{
    authorization_header, decode_type2_b64, identity_from_username, parse_type2, type1_message,
    type3_message,
};
use super::pinned::{AuthSend, ConnectOptions, PinnedConnection, PinnedPool, PinnedResponse};
use super::ureq_client::HttpTransportTimeouts;

pub(crate) struct HttpAuthSettings {
    pub empty_auth: EmptyAuthMode,
    pub proxy_auth_method: ProxyAuthMethod,
    pub http_proxy: Option<Url>,
    /// FILETIME for NTLMv2, sampled when HTTP settings are built.
    pub ntlm_timestamp: [u8; 8],
}

impl Default for HttpAuthSettings {
    fn default() -> Self {
        Self {
            empty_auth: EmptyAuthMode::Never,
            proxy_auth_method: ProxyAuthMethod::AnyAuth,
            http_proxy: None,
            ntlm_timestamp: [0u8; 8],
        }
    }
}

impl HttpAuthSettings {
    pub(crate) fn from_config(
        config: &ConfigSet,
        environment: &crate::environment::Environment,
    ) -> Self {
        let empty_raw = environment
            .var("GIT_HTTP_EMPTY_AUTH")
            .or_else(|| config.value("http.emptyAuth").map(|s| s.to_string()));
        let empty_auth = empty_raw
            .as_deref()
            .and_then(EmptyAuthMode::parse)
            .unwrap_or_default();
        let proxy_raw = environment
            .var("GIT_HTTP_PROXY_AUTHMETHOD")
            .or_else(|| config.value("http.proxyAuthMethod").map(|s| s.to_string()));
        let proxy_auth_method = proxy_raw
            .as_deref()
            .and_then(ProxyAuthMethod::parse)
            .unwrap_or_default();
        let http_proxy = proxy_url_from_config(config);
        let wall_nanos = crate::environment::wall_clock_unix_timestamp_nanos();
        let secs = wall_nanos / 1_000_000_000;
        let subsec = (wall_nanos % 1_000_000_000) as u32;
        let ntlm_timestamp = super::ntlm::ntlmv2_timestamp_from_unix(secs, subsec);
        Self {
            empty_auth,
            proxy_auth_method,
            http_proxy,
            ntlm_timestamp,
        }
    }

    pub(crate) fn proxy_auth_method_for_url(
        &self,
        config: &ConfigSet,
        request_url: &str,
    ) -> ProxyAuthMethod {
        proxy_auth_method_for_url(config, self.proxy_auth_method, request_url)
    }
}

fn proxy_url_from_config(config: &ConfigSet) -> Option<Url> {
    let raw = config.value("http.proxy")?.to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    Url::parse(&with_scheme).ok()
}

fn proxy_auth_method_for_url(
    config: &ConfigSet,
    fallback: ProxyAuthMethod,
    request_url: &str,
) -> ProxyAuthMethod {
    let entries = config.get_regexp(r"remote\..*\.url").unwrap_or_default();
    for entry in entries {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some(name) = rest.strip_suffix(".url") else {
            continue;
        };
        let Some(remote_url) = entry.value.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !urls_describe_same_repo(remote_url, request_url) {
            continue;
        }
        if let Some(raw) = config.value(&format!("remote.{name}.proxyAuthMethod")) {
            if let Some(method) = ProxyAuthMethod::parse(&raw) {
                return method;
            }
        }
    }
    fallback
}

fn urls_describe_same_repo(remote_url: &str, request_url: &str) -> bool {
    let Ok(a) = Url::parse(remote_url) else {
        return false;
    };
    let Ok(b) = Url::parse(request_url) else {
        return false;
    };
    a.host_str() == b.host_str()
        && a.scheme() == b.scheme()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Returns whether this scheme must use a pinned connection (same TCP socket).
#[must_use]
pub(crate) fn scheme_needs_pinned_connection(scheme: AuthScheme) -> bool {
    matches!(scheme, AuthScheme::Ntlm | AuthScheme::Negotiate)
}

/// Credentials and policy for multi-leg proxy `CONNECT` authentication.
pub(crate) struct TunnelAuth<'a> {
    pub settings: &'a HttpAuthSettings,
    pub credential_config: &'a ConfigSet,
    pub provider: Option<&'a (dyn CredentialProvider + Send + Sync)>,
    pub request_url: &'a str,
}

/// Complete NTLM or Negotiate on the proxy `CONNECT` socket after `407`.
pub(crate) fn establish_proxy_connect_tunnel(
    tcp: &mut std::net::TcpStream,
    proxy_host: &str,
    target_host: &str,
    target_port: u16,
    timeouts: &HttpTransportTimeouts,
    auth: &TunnelAuth<'_>,
    initial: &PinnedResponse,
) -> Result<()> {
    use super::pinned::send_proxy_connect;

    let proxy_method = auth
        .settings
        .proxy_auth_method_for_url(auth.credential_config, auth.request_url);
    let mut response = initial.clone();
    let mut proxy_auth_header: Option<String>;

    loop {
        if response.status == 200 {
            return Ok(());
        }
        if response.status != 407 {
            return Err(auth_transport_err(
                auth.request_url,
                format!("CONNECT failed with HTTP {}", response.status),
            ));
        }
        let challenges = parse_authenticate_headers(&response.proxy_authenticate);
        let picked =
            auth::select_proxy_scheme(&challenges, proxy_method).unwrap_or(AuthScheme::Ntlm);
        match picked {
            AuthScheme::Digest | AuthScheme::Basic | AuthScheme::Unknown => {
                return Err(auth_err(
                    auth.request_url,
                    AuthFailure::UnsupportedScheme {
                        schemes: auth::challenge_scheme_names(&challenges),
                    },
                ));
            }
            AuthScheme::Ntlm => {
                proxy_auth_header = Some(authorization_header(&type1_message()));
                response = send_proxy_connect(
                    tcp,
                    target_host,
                    target_port,
                    proxy_auth_header.as_deref(),
                    timeouts,
                )?;
                if response.status == 407 {
                    let ch2 = parse_authenticate_headers(&response.proxy_authenticate);
                    let blob_b64 =
                        ch2.iter()
                            .find_map(|c| ntlm_challenge_blob(c))
                            .ok_or_else(|| {
                                auth_err(
                                    auth.request_url,
                                    AuthFailure::MechanismFailed {
                                        scheme: "ntlm".into(),
                                        detail: "proxy did not send an NTLM challenge".into(),
                                    },
                                )
                            })?;
                    let type2 = parse_type2(&decode_type2_b64(blob_b64).map_err(|_| {
                        auth_err(
                            auth.request_url,
                            AuthFailure::MechanismFailed {
                                scheme: "ntlm".into(),
                                detail: "invalid NTLM type-2 message".into(),
                            },
                        )
                    })?)
                    .map_err(|_| {
                        auth_err(
                            auth.request_url,
                            AuthFailure::MechanismFailed {
                                scheme: "ntlm".into(),
                                detail: "invalid NTLM type-2 message".into(),
                            },
                        )
                    })?;
                    let cred = fill_ntlm_credential(
                        auth.request_url,
                        auth.provider,
                        auth.credential_config,
                    )?;
                    let password = cred.password.as_deref().unwrap_or("");
                    let identity =
                        identity_from_username(cred.username.as_deref().unwrap_or_default());
                    let type3 = type3_message(
                        &identity,
                        password,
                        &type2,
                        proxy_host,
                        Some(auth.settings.ntlm_timestamp),
                    )
                    .map_err(|_| {
                        auth_err(
                            auth.request_url,
                            AuthFailure::MechanismFailed {
                                scheme: "ntlm".into(),
                                detail: "failed to build NTLM type-3 message".into(),
                            },
                        )
                    })?;
                    proxy_auth_header = Some(authorization_header(&type3));
                    response = send_proxy_connect(
                        tcp,
                        target_host,
                        target_port,
                        proxy_auth_header.as_deref(),
                        timeouts,
                    )?;
                    if response.status == 407 {
                        if let Some(p) = auth.provider {
                            let _ = p.reject(&cred);
                        }
                        return Err(auth_err(auth.request_url, AuthFailure::Rejected));
                    }
                    if let Some(p) = auth.provider {
                        let _ = p.approve(&cred);
                    }
                } else if response.status != 200 {
                    return Err(auth_transport_err(
                        auth.request_url,
                        format!("CONNECT NTLM failed with HTTP {}", response.status),
                    ));
                }
            }
            AuthScheme::Negotiate => {
                let mut gss = negotiate::new_negotiate_context()?;
                gss.set_service_host(proxy_host);
                let mut in_bytes: Option<Vec<u8>> = None;
                loop {
                    let token = gss.step(in_bytes.as_deref())?;
                    let Some(token) = token else {
                        break;
                    };
                    proxy_auth_header = Some(format!(
                        "Negotiate {}",
                        base64::engine::general_purpose::STANDARD.encode(&token)
                    ));
                    response = send_proxy_connect(
                        tcp,
                        target_host,
                        target_port,
                        proxy_auth_header.as_deref(),
                        timeouts,
                    )?;
                    if response.status != 407 {
                        break;
                    }
                    let ch = parse_authenticate_headers(&response.proxy_authenticate);
                    in_bytes = ch
                        .iter()
                        .find(|c| c.scheme == AuthScheme::Negotiate)
                        .and_then(|c| {
                            let b64 = c.tail.trim();
                            base64::engine::general_purpose::STANDARD.decode(b64).ok()
                        });
                }
            }
        }
    }
}

/// Parameters for one pinned authenticated HTTP exchange.
pub(crate) struct PinnedAuthRequest<'a> {
    pub pool: &'a PinnedPool,
    pub settings: &'a HttpAuthSettings,
    pub timeouts: &'a HttpTransportTimeouts,
    pub url: &'a str,
    pub method: &'a str,
    pub path_query: &'a str,
    pub extra_headers: &'a [(String, String)],
    pub body: Option<&'a [u8]>,
    pub provider: Option<&'a (dyn CredentialProvider + Send + Sync)>,
    pub credential_config: &'a ConfigSet,
    pub scheme: AuthScheme,
}

/// Run NTLM or Negotiate on a pinned connection, then complete the Git HTTP request.
pub(crate) fn pinned_request_with_auth(req: &PinnedAuthRequest<'_>) -> Result<Vec<u8>> {
    let parsed = Url::parse(req.url).map_err(|e| auth_transport_err(req.url, e.to_string()))?;
    let origin = origin_key(&parsed);
    let host = parsed.host_str().unwrap_or("localhost");

    let mut conn = match req.pool.take(&origin) {
        Some(c) => c,
        None => {
            let proxy = req.settings.http_proxy.as_ref();
            let tunnel_auth = proxy.map(|_| TunnelAuth {
                settings: req.settings,
                credential_config: req.credential_config,
                provider: req.provider,
                request_url: req.url,
            });
            PinnedConnection::connect(&ConnectOptions {
                target: &parsed,
                proxy,
                timeouts: req.timeouts,
                tunnel_auth,
            })?
        }
    };

    let proxy_method = req
        .settings
        .proxy_auth_method_for_url(req.credential_config, req.url);
    let mut auth_header = initial_auth_header(req.settings, req.scheme, host)?;
    let mut response = send(&mut conn, req, auth_header.as_deref(), false)?;

    loop {
        let (challenges, proxy_challenge) = challenge_headers(&response);
        if response.status != 401 && response.status != 407 {
            break;
        }
        let picked = if proxy_challenge {
            auth::select_proxy_scheme(&challenges, proxy_method)
        } else {
            auth::select_server_scheme(&challenges)
        }
        .unwrap_or(req.scheme);

        match picked {
            AuthScheme::Digest | AuthScheme::Basic => {
                return Err(auth_err(
                    req.url,
                    AuthFailure::UnsupportedScheme {
                        schemes: auth::challenge_scheme_names(&challenges),
                    },
                ));
            }
            AuthScheme::Ntlm => {
                auth_header = Some(authorization_header(&type1_message()));
                response = send(&mut conn, req, auth_header.as_deref(), proxy_challenge)?;
                if response.status == 401 || response.status == 407 {
                    let ch2 = challenge_headers(&response).0;
                    let blob_b64 =
                        ch2.iter()
                            .find_map(|c| ntlm_challenge_blob(c))
                            .ok_or_else(|| {
                                auth_err(
                                    req.url,
                                    AuthFailure::MechanismFailed {
                                        scheme: "ntlm".into(),
                                        detail: "server did not send an NTLM challenge".into(),
                                    },
                                )
                            })?;
                    let type2_bytes = decode_type2_b64(blob_b64).map_err(|_| {
                        auth_err(
                            req.url,
                            AuthFailure::MechanismFailed {
                                scheme: "ntlm".into(),
                                detail: "invalid NTLM type-2 message".into(),
                            },
                        )
                    })?;
                    let type2 = parse_type2(&type2_bytes).map_err(|_| {
                        auth_err(
                            req.url,
                            AuthFailure::MechanismFailed {
                                scheme: "ntlm".into(),
                                detail: "invalid NTLM type-2 message".into(),
                            },
                        )
                    })?;
                    let cred = fill_ntlm_credential(req.url, req.provider, req.credential_config)?;
                    let password = cred.password.as_deref().unwrap_or("");
                    let identity =
                        identity_from_username(cred.username.as_deref().unwrap_or_default());
                    let type3 = type3_message(
                        &identity,
                        password,
                        &type2,
                        host,
                        Some(req.settings.ntlm_timestamp),
                    )
                    .map_err(|_| {
                        auth_err(
                            req.url,
                            AuthFailure::MechanismFailed {
                                scheme: "ntlm".into(),
                                detail: "failed to build NTLM type-3 message".into(),
                            },
                        )
                    })?;
                    auth_header = Some(authorization_header(&type3));
                    response = send(&mut conn, req, auth_header.as_deref(), proxy_challenge)?;
                    if response.status == 401 || response.status == 407 {
                        if let Some(p) = req.provider {
                            let _ = p.reject(&cred);
                        }
                        return Err(auth_err(req.url, AuthFailure::Rejected));
                    }
                    if let Some(p) = req.provider {
                        let _ = p.approve(&cred);
                    }
                    break;
                }
            }
            AuthScheme::Negotiate => {
                let mut gss = negotiate::new_negotiate_context()?;
                gss.set_service_host(host);
                let mut in_bytes: Option<Vec<u8>> = None;
                loop {
                    let token = gss.step(in_bytes.as_deref())?;
                    let Some(token) = token else {
                        break;
                    };
                    auth_header = Some(format!(
                        "Negotiate {}",
                        base64::engine::general_purpose::STANDARD.encode(&token)
                    ));
                    response = send(&mut conn, req, auth_header.as_deref(), proxy_challenge)?;
                    if response.status != 401 && response.status != 407 {
                        break;
                    }
                    let ch = challenge_headers(&response).0;
                    in_bytes = ch
                        .iter()
                        .find(|c| c.scheme == AuthScheme::Negotiate)
                        .and_then(|c| {
                            let b64 = c.tail.trim();
                            base64::engine::general_purpose::STANDARD.decode(b64).ok()
                        });
                }
                break;
            }
            AuthScheme::Unknown => {
                return Err(auth_err(
                    req.url,
                    AuthFailure::UnsupportedScheme {
                        schemes: auth::challenge_scheme_names(&challenges),
                    },
                ));
            }
        }
    }

    if response.status >= 400 {
        return Err(auth_err(
            req.url,
            AuthFailure::MechanismFailed {
                scheme: req.scheme.name().into(),
                detail: format!("HTTP {}", response.status),
            },
        ));
    }
    conn.mark_authenticated();
    finish_pinned(req.pool, origin, conn, response)
}

fn send(
    conn: &mut PinnedConnection,
    req: &PinnedAuthRequest<'_>,
    auth: Option<&str>,
    proxy: bool,
) -> Result<PinnedResponse> {
    let auth_send = match auth {
        Some(value) if proxy => AuthSend::ProxyAuthorization(value),
        Some(value) => AuthSend::Authorization(value),
        None => AuthSend::Skip,
    };
    conn.request(
        req.method,
        req.path_query,
        req.extra_headers,
        req.body,
        auth_send,
    )
}

fn challenge_headers(response: &PinnedResponse) -> (Vec<auth::AuthChallenge>, bool) {
    if response.status == 407 {
        (
            parse_authenticate_headers(&response.proxy_authenticate),
            true,
        )
    } else {
        (
            parse_authenticate_headers(&response.www_authenticate),
            false,
        )
    }
}

/// Optional `Authorization` header for the first request when `http.emptyAuth` is set.
pub(crate) fn empty_auth_first_header(
    settings: &HttpAuthSettings,
    url: &str,
) -> Result<Option<String>> {
    let host = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "localhost".to_string());
    match settings.empty_auth {
        EmptyAuthMode::Always => initial_auth_header(settings, AuthScheme::Negotiate, &host),
        _ => Ok(None),
    }
}

/// Whether a `401` may proceed without a credential provider (`http.emptyAuth=auto` + Negotiate).
#[must_use]
pub(crate) fn may_try_empty_negotiate(
    settings: &HttpAuthSettings,
    challenges: &[auth::AuthChallenge],
) -> bool {
    settings.empty_auth == EmptyAuthMode::Auto
        && auth::offers_scheme(challenges, AuthScheme::Negotiate)
}

fn initial_auth_header(
    settings: &HttpAuthSettings,
    scheme: AuthScheme,
    host: &str,
) -> Result<Option<String>> {
    match settings.empty_auth {
        EmptyAuthMode::Always if scheme == AuthScheme::Negotiate => {
            let mut gss = negotiate::new_negotiate_context()?;
            gss.set_service_host(host);
            let token = gss.step(None)?.ok_or_else(|| {
                auth_err(
                    "",
                    AuthFailure::MechanismFailed {
                        scheme: "negotiate".into(),
                        detail: "GSS did not produce an initial token".into(),
                    },
                )
            })?;
            Ok(Some(format!(
                "Negotiate {}",
                base64::engine::general_purpose::STANDARD.encode(&token)
            )))
        }
        EmptyAuthMode::Always => Ok(Some("Basic ".to_owned())),
        _ => Ok(None),
    }
}

fn fill_ntlm_credential(
    url: &str,
    provider: Option<&(dyn CredentialProvider + Send + Sync)>,
    config: &ConfigSet,
) -> Result<Credential> {
    let provider = provider.ok_or_else(|| auth_err(url, AuthFailure::NoCredentialProvider))?;
    let input = crate::transport::http::ureq_client::credential_for_url(url, config)
        .ok_or_else(|| auth_err(url, AuthFailure::UrlNotDescribable))?;
    let cred = provider
        .fill(&input)
        .map_err(|e| auth_err(url, AuthFailure::from_fill_error(e)))?;
    if cred.username.as_deref().is_none_or(str::is_empty) {
        return Err(auth_err(url, AuthFailure::UnusableCredentials));
    }
    Ok(cred)
}

fn finish_pinned(
    pool: &PinnedPool,
    origin: String,
    conn: PinnedConnection,
    response: PinnedResponse,
) -> Result<Vec<u8>> {
    if conn.is_authenticated() {
        pool.store(origin, conn);
    }
    if response.status >= 400 {
        return Err(auth_transport_err("", format!("HTTP {}", response.status)));
    }
    Ok(response.body)
}

fn origin_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    format!("{}://{}:{}", url.scheme(), host, port)
}

fn auth_err(url: &str, reason: AuthFailure) -> Error {
    Error::Remote(RemoteError::Auth {
        url: url.to_owned(),
        reason,
    })
}

fn auth_transport_err(url: &str, detail: String) -> Error {
    Error::Remote(RemoteError::Unreadable {
        url: Some(url.to_owned()),
        reason: crate::error::RemoteUnreadableReason::Transport { detail },
    })
}
