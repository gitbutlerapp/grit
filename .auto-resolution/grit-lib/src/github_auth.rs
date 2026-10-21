//! GitHub sign-in with the OAuth 2.0 **device authorization grant** (RFC 8628),
//! plus the credential shape that makes the resulting token usable for Git over
//! HTTPS.
//!
//! The device flow needs no client secret, so it suits command-line and
//! headless programs:
//!
//! 1. [`DeviceFlow::request_device_code`] asks GitHub for a device code and a
//!    short user code the person enters at the verification page.
//! 2. [`DeviceFlow::wait_for_token`] polls the token endpoint, honoring the
//!    server's polling interval and `slow_down` back-off, until the person
//!    authorizes, denies, or the code expires. [`DeviceFlow::poll`] performs a
//!    single poll for callers that drive their own loop.
//! 3. [`github_token_credential`] builds the [`Credential`] a
//!    [`CredentialProvider`](crate::credentials::CredentialProvider) should
//!    `approve` (store) or `reject` (erase), so later HTTPS fetches and pushes
//!    to `github.com` send the token as the password.
//!
//! Every request goes through the caller's [`HttpClient`], and waiting goes
//! through a [`PollClock`], so the whole flow runs against fakes in tests.
//! Nothing here prompts or prints: showing the user code is the caller's job.

use std::time::{Duration, Instant};

use thiserror::Error;

use crate::credentials::Credential;
use crate::error::Error;
use crate::transport::http::HttpClient;

/// Host name of GitHub's web and Git endpoints.
pub const GITHUB_HOST: &str = "github.com";

/// GitHub endpoint that issues device and user codes.
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// GitHub endpoint polled for the access token.
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Page where the user enters the code; used when GitHub omits
/// `verification_uri` from its response.
pub const VERIFICATION_URL: &str = "https://github.com/login/device";

/// OAuth scope requested by default. `repo` covers fetching and pushing
/// private repositories over HTTPS.
pub const DEFAULT_SCOPE: &str = "repo";

/// Placeholder username GitHub documents for token authentication over HTTPS;
/// GitHub authenticates by the token (the password) regardless of the name.
pub const TOKEN_USERNAME: &str = "x-access-token";

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_EXPIRES_IN: Duration = Duration::from_secs(900);
/// Extra wait RFC 8628 asks for when the server answers `slow_down` without
/// naming a new interval.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// The device and user codes GitHub issued in step 1 of the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    /// Opaque code sent back while polling; never shown to the user.
    pub device_code: String,
    /// Short code the user types on the verification page (e.g. `WDJB-MJHT`).
    pub user_code: String,
    /// Page where the user enters [`user_code`](Self::user_code).
    pub verification_uri: String,
    /// Minimum wait between polls (GitHub's `interval`, default 5 seconds).
    pub interval: Duration,
    /// How long the codes stay valid (GitHub's `expires_in`, default 15 minutes).
    pub expires_in: Duration,
}

/// Result of one poll of the token endpoint that did not end the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollStatus {
    /// The user authorized the app; carries the access token.
    Authorized(String),
    /// The user has not finished authorizing yet; poll again after the same
    /// interval.
    Pending,
    /// The server asked the client to poll less often; poll again after
    /// `interval`.
    SlowDown {
        /// The new minimum wait between polls.
        interval: Duration,
    },
}

/// Which request of the flow failed at the transport level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceFlowStep {
    /// The device-code request ([`DEVICE_CODE_URL`]).
    RequestDeviceCode,
    /// A poll of the token endpoint ([`ACCESS_TOKEN_URL`]).
    PollToken,
}

/// Ways the device flow can fail.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum DeviceFlowError {
    /// The HTTP request itself failed (network, TLS, non-success status).
    #[error("device flow HTTP request failed during {step:?}")]
    Http {
        /// Which request failed.
        step: DeviceFlowStep,
        /// The transport error from the [`HttpClient`].
        #[source]
        source: Error,
    },
    /// GitHub answered the device-code request with an OAuth `error` code
    /// (for example an unknown client id).
    #[error("device-code request rejected: {0}")]
    DeviceCodeRejected(String),
    /// A required field was absent from GitHub's device-code response.
    #[error("device-code response is missing `{0}`")]
    MissingField(&'static str),
    /// The device code expired before the user authorized (`expired_token`).
    #[error("device code expired before authorization")]
    Expired,
    /// The user declined to authorize the app (`access_denied`).
    #[error("authorization was denied")]
    AccessDenied,
    /// [`DeviceAuthorization::expires_in`] elapsed while waiting.
    #[error("timed out waiting for authorization")]
    TimedOut,
    /// The token endpoint answered with an OAuth `error` code the flow does not
    /// handle (for example `incorrect_device_code`).
    #[error("token request failed: {0}")]
    TokenRejected(String),
    /// The token endpoint answered with neither a token nor an `error` code.
    #[error("unexpected response from the token endpoint")]
    UnexpectedResponse,
}

