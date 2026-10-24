//! `grit auth` — sign in to GitHub with the OAuth **Device Flow** and store the
//! resulting token in Git's credential store.
//!
//! The protocol lives in [`grit_lib::remote::github_auth`]: `grit` asks GitHub
//! for a device code, tells you to enter the short user code at
//! <https://github.com/login/device>, then waits until you authorize. The
//! access token GitHub returns is handed to the configured `credential.helper`
//! (`approve`), so a later `grit push` / `grit fetch` over HTTPS to github.com
//! authenticates with it as the HTTP password.
//!
//! There is no intermediate service: every request goes straight to github.com.
//!
//! ## `grit serve` hosts
//!
//! `grit auth <url>` signs in to a `grit serve` host instead. Its admin hands
//! out tokens (`grit serve add-user`), so there is no browser step: the token is
//! checked against the server and stored for the URL's scheme and host with the
//! same credential helper (see [`grit_lib::remote::grit_server_auth`]).
//!
//! ## Client id
//!
//! The flow needs the client id of a registered GitHub OAuth App. It is read,
//! in order, from `$GRIT_GITHUB_CLIENT_ID`, the `grit.githubClientId` config key,
//! then the baked-in [`DEFAULT_GITHUB_CLIENT_ID`]. Because the device flow uses
//! no secret, the client id is not sensitive and is safe to ship.

use std::io::{IsTerminal, Write};

