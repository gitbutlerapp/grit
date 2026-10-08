//! `grit auth` — sign in to GitHub with the OAuth **Device Flow** and store the
//! resulting token in Git's credential store.
//!
//! The device flow needs no client secret (GitHub documents this for headless /
//! CLI apps): `grit` asks GitHub for a `device_code` + a short `user_code`, tells
//! you to enter the code at <https://github.com/login/device>, then polls GitHub
//! until you authorize. The access token GitHub returns is handed to the
//! configured `credential.helper` (`approve`), so a later `grit push` / `grit fetch`
//! over HTTPS to github.com authenticates with it as the HTTP password.
//!
//! There is no intermediate service: every request goes straight to github.com.
//!
//! ## Client id
//!
//! The flow needs the client id of a registered GitHub OAuth App. It is read,
//! in order, from `$GRIT_GITHUB_CLIENT_ID`, the `grit.githubClientId` config key,
//! then the baked-in [`DEFAULT_GITHUB_CLIENT_ID`]. Because the device flow uses
//! no secret, the client id is not sensitive and is safe to ship.

use std::io::{IsTerminal, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::credentials::{Credential, CredentialProvider, HelperCredentialProvider};
use grit_lib::repo::Repository;
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::HttpClient;
use serde::Serialize;

use crate::output::HumanRender;

/// Result of `grit auth` (sign-in).
#[derive(Serialize)]
pub struct AuthOutcome {
    pub authenticated: bool,
    pub host: String,
}

impl HumanRender for AuthOutcome {
    fn render_human(&self) {
        println!("\n✓ Signed in to GitHub — token stored for {}.", self.host);
    }
}

/// Result of `grit auth logout`.
#[derive(Serialize)]
pub struct LogoutOutcome {
    pub logged_out: bool,
    pub host: String,
}

impl HumanRender for LogoutOutcome {
    fn render_human(&self) {
        if self.logged_out {
            println!(
                "✓ Signed out of GitHub — removed the stored token for {}.",
                self.host
            );
        } else {
            println!(
                "No credential helper is configured, so there's no stored token for {} to remove.",
                self.host
            );
        }
    }
}

/// GitHub OAuth App client id used for the device flow. This is grit's
/// registered OAuth App; the device flow uses no client secret, so the id is not
/// sensitive and is safe to ship. Override with `$GRIT_GITHUB_CLIENT_ID` or the
/// `grit.githubClientId` config key.
const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23lin6WOpOsGuXSupZ";

const GITHUB_HOST: &str = "github.com";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const VERIFICATION_URL: &str = "https://github.com/login/device";
/// OAuth scope requested. `repo` covers private-repo fetch and push over HTTPS.
const SCOPE: &str = "repo";
const FORM: &str = "application/x-www-form-urlencoded";

/// Run the interactive `grit auth` device flow and store the token.
pub fn run() -> Result<AuthOutcome> {
    let mut config = load_config();
    let client_id = client_id(&config)?;

    // Make sure there's somewhere to put the token before we send the user off
    // to authorize — otherwise they'd do the dance and we'd silently drop it.
    // On Windows we can set one up automatically; elsewhere we explain how.
    if credential_helpers(&config).is_empty() {
        config = ensure_credential_helper()?;
    }

    let http = UreqHttpClient::from_config(&config).context("could not set up HTTP client")?;

    let device = request_device_code(&http, &client_id)?;

    // The device-code prompt is interactive UX, not command output: send it to
    // stderr so it's visible in both modes and never pollutes JSON on stdout.
    eprintln!("To authorize grit, open this page in your browser:\n");
    eprintln!("    {}\n", device.verification_uri);
    eprintln!("and enter the code:\n");
    eprintln!("    {}\n", device.user_code);
    eprintln!("Waiting for you to authorize… (press Ctrl-C to cancel)");

    let token = poll_for_token(&http, &client_id, &device)?;
    store_token(&config, &token)?;

    Ok(AuthOutcome {
        authenticated: true,
        host: GITHUB_HOST.to_owned(),
    })
}

/// Run `grit auth logout`: erase the token `grit auth` stored, via the configured
/// `credential.helper` (`erase`). This removes the same credential `grit auth`
/// writes — `https://x-access-token@github.com` — so a later push/fetch no longer
/// authenticates with it.
pub fn logout() -> Result<LogoutOutcome> {
    let config = load_config();

    // Without a helper there's nothing that could be holding the token.
    if credential_helpers(&config).is_empty() {
        return Ok(LogoutOutcome {
            logged_out: false,
            host: GITHUB_HOST.to_owned(),
        });
    }

    let cred = Credential {
        protocol: Some("https".to_owned()),
        host: Some(GITHUB_HOST.to_owned()),
        username: Some("x-access-token".to_owned()),
        ..Default::default()
    };
    HelperCredentialProvider::new(config)
        .reject(&cred)
        .context("removing the token with your credential helper")?;

    Ok(LogoutOutcome {
        logged_out: true,
        host: GITHUB_HOST.to_owned(),
    })
}

/// Offer to re-authenticate after a push/fetch failed with an auth error.
///
/// Returns `Ok(true)` when the user re-authenticated successfully and the caller
/// should retry the operation. Returns `Ok(false)` (after printing a hint) when
/// re-auth doesn't apply — the error wasn't an auth failure, the remote isn't an
/// HTTPS github.com remote, there's no TTY to prompt on, or the user declined.
pub fn offer_reauth(err: &anyhow::Error, remote_url: &str) -> Result<bool> {
    if !is_auth_error(err) || !is_https_github(remote_url) {
        return Ok(false);
    }

    eprintln!("\nAuthentication to {GITHUB_HOST} failed — your token may be missing or expired.");

    if !std::io::stdin().is_terminal() {
        eprintln!("Run `grit auth` to sign in to GitHub, then try again.");
        return Ok(false);
    }

    if !confirm("Sign in to GitHub now? [Y/n] ")? {
        eprintln!("Run `grit auth` to sign in to GitHub, then try again.");
        return Ok(false);
    }

    // Reauth runs mid push/fetch (outside the normal emit path), so confirm on
    // stderr — never stdout, which may be carrying a JSON outcome.
    let outcome = run()?;
    eprintln!(
        "\n✓ Signed in to GitHub — token stored for {}.",
        outcome.host
    );
    Ok(true)
}

/// Whether `err` (or a cause) is a transport-level HTTP authentication failure.
fn is_auth_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<grit_lib::error::Error>()
            .is_some_and(|e| matches!(e, grit_lib::error::Error::Auth(_)))
    })
}