/// Time source and sleeper for [`DeviceFlow::wait_for_token`].
///
/// Injected so tests can run the poll loop without real waiting; production
/// callers use [`SystemPollClock`].
pub trait PollClock {
    /// The current instant.
    fn now(&self) -> Instant;
    /// Block for `duration` before the next poll.
    fn sleep(&mut self, duration: Duration);
}

/// [`PollClock`] backed by [`Instant::now`] and [`std::thread::sleep`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPollClock;

impl PollClock for SystemPollClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A GitHub OAuth device-flow client for one OAuth App.
pub struct DeviceFlow<'a> {
    http: &'a dyn HttpClient,
    client_id: &'a str,
    scope: &'a str,
}

impl<'a> DeviceFlow<'a> {
    /// Create a flow for the OAuth App `client_id`, sending requests through
    /// `http` and requesting [`DEFAULT_SCOPE`].
    #[must_use]
    pub fn new(http: &'a dyn HttpClient, client_id: &'a str) -> Self {
        Self {
            http,
            client_id,
            scope: DEFAULT_SCOPE,
        }
    }

    /// Request `scope` (space-separated OAuth scopes) instead of
    /// [`DEFAULT_SCOPE`].
    #[must_use]
    pub fn with_scope(mut self, scope: &'a str) -> Self {
        self.scope = scope;
        self
    }