use anyhow::{anyhow, bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::credentials::Credential;
use grit_lib::remote::credentials::{CredentialProvider, HelperCredentialProvider};
use grit_lib::remote::github_auth::{
    github_token_credential, is_github_https_url, DeviceFlow, DeviceFlowError, DeviceFlowStep,
    SystemPollClock, GITHUB_HOST,
};
use grit_lib::remote::grit_server_auth::{
    fetch_identity, ServerAuthError, ServerUrl, TokenCredentialProvider,
};
use grit_lib::remote::http_client::UreqHttpClient;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit auth` (sign-in).
#[derive(Serialize, JsonSchema)]
pub struct AuthOutcome {
    pub authenticated: bool,
    pub host: String,
    /// The user the token belongs to on a `grit serve` host (absent for GitHub).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl HumanRender for AuthOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match &self.user {
            Some(user) => outln!(w, "✓ Signed in to {} as {user} — token stored.", self.host),
            None => outln!(
                w,
                "\n✓ Signed in to GitHub — token stored for {}.",
                self.host
            ),
        }
        Ok(())
    }
}

impl MarkdownRender for AuthOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_auth_outcome(w, self)?;
        Ok(())
    }
}

/// Stored credential fields reported by `grit auth logout --dry-run`.
#[derive(Serialize, JsonSchema)]
pub struct LogoutCredentialPreview {
    pub protocol: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// Result of `grit auth logout`.
#[derive(Serialize, JsonSchema)]
pub struct LogoutOutcome {
    pub logged_out: bool,
    pub host: String,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(skip_serializing_if = "is_false")]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub credential: Option<LogoutCredentialPreview>,
}

impl HumanRender for LogoutOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            if let Some(cred) = &self.credential {
                outln!(
                    w,
                    "Dry run: would remove stored credential for {} ({}) on {}.",
                    cred.host,
                    cred.username.as_deref().unwrap_or("(no username)"),
                    cred.protocol
                );
            } else {
                outln!(
                    w,
                    "Dry run: no stored credential for {} would be removed.",
                    self.host
                );
            }
            return Ok(());
        }
        if self.logged_out {
            let service = if self.host == GITHUB_HOST {
                "GitHub"
            } else {
                &self.host
            };
            outln!(
                w,
                "✓ Signed out of {service} — removed the stored token for {}.",
                self.host
            );
        } else {
            outln!(
                w,
                "No credential helper is configured, so there's no stored token for {} to remove.",
                self.host
            );
        }
        Ok(())
    }
}

impl MarkdownRender for LogoutOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_logout_outcome(w, self)?;
        Ok(())
    }
}

/// GitHub OAuth App client id used for the device flow. This is grit's
/// registered OAuth App; the device flow uses no client secret, so the id is not
/// sensitive and is safe to ship. Override with `$GRIT_GITHUB_CLIENT_ID` or the
/// `grit.githubClientId` config key.
const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23lin6WOpOsGuXSupZ";

/// Run the interactive `grit auth` device flow and store the token.
pub fn run() -> Result<AuthOutcome> {
    let mut config = load_config();
    let client_id = client_id(&config)?;

    // Make sure there's somewhere to put the token before we send the user off
    // to authorize — otherwise they'd do the dance and we'd silently drop it.
    // On Windows we can set one up automatically; elsewhere we explain how.
    if !has_token_helper(&config, &github_token_credential(None)) {
        config = ensure_credential_helper()?;
    }

    let http = UreqHttpClient::from_config(&config).context("could not set up HTTP client")?;
    let flow = DeviceFlow::new(&http, &client_id);

    let device = flow.request_device_code().map_err(device_flow_error)?;

    // The device-code prompt is interactive UX, not command output: send it to
    // stderr so it's visible in both modes and never pollutes JSON on stdout.
    eprintln!("To authorize grit, open this page in your browser:\n");
    eprintln!("    {}\n", device.verification_uri);
    eprintln!("and enter the code:\n");
    eprintln!("    {}\n", device.user_code);
    eprintln!("Waiting for you to authorize… (press Ctrl-C to cancel)");

    let token = flow
        .wait_for_token(&device, &mut SystemPollClock)
        .map_err(device_flow_error)?;
    HelperCredentialProvider::new(config)
        .approve(&github_token_credential(Some(&token)))
        .context("storing the token with your credential helper")?;

    Ok(AuthOutcome {
        authenticated: true,
        host: GITHUB_HOST.to_owned(),
        user: None,
    })
}

/// The `grit serve` host `url` names, or `None` when it is GitHub (which signs
/// in with the device flow instead).
///
/// # Errors
///
/// Fails when `url` is not an `http`/`https` URL with a host.
pub fn grit_server(url: &str) -> Result<Option<ServerUrl>> {
    let server = ServerUrl::parse(url).map_err(|e| anyhow!("{e}"))?;
    let host = server.host().split(':').next().unwrap_or_default();
    let is_github =
        host.eq_ignore_ascii_case(GITHUB_HOST) || host.eq_ignore_ascii_case("www.github.com");
    Ok((!is_github).then_some(server))
}

/// Run `grit auth <url>` for a `grit serve` host: check `token` against the
/// server, then store it with the credential helper under the server's scheme
/// and host, with the server's user name.
///
/// # Errors
///
/// Fails when no credential helper is configured, the server rejects the token
/// or cannot be reached, or the helper cannot store it.
pub fn run_server(server: &ServerUrl, token: &str) -> Result<AuthOutcome> {
    let mut config = load_config();
    let lookup = server.credential(None, None);
    if !has_token_helper(&config, &lookup) {
        config = ensure_credential_helper()?;
    }

    let http = UreqHttpClient::from_config(&config)
        .context("could not set up HTTP client")?
        .with_credential_provider(Box::new(TokenCredentialProvider::new(token)));
    let identity = fetch_identity(&http, server).map_err(|e| match e {
        ServerAuthError::TokenRejected => anyhow!(
            "{} rejected the token. Ask the server's admin for one (`grit serve add-user`).",
            server.root_url()
        ),
        ServerAuthError::Http(source) => {
            anyhow::Error::new(source).context(format!("signing in to {}", server.root_url()))
        }
        other => anyhow::Error::new(other),
    })?;

    HelperCredentialProvider::new(config)
        .approve(&server.credential(Some(&identity.name), Some(token)))
        .context("storing the token with your credential helper")?;

    Ok(AuthOutcome {
        authenticated: true,
        host: server.host().to_owned(),
        user: Some(identity.name),
    })
}

/// Run `grit auth logout`: erase the token `grit auth` stored, via the configured
/// `credential.helper` (`erase`). Without `server` this removes the same
/// credential `grit auth` writes — `https://x-access-token@github.com`; with a
/// `grit serve` host it removes whatever is stored for that scheme and host. A
/// later push/fetch then no longer authenticates with it.
pub fn logout(server: Option<&ServerUrl>, dry_run: bool) -> Result<LogoutOutcome> {
    let config = load_config();
    let (lookup, host) = match server {
        Some(server) => (server.credential(None, None), server.host().to_owned()),
        None => (github_token_credential(None), GITHUB_HOST.to_owned()),
    };

    // Without a helper there's nothing that could be holding the token.
    if !has_token_helper(&config, &lookup) {
        return Ok(LogoutOutcome {
            logged_out: false,
            host,
            dry_run,
            credential: None,
        });
    }

    if dry_run {
        let filled = HelperCredentialProvider::new(config.clone())
            .fill(&lookup)
            .ok()
            .filter(|c| c.password.as_ref().is_some_and(|p| !p.is_empty()));
        let credential = filled.map(|c| LogoutCredentialPreview {
            protocol: c.protocol.unwrap_or_else(|| "https".to_owned()),
            host: c.host.unwrap_or_else(|| host.clone()),
            username: c.username,
        });
        return Ok(LogoutOutcome {
            logged_out: credential.is_some(),
            host,
            dry_run: true,
            credential,
        });
    }

    HelperCredentialProvider::new(config)
        .reject(&lookup)
        .context("removing the token with your credential helper")?;

    Ok(LogoutOutcome {
        logged_out: true,
        host,
        dry_run: false,
        credential: None,
    })
}

/// The token for `grit auth <url>`: `--token` when given, otherwise a hidden
/// prompt on a terminal, otherwise the first line of stdin.
///
/// # Errors
///
/// Fails when the token cannot be read or is empty.
pub fn read_token(token: Option<String>) -> Result<String> {
    let token = match token {
        Some(token) => token,
        None if std::io::stdin().is_terminal() => {
            rpassword::prompt_password("Token (from `grit serve add-user`): ")
                .context("reading the token")?
        }
        None => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("reading the token from stdin")?;
            line
        }
    };
    let token = token.trim();
    if token.is_empty() {
        bail!("no token given");
    }
    Ok(token.to_owned())
}