/// A `https://…github.com…` remote (the only place a stored token is the HTTP
/// password, so the only place re-auth makes sense).
fn is_https_github(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    // Drop any `user[:pass]@` userinfo, then read the host up to `/`, `:`, or end.
    let after_userinfo = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    let host = after_userinfo
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == GITHUB_HOST || host == "www.github.com"
}

/// Prompt on the TTY for a yes/no answer (default yes on empty input).
fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading your answer")?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Load config from the surrounding repo when there is one (so repo-scoped
/// `credential.helper` applies), otherwise from the global/system files so
/// `grit auth` works anywhere.
fn load_config() -> ConfigSet {
    let env = crate::context::environment();
    if let Ok(repo) = Repository::discover_with(
        &grit_lib::environment::RepositoryOptions::with_environment(env.clone()),
        None,
    ) {
        if let Ok(config) = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true) {
            return config;
        }
    }
    ConfigSet::load(&env, None, true).unwrap_or_default()
}

/// Resolve the OAuth App client id (env, then config, then the baked-in value).
fn client_id(config: &ConfigSet) -> Result<String> {
    let from_env = std::env::var("GRIT_GITHUB_CLIENT_ID")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let from_config = config
        .get("grit.githubClientId")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let baked = (!DEFAULT_GITHUB_CLIENT_ID.is_empty()).then(|| DEFAULT_GITHUB_CLIENT_ID.to_owned());

    from_env.or(from_config).or(baked).context(
        "no GitHub OAuth client id configured.\n\
         Set it with `GRIT_GITHUB_CLIENT_ID=…` or `grit config --global grit.githubClientId …` \
         (your GitHub OAuth App's client id).",
    )
}