    /// Step 1: ask GitHub for a device code and the user code to display.
    ///
    /// Missing `interval` / `expires_in` fall back to 5 seconds / 15 minutes,
    /// and a missing `verification_uri` to [`VERIFICATION_URL`].
    ///
    /// # Errors
    ///
    /// [`DeviceFlowError::Http`] when the request fails,
    /// [`DeviceFlowError::DeviceCodeRejected`] when GitHub returns an OAuth
    /// error, and [`DeviceFlowError::MissingField`] when `device_code` or
    /// `user_code` is absent.
    pub fn request_device_code(&self) -> Result<DeviceAuthorization, DeviceFlowError> {
        let body = form_encode(&[("client_id", self.client_id), ("scope", self.scope)]);
        let fields = self.post_form(DEVICE_CODE_URL, &body, DeviceFlowStep::RequestDeviceCode)?;
        if let Some(code) = form_field(&fields, "error") {
            return Err(DeviceFlowError::DeviceCodeRejected(code.to_owned()));
        }
        let required = |key: &'static str| {
            form_field(&fields, key)
                .map(str::to_owned)
                .ok_or(DeviceFlowError::MissingField(key))
        };
        Ok(DeviceAuthorization {
            device_code: required("device_code")?,
            user_code: required("user_code")?,
            verification_uri: form_field(&fields, "verification_uri")
                .unwrap_or(VERIFICATION_URL)
                .to_owned(),
            interval: seconds_field(&fields, "interval").unwrap_or(DEFAULT_INTERVAL),
            expires_in: seconds_field(&fields, "expires_in").unwrap_or(DEFAULT_EXPIRES_IN),
        })
    }

    /// Poll the token endpoint once for `device`.
    ///
    /// `current_interval` is the wait in effect; it is the base for the
    /// RFC 8628 back-off when GitHub answers `slow_down` without a new
    /// `interval`.
    ///
    /// # Errors
    ///
    /// [`DeviceFlowError::Http`] when the request fails,
    /// [`DeviceFlowError::Expired`] / [`DeviceFlowError::AccessDenied`] for the
    /// terminal OAuth errors, [`DeviceFlowError::TokenRejected`] for other
    /// OAuth errors, and [`DeviceFlowError::UnexpectedResponse`] when the
    /// response carries neither a token nor an error.
    pub fn poll(
        &self,
        device: &DeviceAuthorization,
        current_interval: Duration,
    ) -> Result<PollStatus, DeviceFlowError> {
        let body = form_encode(&[
            ("client_id", self.client_id),
            ("device_code", &device.device_code),
            ("grant_type", DEVICE_CODE_GRANT_TYPE),
        ]);
        let fields = self.post_form(ACCESS_TOKEN_URL, &body, DeviceFlowStep::PollToken)?;
        if let Some(token) = form_field(&fields, "access_token").filter(|t| !t.is_empty()) {
            return Ok(PollStatus::Authorized(token.to_owned()));
        }
        match form_field(&fields, "error") {
            Some("authorization_pending") => Ok(PollStatus::Pending),
            Some("slow_down") => Ok(PollStatus::SlowDown {
                interval: seconds_field(&fields, "interval")
                    .unwrap_or(current_interval + SLOW_DOWN_STEP),
            }),
            Some("expired_token") => Err(DeviceFlowError::Expired),
            Some("access_denied") => Err(DeviceFlowError::AccessDenied),
            Some(other) => Err(DeviceFlowError::TokenRejected(other.to_owned())),
            None => Err(DeviceFlowError::UnexpectedResponse),
        }
    }

    /// Step 2: poll until the user authorizes and return the access token.
    ///
    /// Waits at least `device.interval` (never less than one second) before
    /// every poll, adopts the longer interval on `slow_down`, and gives up once
    /// `device.expires_in` has elapsed since the call started.
    ///
    /// # Errors
    ///
    /// [`DeviceFlowError::TimedOut`] when the codes expire locally, plus every
    /// error [`poll`](Self::poll) returns.
    pub fn wait_for_token(
        &self,
        device: &DeviceAuthorization,
        clock: &mut dyn PollClock,
    ) -> Result<String, DeviceFlowError> {
        let deadline = clock.now() + device.expires_in;
        let mut interval = device.interval.max(Duration::from_secs(1));
        loop {
            clock.sleep(interval);
            if clock.now() >= deadline {
                return Err(DeviceFlowError::TimedOut);
            }
            match self.poll(device, interval)? {
                PollStatus::Authorized(token) => return Ok(token),
                PollStatus::Pending => {}
                PollStatus::SlowDown { interval: next } => interval = next,
            }
        }
    }

    fn post_form(
        &self,
        url: &str,
        body: &str,
        step: DeviceFlowStep,
    ) -> Result<Vec<(String, String)>, DeviceFlowError> {
        let response = self
            .http
            .post(
                url,
                FORM_CONTENT_TYPE,
                FORM_CONTENT_TYPE,
                body.as_bytes(),
                None,
            )
            .map_err(|source| DeviceFlowError::Http { step, source })?;
        Ok(parse_form(&String::from_utf8_lossy(&response)))
    }
}

/// The credential under which a GitHub token is stored for HTTPS Git access.
///
/// Pass `Some(token)` to build the credential to
/// [`approve`](crate::credentials::CredentialProvider::approve) (store), or
/// `None` for the credential to
/// [`reject`](crate::credentials::CredentialProvider::reject) (erase) — the
/// same protocol, host and [`TOKEN_USERNAME`], so erasing removes exactly what
/// storing wrote.
#[must_use]
pub fn github_token_credential(token: Option<&str>) -> Credential {
    Credential {
        protocol: Some("https".to_owned()),
        host: Some(GITHUB_HOST.to_owned()),
        username: Some(TOKEN_USERNAME.to_owned()),
        password: token.map(str::to_owned),
        ..Default::default()
    }
}

/// Whether `url` is an `https://` URL whose host is `github.com` (or
/// `www.github.com`), ignoring case, userinfo and port.
///
/// These are the only remotes where a token stored by
/// [`github_token_credential`] is sent as the HTTP password.
#[must_use]
pub fn is_github_https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host_port.split(':').next().unwrap_or_default();
    host.eq_ignore_ascii_case(GITHUB_HOST) || host.eq_ignore_ascii_case("www.github.com")
}

fn form_field<'f>(fields: &'f [(String, String)], key: &str) -> Option<&'f str> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn seconds_field(fields: &[(String, String)], key: &str) -> Option<Duration> {
    form_field(fields, key)
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
}

