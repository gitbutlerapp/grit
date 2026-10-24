//! Sign-in to a `grit serve` host with an access token.
//!
//! A `grit serve` server authenticates every request with a token its admin
//! created (`grit serve add-user`). Signing in stores that token with the
//! user's credential helper, so later HTTP fetches and pushes to the host send
//! it as the basic-auth password without the token ever appearing in a remote
//! URL:
//!
//! 1. [`ServerUrl::parse`] reduces whatever URL the user typed (a server root
//!    or a repository URL) to the scheme and `host[:port]` that credential
//!    helpers key on.
//! 2. [`fetch_identity`] asks the server who the token belongs to, at
//!    [`IDENTITY_PATH`]. A wrong token or a host that is not a grit server is
//!    reported before anything is stored.
//! 3. [`ServerUrl::credential`] builds the [`Credential`] a
//!    [`CredentialProvider`] should `approve` (store) or `reject` (erase).
//!
//! The identity endpoint's JSON body is [`ServerIdentity`]; the server
//! serializes the same type, so client and server share one contract.
//! Nothing here prompts or prints.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::credentials::{Credential, CredentialProvider};
use crate::error::{Error, Result};
use crate::transport::http::HttpClient;

/// Path, relative to the server root, of the endpoint that names the user a
/// token belongs to. It starts with `.`, which no served repository name may,
/// so it never shadows a repository.
pub const IDENTITY_PATH: &str = ".grit/user";

/// User name sent with the token while verifying it; the server ignores it.
const VERIFY_USERNAME: &str = "grit";

/// The user a token authenticates as: the JSON body of [`IDENTITY_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIdentity {
    /// The user's name on the server.
    pub name: String,
    /// Whether the user is an admin (read and write on every repository).
    pub admin: bool,
}

/// Why a server URL could not be used or a token could not be verified.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerAuthError {
    /// The URL could not be parsed, or has no host.
    #[error("not a server URL: {0}")]
    InvalidUrl(String),
    /// The URL's scheme is not `http` or `https`.
    #[error("unsupported URL scheme {0:?}; use http:// or https://")]
    UnsupportedScheme(String),
    /// The server refused the token (`401`).
    #[error("the server rejected the token")]
    TokenRejected,
    /// The request failed (connection, TLS, or an HTTP error status).
    #[error(transparent)]
    Http(Error),
    /// The server answered, but not with a grit identity.
    #[error("{0} did not answer like a grit server")]
    NotGritServer(String),
}

/// A server's scheme and authority: what credentials for it are stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    protocol: String,
    host: String,
}

impl ServerUrl {
    /// Parse a server root or repository URL, keeping only its scheme and
    /// `host[:port]`.
    ///
    /// A URL without a scheme is taken as `https://`. User info, path, query
    /// and fragment are dropped, so `http://alice@host:8080/app.git` and
    /// `http://host:8080` name the same server.
    ///
    /// # Errors
    ///
    /// [`ServerAuthError::InvalidUrl`] when the URL does not parse or has no
    /// host, and [`ServerAuthError::UnsupportedScheme`] for anything but
    /// `http` and `https`.
    pub fn parse(url: &str) -> std::result::Result<Self, ServerAuthError> {
        let trimmed = url.trim();
        let with_scheme = if trimmed.contains("://") {
            trimmed.to_owned()
        } else {
            format!("https://{trimmed}")
        };
        let parsed = url::Url::parse(&with_scheme)
            .map_err(|_| ServerAuthError::InvalidUrl(trimmed.to_owned()))?;
        let protocol = parsed.scheme().to_owned();
        if protocol != "http" && protocol != "https" {
            return Err(ServerAuthError::UnsupportedScheme(protocol));
        }
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| ServerAuthError::InvalidUrl(trimmed.to_owned()))?;
        let host = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        Ok(Self { protocol, host })
    }

    /// `http` or `https`.
    #[must_use]
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// The host, with `:port` when the URL named one.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The server root, `<protocol>://<host>/`.
    #[must_use]
    pub fn root_url(&self) -> String {
        format!("{}://{}/", self.protocol, self.host)
    }

    /// The identity endpoint's URL.
    #[must_use]
    pub fn identity_url(&self) -> String {
        format!("{}{IDENTITY_PATH}", self.root_url())
    }

    /// The credential a token for this server is stored under.
    ///
    /// Pass the user name and `Some(token)` to build the credential to
    /// [`approve`](CredentialProvider::approve); pass `None` for both to build
    /// the lookup to [`reject`](CredentialProvider::reject), which erases
    /// whatever is stored for the host.
    #[must_use]
    pub fn credential(&self, username: Option<&str>, token: Option<&str>) -> Credential {
        Credential {
            protocol: Some(self.protocol.clone()),
            host: Some(self.host.clone()),
            username: username.map(str::to_owned),
            password: token.map(str::to_owned),
            ..Default::default()
        }
    }
}

/// A [`CredentialProvider`] that answers every request with one token.
///
/// Give it to an HTTP client to check a token before it is stored: it never
/// runs a helper, and `approve` / `reject` do nothing.
#[derive(Clone)]
pub struct TokenCredentialProvider {
    token: String,
}

impl TokenCredentialProvider {
    /// A provider that sends `token` as the basic-auth password.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl std::fmt::Debug for TokenCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCredentialProvider")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl CredentialProvider for TokenCredentialProvider {
    fn fill(&self, input: &Credential) -> Result<Credential> {
        let mut filled = input.clone();
        filled.username = Some(VERIFY_USERNAME.to_owned());
        filled.password = Some(self.token.clone());
        Ok(filled)
    }

    fn approve(&self, _cred: &Credential) -> Result<()> {
        Ok(())
    }

