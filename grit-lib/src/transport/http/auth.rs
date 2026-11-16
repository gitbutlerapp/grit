//! HTTP authentication challenge parsing and scheme selection (server and proxy).
//!
//! Preference order for `anyauth` matches libcurl's `CURLAUTH_ANY`: Negotiate, then
//! NTLM, then Basic. Digest is recognized but not supported.

/// Authentication scheme named in a `WWW-Authenticate` / `Proxy-Authenticate` challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScheme {
    Negotiate,
    Ntlm,
    Basic,
    Digest,
    Unknown,
}

impl AuthScheme {
    fn from_token(token: &str) -> Self {
        match token.trim().to_ascii_lowercase().as_str() {
            "negotiate" | "spnego" => Self::Negotiate,
            "ntlm" => Self::Ntlm,
            "basic" => Self::Basic,
            "digest" => Self::Digest,
            _ => Self::Unknown,
        }
    }

    /// Stable lowercase name for errors and logging.
    #[must_use]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Negotiate => "negotiate",
            Self::Ntlm => "ntlm",
            Self::Basic => "basic",
            Self::Digest => "digest",
            Self::Unknown => "unknown",
        }
    }
}

/// One parsed challenge from `WWW-Authenticate` or `Proxy-Authenticate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthChallenge {
    pub scheme: AuthScheme,
    /// Raw challenge text after the scheme name (may include base64 tokens).
    pub tail: String,
}

/// Parse all challenges from repeated header values, splitting comma-separated
/// entries while keeping commas inside quoted strings intact.
#[must_use]
pub(crate) fn parse_authenticate_headers(values: &[String]) -> Vec<AuthChallenge> {
    let mut out = Vec::new();
    for value in values {
        for piece in split_http_challenges(value) {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let (scheme_token, tail) = piece
                .split_once(|c: char| c.is_whitespace())
                .map_or((piece, ""), |(s, t)| (s, t.trim()));
            out.push(AuthChallenge {
                scheme: AuthScheme::from_token(scheme_token),
                tail: tail.to_owned(),
            });
        }
    }
    out
}

/// Split a single header value into challenge segments at commas outside quotes.
fn split_http_challenges(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if b == b',' && !in_quotes {
            parts.push(value[start..i].trim());
            start = i + 1;
        }
        i += 1;
    }
    parts.push(value[start..].trim());
    parts
}

/// Configured proxy authentication method (`http.proxyAuthMethod`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ProxyAuthMethod {
    #[default]
    AnyAuth,
    Basic,
    Negotiate,
    Ntlm,
    Digest,
}

impl ProxyAuthMethod {
    /// Parse Git's `http.proxyAuthMethod` value (case-insensitive).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "anyauth" | "any" => Some(Self::AnyAuth),
            "basic" => Some(Self::Basic),
            "negotiate" => Some(Self::Negotiate),
            "ntlm" => Some(Self::Ntlm),
            "digest" => Some(Self::Digest),
            _ => None,
        }
    }
}

/// `http.emptyAuth` semantics (Git 2.43+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EmptyAuthMode {
    /// Never send empty credentials on the first request (`false`, default).
    #[default]
    Never,
    /// Send an empty `Authorization` header on the first request (`true`).
    Always,
    /// On `401`, if Negotiate is offered, try GSS with no username before the helper (`auto`).
    Auto,
}

impl EmptyAuthMode {
    /// Parse `http.emptyAuth` / `GIT_HTTP_EMPTY_AUTH` (boolean or `auto`).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        if s.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        match crate::config::parse_git_config_maybe_bool(s)? {
            true => Some(Self::Always),
            false => Some(Self::Never),
        }
    }
}

/// Whether the challenge list offers a scheme (permissive for Basic when empty).
#[must_use]
pub(crate) fn offers_scheme(challenges: &[AuthChallenge], scheme: AuthScheme) -> bool {
    challenges.is_empty() && matches!(scheme, AuthScheme::Basic)
        || challenges
            .iter()
            .any(|c| c.scheme == scheme && c.scheme != AuthScheme::Unknown)
}

/// Pick the server authentication scheme from challenges (CURLAUTH_ANY order).
///
/// Returns `None` when only unsupported schemes (for example Digest alone) are present.
#[must_use]
pub(crate) fn select_server_scheme(challenges: &[AuthChallenge]) -> Option<AuthScheme> {
    if challenges.is_empty() {
        return Some(AuthScheme::Basic);
    }
    [AuthScheme::Negotiate, AuthScheme::Ntlm, AuthScheme::Basic]
        .into_iter()
        .find(|&preferred| offers_scheme(challenges, preferred))
}