/// Encode key/value pairs as an `application/x-www-form-urlencoded` body.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_component_encode(k), form_component_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse an `application/x-www-form-urlencoded` body into ordered pairs.
fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (form_component_decode(k), form_component_decode(v)),
            None => (form_component_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-encode everything outside the RFC 3986 unreserved set; spaces
/// become `+`.
fn form_component_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decode `+` as space and `%XX` as a byte; malformed escapes stay literal.
fn form_component_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' => {
                let escaped = bytes
                    .get(idx + 1..idx + 3)
                    .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match escaped {
                    Some(byte) => {
                        out.push(byte);
                        idx += 3;
                    }
                    None => {
                        out.push(b'%');
                        idx += 1;
                    }
                }
            }
            other => {
                out.push(other);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// One recorded `POST`: URL and decoded form body.
    type Request = (String, Vec<(String, String)>);

    /// [`HttpClient`] that answers `POST`s from a script and records them.
    struct ScriptedHttp {
        responses: Mutex<VecDeque<crate::error::Result<String>>>,
        requests: Mutex<Vec<Request>>,
    }

    impl ScriptedHttp {
        fn new(responses: Vec<crate::error::Result<&str>>) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|r| r.map(str::to_owned))
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl HttpClient for ScriptedHttp {
        fn get(&self, url: &str, _git_protocol: Option<&str>) -> crate::error::Result<Vec<u8>> {
            panic!("unexpected GET {url}")
        }

        fn post(
            &self,
            url: &str,
            content_type: &str,
            accept: &str,
            body: &[u8],
            _git_protocol: Option<&str>,
        ) -> crate::error::Result<Vec<u8>> {
            assert_eq!(content_type, FORM_CONTENT_TYPE);
            assert_eq!(accept, FORM_CONTENT_TYPE);
            self.requests.lock().unwrap().push((
                url.to_owned(),
                parse_form(std::str::from_utf8(body).unwrap()),
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response")
                .map(String::into_bytes)
        }
    }

    /// [`PollClock`] whose time only advances when the flow sleeps.
    struct FakeClock {
        start: Instant,
        elapsed: Duration,
        sleeps: Vec<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: Duration::ZERO,
                sleeps: Vec::new(),
            }
        }
    }

    impl PollClock for FakeClock {
        fn now(&self) -> Instant {
            self.start + self.elapsed
        }

        fn sleep(&mut self, duration: Duration) {
            self.elapsed += duration;
            self.sleeps.push(duration);
        }
    }

    fn device(interval: u64, expires_in: u64) -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "dev-123".to_owned(),
            user_code: "WDJB-MJHT".to_owned(),
            verification_uri: VERIFICATION_URL.to_owned(),
            interval: Duration::from_secs(interval),
            expires_in: Duration::from_secs(expires_in),
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn form_round_trips_reserved_characters() {
        let encoded = form_encode(&[
            ("grant_type", DEVICE_CODE_GRANT_TYPE),
            ("scope", "repo read:org"),
            ("odd", "a&b=c%d~"),
        ]);
        assert_eq!(
            encoded,
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code\
             &scope=repo+read%3Aorg&odd=a%26b%3Dc%25d~"
        );
        let parsed = parse_form(&encoded);
        assert_eq!(
            form_field(&parsed, "grant_type"),
            Some(DEVICE_CODE_GRANT_TYPE)
        );
        assert_eq!(form_field(&parsed, "scope"), Some("repo read:org"));
        assert_eq!(form_field(&parsed, "odd"), Some("a&b=c%d~"));
    }

    #[test]
    fn form_decode_keeps_malformed_escapes_literal() {
        assert_eq!(form_component_decode("100%"), "100%");
        assert_eq!(form_component_decode("%zz%4"), "%zz%4");
        assert_eq!(form_component_decode("%+4"), "% 4");
        assert_eq!(form_component_decode("%41%62"), "Ab");
        let parsed = parse_form("flag&&k=v");
        assert_eq!(form_field(&parsed, "flag"), Some(""));
        assert_eq!(form_field(&parsed, "k"), Some("v"));
    }

    #[test]
    fn request_device_code_parses_github_response() {
        let http = ScriptedHttp::new(vec![Ok("device_code=3584d83&user_code=WDJB-MJHT\
             &verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice\
             &expires_in=600&interval=7")]);
        let flow = DeviceFlow::new(&http, "client-abc");
        let device = flow.request_device_code().unwrap();
        assert_eq!(
            device,
            DeviceAuthorization {
                device_code: "3584d83".to_owned(),
                user_code: "WDJB-MJHT".to_owned(),
                verification_uri: "https://github.com/login/device".to_owned(),
                interval: secs(7),
                expires_in: secs(600),
            }
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, DEVICE_CODE_URL);
        assert_eq!(form_field(&requests[0].1, "client_id"), Some("client-abc"));
        assert_eq!(form_field(&requests[0].1, "scope"), Some(DEFAULT_SCOPE));
    }

    #[test]
    fn request_device_code_defaults_optional_fields() {
        let http = ScriptedHttp::new(vec![Ok("device_code=d&user_code=u&interval=soon")]);
        let device = DeviceFlow::new(&http, "id")
            .with_scope("read:user")
            .request_device_code()
            .unwrap();
        assert_eq!(device.verification_uri, VERIFICATION_URL);
        assert_eq!(device.interval, DEFAULT_INTERVAL);
        assert_eq!(device.expires_in, DEFAULT_EXPIRES_IN);
        assert_eq!(
            form_field(&http.requests()[0].1, "scope"),
            Some("read:user")
        );
    }

    #[test]
    fn request_device_code_reports_rejection_and_missing_fields() {
        let http = ScriptedHttp::new(vec![
            Ok("error=unauthorized_client&error_description=nope"),
            Ok("user_code=u"),
            Ok("device_code=d"),
        ]);
        let flow = DeviceFlow::new(&http, "id");
        assert!(matches!(
            flow.request_device_code(),
            Err(DeviceFlowError::DeviceCodeRejected(code)) if code == "unauthorized_client"
        ));
        assert!(matches!(
            flow.request_device_code(),
            Err(DeviceFlowError::MissingField("device_code"))
        ));
        assert!(matches!(
            flow.request_device_code(),
            Err(DeviceFlowError::MissingField("user_code"))
        ));
    }

    #[test]
    fn transport_errors_name_the_failing_step() {
        let http = ScriptedHttp::new(vec![
            Err(Error::Auth("denied".to_owned())),
            Err(Error::Message("offline".to_owned())),
        ]);
        let flow = DeviceFlow::new(&http, "id");
        assert!(matches!(
            flow.request_device_code(),
            Err(DeviceFlowError::Http {
                step: DeviceFlowStep::RequestDeviceCode,
                source: Error::Auth(_),
            })
        ));
        assert!(matches!(
            flow.poll(&device(5, 900), secs(5)),
            Err(DeviceFlowError::Http {
                step: DeviceFlowStep::PollToken,
                source: Error::Message(_),
            })
        ));
    }

    #[test]
    fn poll_sends_device_grant_and_classifies_responses() {
        let http = ScriptedHttp::new(vec![
            Ok("error=authorization_pending"),
            Ok("error=slow_down&interval=12"),
            Ok("error=slow_down"),
            Ok("access_token=&error=authorization_pending"),
            Ok("access_token=gho_abc&token_type=bearer&scope=repo"),
        ]);
        let flow = DeviceFlow::new(&http, "id");
        let dev = device(5, 900);
        assert_eq!(flow.poll(&dev, secs(5)).unwrap(), PollStatus::Pending);
        assert_eq!(
            flow.poll(&dev, secs(5)).unwrap(),
            PollStatus::SlowDown { interval: secs(12) }
        );
        assert_eq!(
            flow.poll(&dev, secs(8)).unwrap(),
            PollStatus::SlowDown { interval: secs(13) }
        );
        assert_eq!(flow.poll(&dev, secs(5)).unwrap(), PollStatus::Pending);
        assert_eq!(
            flow.poll(&dev, secs(5)).unwrap(),
            PollStatus::Authorized("gho_abc".to_owned())
        );
        let request = &http.requests()[0];
        assert_eq!(request.0, ACCESS_TOKEN_URL);
        assert_eq!(form_field(&request.1, "client_id"), Some("id"));
        assert_eq!(form_field(&request.1, "device_code"), Some("dev-123"));
        assert_eq!(
            form_field(&request.1, "grant_type"),
            Some(DEVICE_CODE_GRANT_TYPE)
        );
    }

    #[test]
    fn poll_maps_terminal_oauth_errors() {
        let http = ScriptedHttp::new(vec![
            Ok("error=expired_token"),
            Ok("error=access_denied"),
            Ok("error=incorrect_device_code"),
            Ok("token_type=bearer"),
        ]);
        let flow = DeviceFlow::new(&http, "id");
        let dev = device(5, 900);
        assert!(matches!(
            flow.poll(&dev, secs(5)),
            Err(DeviceFlowError::Expired)
        ));
        assert!(matches!(
            flow.poll(&dev, secs(5)),
            Err(DeviceFlowError::AccessDenied)
        ));
        assert!(matches!(
            flow.poll(&dev, secs(5)),
            Err(DeviceFlowError::TokenRejected(code)) if code == "incorrect_device_code"
        ));
        assert!(matches!(
            flow.poll(&dev, secs(5)),
            Err(DeviceFlowError::UnexpectedResponse)
        ));
    }

    #[test]
    fn wait_for_token_honors_interval_and_slow_down() {
        let http = ScriptedHttp::new(vec![
            Ok("error=authorization_pending"),
            Ok("error=slow_down"),
            Ok("error=slow_down&interval=20"),
            Ok("access_token=gho_tok"),
        ]);
        let mut clock = FakeClock::new();
        let token = DeviceFlow::new(&http, "id")
            .wait_for_token(&device(5, 900), &mut clock)
            .unwrap();
        assert_eq!(token, "gho_tok");
        assert_eq!(clock.sleeps, vec![secs(5), secs(5), secs(10), secs(20)]);
        assert_eq!(http.requests().len(), 4);
    }

    #[test]
    fn wait_for_token_waits_at_least_one_second() {
        let http = ScriptedHttp::new(vec![Ok("access_token=t")]);
        let mut clock = FakeClock::new();
        DeviceFlow::new(&http, "id")
            .wait_for_token(&device(0, 900), &mut clock)
            .unwrap();
        assert_eq!(clock.sleeps, vec![secs(1)]);
    }

    #[test]
    fn wait_for_token_times_out_without_polling_past_expiry() {
        let http = ScriptedHttp::new(vec![
            Ok("error=authorization_pending"),
            Ok("error=authorization_pending"),
        ]);
        let mut clock = FakeClock::new();
        let result = DeviceFlow::new(&http, "id").wait_for_token(&device(5, 12), &mut clock);
        assert!(matches!(result, Err(DeviceFlowError::TimedOut)));
        assert_eq!(http.requests().len(), 2);
        assert_eq!(clock.sleeps, vec![secs(5), secs(5), secs(5)]);
    }

    #[test]
    fn wait_for_token_stops_on_denial() {
        let http = ScriptedHttp::new(vec![
            Ok("error=authorization_pending"),
            Ok("error=access_denied"),
        ]);
        let mut clock = FakeClock::new();
        let result = DeviceFlow::new(&http, "id").wait_for_token(&device(5, 900), &mut clock);
        assert!(matches!(result, Err(DeviceFlowError::AccessDenied)));
    }

    #[test]
    fn token_credential_targets_github_https() {
        let store = github_token_credential(Some("gho_x"));
        assert_eq!(store.protocol.as_deref(), Some("https"));
        assert_eq!(store.host.as_deref(), Some(GITHUB_HOST));
        assert_eq!(store.username.as_deref(), Some(TOKEN_USERNAME));
        assert_eq!(store.password.as_deref(), Some("gho_x"));
        assert!(store.is_complete());
        assert_eq!(
            store.target_url().as_deref(),
            Some("https://x-access-token@github.com")
        );

        let erase = github_token_credential(None);
        assert_eq!(erase.password, None);
        assert_eq!(erase.target_url(), store.target_url());
    }

    #[test]
    fn recognizes_https_github_urls() {
        assert!(is_github_https_url("https://github.com/owner/repo.git"));
        assert!(is_github_https_url(
            "https://x-access-token@github.com/owner/repo.git"
        ));
        assert!(is_github_https_url("https://user:pw@GitHub.com:443/o/r"));
        assert!(is_github_https_url("https://www.github.com/owner/repo"));
        assert!(is_github_https_url("https://github.com"));
        assert!(!is_github_https_url("https://gitlab.com/owner/repo.git"));
        assert!(!is_github_https_url("http://github.com/owner/repo.git"));
        assert!(!is_github_https_url("git@github.com:owner/repo.git"));
        assert!(!is_github_https_url("https://evil.com/github.com"));
        assert!(!is_github_https_url("https://github.com.evil.com/o/r"));
        assert!(!is_github_https_url("https://evil.com/x@github.com/o/r"));
    }
}