/// Offer to re-authenticate after a push/fetch failed with an auth error.
///
/// Returns `Ok(true)` when the user re-authenticated successfully and the caller
/// should retry the operation. Returns `Ok(false)` when re-auth doesn't apply — the
/// error wasn't an auth failure, the remote isn't an HTTPS github.com remote, there's
/// no TTY to prompt on, or the user declined; the error's own hint then names
/// `grit auth`.
pub fn offer_reauth(err: &anyhow::Error, remote_url: &str) -> Result<bool> {
    if !is_auth_error(err) || !is_github_https_url(remote_url) || !std::io::stdin().is_terminal() {
        return Ok(false);
    }

    eprintln!("\nAuthentication to {GITHUB_HOST} failed — your token may be missing or expired.");
    if !confirm("Sign in to GitHub now? [Y/n] ")? {
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
    let structured_auth = crate::json_error::structured_json_payload(err).is_some_and(|p| {
        matches!(
            p.kind,
            crate::error_kind::ErrorKind::AuthRequired | crate::error_kind::ErrorKind::AuthRejected
        )
    });
    structured_auth
        || err.chain().any(|cause| {
            cause
                .downcast_ref::<grit_lib::error::Error>()
                .is_some_and(|e| matches!(e, grit_lib::error::Error::Auth { .. }))
        })
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
    if let Ok(repo) = Repository::discover(
        None,
        &grit_lib::config::environment::RepositoryOptions::with_environment(env.clone()),
    ) {
        if let Ok(config) = ConfigSet::load(repo.environment(), Some(repo.git_dir()), true) {
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

/// Whether any `credential.helper` would receive `credential`.
fn has_token_helper(config: &ConfigSet, credential: &Credential) -> bool {
    !HelperCredentialProvider::new(config.clone())
        .helpers_for(credential)
        .is_empty()
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

/// Render a device-flow failure for the terminal.
fn device_flow_error(err: DeviceFlowError) -> anyhow::Error {
    match err {
        DeviceFlowError::Http { step, source } => {
            let doing = match step {
                DeviceFlowStep::RequestDeviceCode => "requesting a device code from GitHub",
                DeviceFlowStep::PollToken => "polling GitHub for the access token",
            };
            anyhow::Error::new(source).context(doing)
        }
        DeviceFlowError::DeviceCodeRejected(code) => {
            anyhow!("GitHub rejected the device-code request: {code}")
        }
        DeviceFlowError::MissingField(field) => anyhow!("GitHub response had no {field}"),
        DeviceFlowError::TimedOut => {
            anyhow!("timed out waiting for authorization. Run `grit auth` to try again.")
        }
        DeviceFlowError::Expired => {
            anyhow!("the code expired before you authorized. Run `grit auth` to try again.")
        }
        DeviceFlowError::AccessDenied => anyhow!("authorization was denied on GitHub."),
        DeviceFlowError::TokenRejected(code) => {
            anyhow!("GitHub returned an error while authorizing: {code}")
        }
        DeviceFlowError::UnexpectedResponse => {
            anyhow!("unexpected response from GitHub while polling for the token")
        }
        other => anyhow::Error::new(other),
    }
}