/// The `credential.helper` programs that apply to `https://github.com` — the
/// section-default helpers plus any URL-scoped `credential.<url>.helper` matches.
fn credential_helpers(config: &ConfigSet) -> Vec<String> {
    let url = format!("https://{GITHUB_HOST}");
    let mut helpers: Vec<String> = Vec::new();
    let push = |value: &str, helpers: &mut Vec<String>| {
        let value = value.trim().to_owned();
        if value.is_empty() {
            // An empty value resets the list, matching Git's helper semantics.
            helpers.clear();
        } else if !helpers.contains(&value) {
            helpers.push(value);
        }
    };
    for value in config.get_all("credential.helper") {
        push(&value, &mut helpers);
    }
    for (var, value, _scope) in
        grit_lib::config::get_urlmatch_all_in_section(config.entries(), "credential", &url)
    {
        if var.eq_ignore_ascii_case("helper") {
            push(&value, &mut helpers);
        }
    }
    helpers
}

/// Called when no `credential.helper` is configured. On Windows we wire one up
/// automatically — pointing `credential.helper` at the built-in `grit manager`
/// (Windows Credential Manager) — and return the reloaded config. Elsewhere we
/// bail with instructions, since storing the token would otherwise be silently
/// lost. We use `grit manager` rather than Git Credential Manager because the
/// latter only ships with Git for Windows, which may not be installed.
#[cfg(windows)]
fn ensure_credential_helper() -> Result<ConfigSet> {
    let exe = std::env::current_exe()
        .context("could not locate the grit executable to set up the credential helper")?;
    // Git accepts forward slashes in helper paths on Windows, and they avoid the
    // backslash-escaping pitfalls of the helper string being shell-split.
    let exe = exe.to_string_lossy().replace('\\', "/");
    let helper = format!("\"{exe}\" manager");
    crate::commands::config::set_global("credential.helper", &helper)
        .context("setting up the Windows credential helper")?;
    eprintln!("Set credential.helper to `grit manager` (Windows Credential Manager).");
    Ok(load_config())
}

#[cfg(not(windows))]
fn ensure_credential_helper() -> Result<ConfigSet> {
    bail!(
        "no credential helper is configured, so the token couldn't be stored.\n\
         Enable one first, for example:\n  \
         macOS:   grit config --global credential.helper osxkeychain\n  \
         Linux:   grit config --global credential.helper libsecret\n  \
         any:     grit config --global credential.helper store   (plaintext file)"
    )
}

/// The fields GitHub returns from the device-code request.
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

/// Step 1: ask GitHub for a device code and the user code to display.
fn request_device_code(http: &UreqHttpClient, client_id: &str) -> Result<DeviceCode> {
    let body = form_encode(&[("client_id", client_id), ("scope", SCOPE)]);
    let resp = http
        .post(DEVICE_CODE_URL, FORM, FORM, body.as_bytes(), None)
        .context("requesting a device code from GitHub")?;
    let fields = parse_form(&String::from_utf8_lossy(&resp));
    let get = |key: &str| field(&fields, key);

    if let Some(err) = get("error") {
        bail!("GitHub rejected the device-code request: {err}");
    }

    Ok(DeviceCode {
        device_code: get("device_code").context("GitHub response had no device_code")?,
        user_code: get("user_code").context("GitHub response had no user_code")?,
        verification_uri: get("verification_uri").unwrap_or_else(|| VERIFICATION_URL.to_owned()),
        interval: get("interval").and_then(|v| v.parse().ok()).unwrap_or(5),
        expires_in: get("expires_in")
            .and_then(|v| v.parse().ok())
            .unwrap_or(900),
    })
}