    fn reject(&self, _cred: &Credential) -> Result<()> {
        Ok(())
    }
}

/// Ask `server` who the token in `http`'s credential provider belongs to.
///
/// `http` should authenticate with the token being checked, for example a
/// client built with a [`TokenCredentialProvider`].
///
/// # Errors
///
/// [`ServerAuthError::TokenRejected`] when the server answers `401` to the
/// token, [`ServerAuthError::Http`] when the request fails otherwise, and
/// [`ServerAuthError::NotGritServer`] when the body is not a
/// [`ServerIdentity`].
pub fn fetch_identity(
    http: &dyn HttpClient,
    server: &ServerUrl,
) -> std::result::Result<ServerIdentity, ServerAuthError> {
    let url = server.identity_url();
    let body = http.get(&url, None).map_err(|e| match e {
        Error::Auth { .. } => ServerAuthError::TokenRejected,
        other => ServerAuthError::Http(other),
    })?;
    serde_json::from_slice(&body).map_err(|_| ServerAuthError::NotGritServer(server.root_url()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn urls_reduce_to_scheme_and_authority() {
        let server = ServerUrl::parse("http://alice@127.0.0.1:8080/app.git?x=1").unwrap();
        assert_eq!(server.protocol(), "http");
        assert_eq!(server.host(), "127.0.0.1:8080");
        assert_eq!(server.root_url(), "http://127.0.0.1:8080/");
        assert_eq!(server.identity_url(), "http://127.0.0.1:8080/.grit/user");

        let default_port = ServerUrl::parse("https://git.example.com:443/").unwrap();
        assert_eq!(default_port.host(), "git.example.com");
    }

    #[test]
    fn a_bare_host_means_https() {
        let server = ServerUrl::parse("git.example.com:9000").unwrap();
        assert_eq!(server.protocol(), "https");
        assert_eq!(server.host(), "git.example.com:9000");
    }

    #[test]
    fn other_schemes_and_hostless_urls_are_refused() {
        assert!(matches!(
            ServerUrl::parse("ssh://git.example.com/app.git"),
            Err(ServerAuthError::UnsupportedScheme(s)) if s == "ssh"
        ));
        assert!(matches!(
            ServerUrl::parse("http://"),
            Err(ServerAuthError::InvalidUrl(_))
        ));
    }

    #[test]
    fn credentials_key_on_protocol_and_host() {
        let server = ServerUrl::parse("http://127.0.0.1:8080/app.git").unwrap();
        let stored = server.credential(Some("bob"), Some("grit_abc"));
        assert_eq!(stored.protocol.as_deref(), Some("http"));
        assert_eq!(stored.host.as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(stored.username.as_deref(), Some("bob"));
        assert_eq!(stored.password.as_deref(), Some("grit_abc"));
        assert_eq!(stored.path, None);

        let lookup = server.credential(None, None);
        assert_eq!(lookup.username, None);
        assert_eq!(lookup.password, None);
    }

    #[test]
    fn the_token_provider_fills_the_password_and_redacts_debug() {
        let provider = TokenCredentialProvider::new("grit_secret");
        let filled = provider.fill(&Credential::default()).unwrap();
        assert_eq!(filled.password.as_deref(), Some("grit_secret"));
        assert!(filled.username.is_some());
        assert!(provider.approve(&filled).is_ok());
        assert!(provider.reject(&filled).is_ok());
        assert!(!format!("{provider:?}").contains("grit_secret"));
    }

    /// Answers every GET with one canned result and records the URL.
    struct Canned {
        answer: Mutex<Option<Result<Vec<u8>>>>,
        seen: Mutex<Vec<String>>,
    }

    impl Canned {
        fn new(answer: Result<Vec<u8>>) -> Self {
            Self {
                answer: Mutex::new(Some(answer)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpClient for Canned {
        fn get(&self, url: &str, _git_protocol: Option<&str>) -> Result<Vec<u8>> {
            self.seen.lock().unwrap().push(url.to_owned());
            self.answer.lock().unwrap().take().unwrap()
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _accept: &str,
            _body: &[u8],
            _git_protocol: Option<&str>,
        ) -> Result<Vec<u8>> {
            unreachable!("identity lookups only GET")
        }
    }

    #[test]
    fn identity_is_read_from_the_identity_endpoint() {
        let http = Canned::new(Ok(br#"{"name":"bob","admin":false}"#.to_vec()));
        let server = ServerUrl::parse("http://127.0.0.1:8080").unwrap();
        let identity = fetch_identity(&http, &server).unwrap();
        assert_eq!(
            identity,
            ServerIdentity {
                name: "bob".to_owned(),
                admin: false
            }
        );
        assert_eq!(
            http.seen.lock().unwrap().as_slice(),
            ["http://127.0.0.1:8080/.grit/user"]
        );
    }

    #[test]
    fn identity_failures_are_typed() {
        let server = ServerUrl::parse("http://127.0.0.1:8080").unwrap();

        let rejected = Canned::new(Err(Error::Auth {
            url: "http://127.0.0.1:8080".to_owned(),
            reason: crate::error::AuthFailure::Rejected,
        }));
        assert!(matches!(
            fetch_identity(&rejected, &server),
            Err(ServerAuthError::TokenRejected)
        ));

        let down = Canned::new(Err(Error::Message("HTTP 404".to_owned())));
        assert!(matches!(
            fetch_identity(&down, &server),
            Err(ServerAuthError::Http(_))
        ));

        let html = Canned::new(Ok(b"<html>hello</html>".to_vec()));
        assert!(matches!(
            fetch_identity(&html, &server),
            Err(ServerAuthError::NotGritServer(_))
        ));
    }
}
