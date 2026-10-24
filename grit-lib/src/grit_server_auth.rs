//! Sign-in to a `grit serve` host.
//!
//! A `grit serve` server authenticates Git requests with access tokens. A
//! client gets one by signing in through the server's web page, much like
//! GitHub's device flow, and stores it with the user's credential helper, so
//! later HTTP fetches and pushes send it as the basic-auth password without the
//! token ever appearing in a remote URL:
//!
//! 1. [`ServerUrl::parse`] reduces whatever URL the user typed (a server root
//!    or a repository URL) to the scheme and `host[:port]` that credential
//!    helpers key on.
//! 2. [`LoginFlow::start`] asks the server for a login code ([`LoginStart`]).
//!    The user opens [`ServerUrl::authorize_url`] in a browser, signs in with
//!    their email address and password, and approves the code.
//! 3. [`LoginFlow::wait`] polls the server until the user approves, then
//!    returns the new token ([`LoginGrant`]).
//! 4. [`ServerUrl::credential`] builds the [`Credential`] a
//!    [`CredentialProvider`] should `approve` (store) or `reject` (erase).
//!
//! A token from elsewhere (an admin's `grit serve token`) can instead be
//! checked with [`fetch_identity`], which asks the server who it belongs to.
//!
//! The JSON bodies ([`ServerIdentity`], [`LoginStart`], [`LoginPollRequest`],
//! [`LoginPoll`]) are serialized by the server from the same types, so client
//! and server share one contract. Nothing here prompts or prints.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::credentials::{Credential, CredentialProvider};
use crate::error::{Error, Result};
use crate::github_auth::PollClock;
use crate::transport::http::HttpClient;

/// Path, relative to the server root, of the endpoint that names the user a
/// token belongs to. It starts with `.`, which no served repository name may,
/// so it never shadows a repository.
pub const IDENTITY_PATH: &str = ".grit/user";

/// Path of the endpoint that starts a sign-in: `POST` with an empty body
/// answers a [`LoginStart`].
pub const LOGIN_PATH: &str = ".grit/login";

/// Path of the endpoint a client polls while the user signs in: `POST` a
/// [`LoginPollRequest`], get a [`LoginPoll`].
pub const LOGIN_POLL_PATH: &str = ".grit/login/poll";

/// Path of the web page where the user approves a login code
/// (`?code=<user code>`).
pub const AUTHORIZE_PATH: &str = ".grit/authorize";

/// Shortest wait between polls, whatever the server asks for.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

const JSON_CONTENT_TYPE: &str = "application/json";

/// User name sent with the token while verifying it; the server ignores it.
const VERIFY_USERNAME: &str = "grit";

/// The user a token authenticates as: the JSON body of [`IDENTITY_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIdentity {
    /// The user's email address, which is their name on the server.
    pub email: String,
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
    /// The user declined the login code in the browser.
    #[error("the sign-in was declined")]
    LoginDenied,
    /// The login code expired before the user approved it.
    #[error("the sign-in code expired")]
    LoginExpired,
}

/// A started sign-in: the JSON body [`LOGIN_PATH`] answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginStart {
    /// Secret the client polls with; never shown to the user.
    pub device_code: String,
    /// Short code the user checks in the browser, like `ABCD-2345`.
    pub user_code: String,
    /// Seconds to wait between polls.
    pub interval: u64,
    /// Seconds until the codes expire.
    pub expires_in: u64,
}

/// The JSON body a client sends to [`LOGIN_POLL_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginPollRequest {
    /// [`LoginStart::device_code`].
    pub device_code: String,
}

/// The token a finished sign-in hands the client. `Debug` hides the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginGrant {
    /// The email address of the user who approved the sign-in.
    pub email: String,
    /// A new access token for that user.
    pub token: String,
}

impl std::fmt::Debug for LoginGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginGrant")
            .field("email", &self.email)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Where a sign-in stands: the JSON body [`LOGIN_POLL_PATH`] answers,
/// tagged by `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginPoll {
    /// The user has not approved the code yet.
    Pending,
    /// The user approved; the token is handed out once.
    Authorized(LoginGrant),
    /// The user declined the code.
    Denied,
    /// The code expired, was already used, or never existed.
    Expired,
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

    /// The web page where the user approves `user_code`.
    ///
    /// Built from the URL the client used rather than one the server names, so
    /// it works behind proxies and never sends the user somewhere unexpected.
    #[must_use]
    pub fn authorize_url(&self, user_code: &str) -> String {
        let code: String = url::form_urlencoded::byte_serialize(user_code.as_bytes()).collect();
        format!("{}{AUTHORIZE_PATH}?code={code}", self.root_url())
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

/// The browser sign-in to one `grit serve` host.
pub struct LoginFlow<'a> {
    http: &'a dyn HttpClient,
    server: &'a ServerUrl,
}