/// Step 2: poll the access-token endpoint until the user authorizes, honoring
/// GitHub's `interval` and `slow_down` back-off, until the code expires.
fn poll_for_token(http: &UreqHttpClient, client_id: &str, device: &DeviceCode) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval.max(1);
    let body = form_encode(&[
        ("client_id", client_id),
        ("device_code", &device.device_code),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ]);

    loop {
        // GitHub requires waiting at least `interval` seconds between polls.
        sleep(Duration::from_secs(interval));
        if Instant::now() >= deadline {
            bail!("timed out waiting for authorization. Run `grit auth` to try again.");
        }

        let resp = http
            .post(ACCESS_TOKEN_URL, FORM, FORM, body.as_bytes(), None)
            .context("polling GitHub for the access token")?;
        let fields = parse_form(&String::from_utf8_lossy(&resp));
        let get = |key: &str| field(&fields, key);

        if let Some(token) = get("access_token").filter(|t| !t.is_empty()) {
            return Ok(token);
        }

        match get("error").as_deref() {
            // Not authorized yet — keep polling at the current interval.
            Some("authorization_pending") => {}
            // Asked to back off: adopt the new interval (or add 5s per the spec).
            Some("slow_down") => {
                interval = get("interval")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(interval + 5);
            }
            Some("expired_token") => {
                bail!("the code expired before you authorized. Run `grit auth` to try again.")
            }
            Some("access_denied") => bail!("authorization was denied on GitHub."),
            Some(other) => bail!("GitHub returned an error while authorizing: {other}"),
            None => bail!("unexpected response from GitHub while polling for the token"),
        }
    }
}

/// Step 3: store the token via the configured `credential.helper` so Git uses it
/// as the HTTP password for github.com. The username `x-access-token` is GitHub's
/// documented placeholder; GitHub authenticates by the token regardless.
fn store_token(config: &ConfigSet, token: &str) -> Result<()> {
    let cred = Credential {
        protocol: Some("https".to_owned()),
        host: Some(GITHUB_HOST.to_owned()),
        username: Some("x-access-token".to_owned()),
        password: Some(token.to_owned()),
        ..Default::default()
    };
    HelperCredentialProvider::new(config.clone())
        .approve(&cred)
        .context("storing the token with your credential helper")
}

/// Look up a field from parsed form pairs.
fn field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// Encode key/value pairs as `application/x-www-form-urlencoded`.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse an `application/x-www-form-urlencoded` body into key/value pairs.
fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-encode a string for form bodies (encode everything outside the
/// unreserved set; spaces become `+`).
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decode a percent-encoded form component (`+` → space, `%XX` → byte).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' if idx + 2 < bytes.len() => {
                match (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        idx += 3;
                    }
                    _ => {
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

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_round_trips() {
        let encoded = form_encode(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", "abc123"),
        ]);
        let parsed = parse_form(&encoded);
        assert_eq!(field(&parsed, "client_id").as_deref(), Some("abc123"));
        assert_eq!(
            field(&parsed, "grant_type").as_deref(),
            Some("urn:ietf:params:oauth:grant-type:device_code")
        );
    }

    #[test]
    fn parses_github_device_response() {
        let body = "device_code=3584d83&user_code=WDJB-MJHT\
            &verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice&expires_in=900&interval=5";
        let fields = parse_form(body);
        assert_eq!(field(&fields, "user_code").as_deref(), Some("WDJB-MJHT"));
        assert_eq!(
            field(&fields, "verification_uri").as_deref(),
            Some("https://github.com/login/device")
        );
    }

    #[test]
    fn recognizes_https_github_remotes() {
        assert!(is_https_github("https://github.com/owner/repo.git"));
        assert!(is_https_github(
            "https://x-access-token@github.com/owner/repo.git"
        ));
        assert!(is_https_github("https://GitHub.com/owner/repo"));
        assert!(!is_https_github("https://gitlab.com/owner/repo.git"));
        assert!(!is_https_github("git@github.com:owner/repo.git"));
        assert!(!is_https_github("https://evil.com/github.com"));
    }
}