/// Pick a proxy authentication scheme honoring `http.proxyAuthMethod`.
#[must_use]
pub(crate) fn select_proxy_scheme(
    challenges: &[AuthChallenge],
    method: ProxyAuthMethod,
) -> Option<AuthScheme> {
    match method {
        ProxyAuthMethod::Digest => {
            if offers_scheme(challenges, AuthScheme::Digest) {
                return Some(AuthScheme::Digest);
            }
            None
        }
        ProxyAuthMethod::Basic | ProxyAuthMethod::Negotiate | ProxyAuthMethod::Ntlm => {
            let fixed = match method {
                ProxyAuthMethod::Basic => AuthScheme::Basic,
                ProxyAuthMethod::Negotiate => AuthScheme::Negotiate,
                ProxyAuthMethod::Ntlm => AuthScheme::Ntlm,
                _ => unreachable!(),
            };
            offers_scheme(challenges, fixed).then_some(fixed)
        }
        ProxyAuthMethod::AnyAuth => select_server_scheme(challenges),
    }
}

/// Extract the base64 NTLM blob from a challenge tail (`NTLM <blob>` or bare blob).
#[must_use]
pub(crate) fn ntlm_challenge_blob(challenge: &AuthChallenge) -> Option<&str> {
    if challenge.scheme != AuthScheme::Ntlm {
        return None;
    }
    let tail = challenge.tail.trim();
    if tail.is_empty() {
        return None;
    }
    if let Some(rest) = tail.strip_prefix("NTLM ") {
        return Some(rest.trim());
    }
    if tail
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    {
        return Some(tail);
    }
    None
}

/// Collect scheme names from challenges for [`AuthFailure::UnsupportedScheme`].
#[must_use]
pub(crate) fn challenge_scheme_names(challenges: &[AuthChallenge]) -> Vec<String> {
    let mut names = Vec::new();
    for c in challenges {
        let name = match c.scheme {
            AuthScheme::Unknown => {
                continue;
            }
            other => other.name().to_string(),
        };
        if !names.iter().any(|n| n == &name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_quoted_comma_stays_in_one_challenge() {
        let v = r#"Negotiate, Basic realm="a,b", NTLM"#;
        let parts = split_http_challenges(v);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "Negotiate");
        assert_eq!(parts[1], r#"Basic realm="a,b""#);
        assert_eq!(parts[2], "NTLM");
    }

    #[test]
    fn select_prefers_negotiate_then_ntlm_then_basic() {
        let challenges = parse_authenticate_headers(&[
            "Basic realm=\"x\"".into(),
            "NTLM".into(),
            "Negotiate".into(),
        ]);
        assert_eq!(
            select_server_scheme(&challenges),
            Some(AuthScheme::Negotiate)
        );
        let only_ntlm = parse_authenticate_headers(&["NTLM".into()]);
        assert_eq!(select_server_scheme(&only_ntlm), Some(AuthScheme::Ntlm));
        let only_basic = parse_authenticate_headers(&["Basic realm=\"git\"".into()]);
        assert_eq!(select_server_scheme(&only_basic), Some(AuthScheme::Basic));
    }

    #[test]
    fn digest_only_yields_none() {
        let challenges = parse_authenticate_headers(&["Digest realm=\"x\"".into()]);
        assert_eq!(select_server_scheme(&challenges), None);
    }

    #[test]
    fn proxy_method_digest_returns_digest_only() {
        let challenges = parse_authenticate_headers(&["NTLM".into()]);
        assert_eq!(
            select_proxy_scheme(&challenges, ProxyAuthMethod::Digest),
            None
        );
        let digest = parse_authenticate_headers(&["Digest realm=\"p\"".into()]);
        assert_eq!(
            select_proxy_scheme(&digest, ProxyAuthMethod::Digest),
            Some(AuthScheme::Digest)
        );
    }

    #[test]
    fn ntlm_blob_from_challenge() {
        let ch = AuthChallenge {
            scheme: AuthScheme::Ntlm,
            tail: "TlRMTVNTUAABAAAAB4IIogAAAAAAAAAAAAAAAAAAAAAGAbEdAAAADw==".into(),
        };
        assert!(ntlm_challenge_blob(&ch).is_some());
    }
}