impl<'a> LoginFlow<'a> {
    /// A sign-in to `server`, sending requests through `http` (which needs no
    /// credentials: these endpoints are open).
    #[must_use]
    pub fn new(http: &'a dyn HttpClient, server: &'a ServerUrl) -> Self {
        Self { http, server }
    }

    /// Ask the server for a login code.
    ///
    /// # Errors
    ///
    /// [`ServerAuthError::Http`] when the request fails, and
    /// [`ServerAuthError::NotGritServer`] when the answer is not a
    /// [`LoginStart`].
    pub fn start(&self) -> std::result::Result<LoginStart, ServerAuthError> {
        let url = format!("{}{LOGIN_PATH}", self.server.root_url());
        let body = self
            .http
            .post(&url, JSON_CONTENT_TYPE, JSON_CONTENT_TYPE, b"{}", None)
            .map_err(ServerAuthError::Http)?;
        serde_json::from_slice(&body)
            .map_err(|_| ServerAuthError::NotGritServer(self.server.root_url()))
    }

    /// Ask once whether the user has approved `login`.
    ///
    /// # Errors
    ///
    /// [`ServerAuthError::Http`] when the request fails, and
    /// [`ServerAuthError::NotGritServer`] when the answer is not a
    /// [`LoginPoll`].
    pub fn poll(&self, login: &LoginStart) -> std::result::Result<LoginPoll, ServerAuthError> {
        let url = format!("{}{LOGIN_POLL_PATH}", self.server.root_url());
        let request = serde_json::to_vec(&LoginPollRequest {
            device_code: login.device_code.clone(),
        })
        .map_err(|_| ServerAuthError::NotGritServer(self.server.root_url()))?;
        let body = self
            .http
            .post(&url, JSON_CONTENT_TYPE, JSON_CONTENT_TYPE, &request, None)
            .map_err(ServerAuthError::Http)?;
        serde_json::from_slice(&body)
            .map_err(|_| ServerAuthError::NotGritServer(self.server.root_url()))
    }

    /// Poll until the user approves `login`, and return the token.
    ///
    /// Waits `login.interval` seconds (at least one) before every poll and
    /// gives up once `login.expires_in` seconds have passed.
    ///
    /// # Errors
    ///
    /// [`ServerAuthError::LoginDenied`] when the user declines,
    /// [`ServerAuthError::LoginExpired`] when the code expires first, and the
    /// errors of [`poll`](Self::poll).
    pub fn wait(
        &self,
        login: &LoginStart,
        clock: &mut dyn PollClock,
    ) -> std::result::Result<LoginGrant, ServerAuthError> {
        let deadline = clock.now() + Duration::from_secs(login.expires_in);
        let interval = Duration::from_secs(login.interval).max(MIN_POLL_INTERVAL);
        loop {
            clock.sleep(interval);
            if clock.now() >= deadline {
                return Err(ServerAuthError::LoginExpired);
            }
            match self.poll(login)? {
                LoginPoll::Pending => {}
                LoginPoll::Authorized(grant) => return Ok(grant),
                LoginPoll::Denied => return Err(ServerAuthError::LoginDenied),
                LoginPoll::Expired => return Err(ServerAuthError::LoginExpired),
            }
        }
    }
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
        let http = Canned::new(Ok(br#"{"email":"bob@example.com","admin":false}"#.to_vec()));
        let server = ServerUrl::parse("http://127.0.0.1:8080").unwrap();
        let identity = fetch_identity(&http, &server).unwrap();
        assert_eq!(
            identity,
            ServerIdentity {
                email: "bob@example.com".to_owned(),
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

    /// Answers POSTs from a script, in order, and records each URL and body.
    struct Scripted {
        answers: Mutex<Vec<Result<Vec<u8>>>>,
        seen: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<Vec<u8>>>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().rev().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpClient for Scripted {
        fn get(&self, _url: &str, _git_protocol: Option<&str>) -> Result<Vec<u8>> {
            unreachable!("the login flow only POSTs")
        }

        fn post(
            &self,
            url: &str,
            content_type: &str,
            _accept: &str,
            body: &[u8],
            _git_protocol: Option<&str>,
        ) -> Result<Vec<u8>> {
            assert_eq!(content_type, "application/json");
            self.seen
                .lock()
                .unwrap()
                .push((url.to_owned(), body.to_vec()));
            self.answers.lock().unwrap().pop().unwrap()
        }
    }

    struct FakeClock {
        start: std::time::Instant,
        elapsed: Duration,
        sleeps: usize,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                start: std::time::Instant::now(),
                elapsed: Duration::ZERO,
                sleeps: 0,
            }
        }
    }

    impl PollClock for FakeClock {
        fn now(&self) -> std::time::Instant {
            self.start + self.elapsed
        }

        fn sleep(&mut self, duration: Duration) {
            self.elapsed += duration;
            self.sleeps += 1;
        }
    }

    fn login() -> LoginStart {
        LoginStart {
            device_code: "dev123".to_owned(),
            user_code: "ABCD-2345".to_owned(),
            interval: 0,
            expires_in: 60,
        }
    }

    #[test]
    fn the_authorize_url_is_built_from_the_client_url() {
        let server = ServerUrl::parse("http://127.0.0.1:8080/app.git").unwrap();
        assert_eq!(
            server.authorize_url("ABCD-2345"),
            "http://127.0.0.1:8080/.grit/authorize?code=ABCD-2345"
        );
        assert_eq!(
            server.authorize_url("a&b"),
            "http://127.0.0.1:8080/.grit/authorize?code=a%26b"
        );
    }

    #[test]
    fn login_starts_then_polls_until_authorized() {
        let server = ServerUrl::parse("http://127.0.0.1:8080").unwrap();
        let start = serde_json::to_vec(&login()).unwrap();
        let http = Scripted::new(vec![
            Ok(start),
            Ok(br#"{"status":"pending"}"#.to_vec()),
            Ok(br#"{"status":"authorized","email":"bob@example.com","token":"grit_t"}"#.to_vec()),
        ]);
        let flow = LoginFlow::new(&http, &server);
        let started = flow.start().unwrap();
        assert_eq!(started, login());
        let mut clock = FakeClock::new();
        let grant = flow.wait(&started, &mut clock).unwrap();
        assert_eq!(grant.email, "bob@example.com");
        assert_eq!(grant.token, "grit_t");
        assert!(!format!("{grant:?}").contains("grit_t"));
        assert_eq!(clock.sleeps, 2, "one wait before each poll");
        assert!(
            clock.elapsed >= Duration::from_secs(2),
            "polls at least a second apart"
        );

        let seen = http.seen.lock().unwrap();
        assert_eq!(seen[0].0, "http://127.0.0.1:8080/.grit/login");
        assert_eq!(seen[1].0, "http://127.0.0.1:8080/.grit/login/poll");
        let request: LoginPollRequest = serde_json::from_slice(&seen[1].1).unwrap();
        assert_eq!(request.device_code, "dev123");
    }

    #[test]
    fn login_failures_are_typed() {
        let server = ServerUrl::parse("http://127.0.0.1:8080").unwrap();
        let wait = |answer: &[u8]| {
            let http = Scripted::new(vec![Ok(answer.to_vec())]);
            LoginFlow::new(&http, &server).wait(&login(), &mut FakeClock::new())
        };
        assert!(matches!(
            wait(br#"{"status":"denied"}"#),
            Err(ServerAuthError::LoginDenied)
        ));
        assert!(matches!(
            wait(br#"{"status":"expired"}"#),
            Err(ServerAuthError::LoginExpired)
        ));
        assert!(matches!(
            wait(b"<html>"),
            Err(ServerAuthError::NotGritServer(_))
        ));

        let http = Scripted::new(vec![Err(Error::Message("HTTP 404".to_owned()))]);
        assert!(matches!(
            LoginFlow::new(&http, &server).start(),
            Err(ServerAuthError::Http(_))
        ));
    }

    #[test]
    fn login_gives_up_when_the_code_expires_locally() {
        let server = ServerUrl::parse("http://127.0.0.1:8080").unwrap();
        let http = Scripted::new(
            (0..3)
                .map(|_| Ok(br#"{"status":"pending"}"#.to_vec()))
                .collect(),
        );
        let mut short = login();
        short.expires_in = 3;
        let result = LoginFlow::new(&http, &server).wait(&short, &mut FakeClock::new());
        assert!(matches!(result, Err(ServerAuthError::LoginExpired)));
        assert_eq!(http.seen.lock().unwrap().len(), 2);
    }
}
