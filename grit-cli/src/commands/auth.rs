//! `grit auth` — sign in to grithub or GitHub (with the OAuth **Device Flow**)
//! and store the resulting token in Git's credential store.
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
//! ## Which host
//!
//! Without a URL, `grit auth` signs in to GitHub only when the current
//! repository's `origin` is on github.com; everywhere else it signs in to
//! grithub ([`GRITHUB_URL`]), a `grit serve`-style host, as below
//! ([`default_host`]). `grit auth https://github.com` always means GitHub.
//!
//! ## `grit serve` hosts
//!
//! `grit auth <url>` (or `grit login <url>`) signs in to a `grit serve` host
//! instead, in the same style: it prints (and opens) the server's authorize
//! page, the user signs in there with their email address and password and
//! approves the code, and `grit` polls for the new token. The token is stored
//! for the URL's scheme and host with the same credential helper (see
//! [`grit_lib::remote::grit_server_auth`]). `--token` stores a token an admin
//! made with `grit serve token` instead, after checking it with the server.
//!
//! ## Any other HTTPS host
//!
//! `grit auth <url> --username <user> --token <token>` stores a user name and
//! token (or password) for the URL (its scheme and host, plus the path under
//! `credential.useHttpPath`) with the same credential
//! helper, without contacting the server: grit never prompts for a password,
//! so this is how a person saves one for GitLab, Gitea, Bitbucket or any other
//! Basic-auth host. The next push or fetch sends it, and a host that refuses it
//! has it erased.
//!
//! ## Credential helper
//!
//! Without a `credential.helper`, the token would have nowhere to go, so
//! `grit auth` sets one in the global config first: the platform's secure store
//! when it is installed (macOS keychain, libsecret), otherwise `store`, a
//! plaintext file, with a warning. On Windows it is the built-in `grit manager`.
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
    credential_for_url, fetch_identity, LoginFlow, ServerAuthError, ServerUrl,
    TokenCredentialProvider,
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
    /// The user the token belongs to on a `grit serve` host, or the `--username`
    /// it was stored for (absent for GitHub).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Whether the host confirmed the token. `false` for `--username`/`--token`,
    /// which stores it without contacting the host.
    pub verified: bool,
}

impl HumanRender for AuthOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match &self.user {
            Some(user) if !self.verified => outln!(
                w,
                "✓ Stored the token for {user} on {} — the next push or fetch uses it.",
                self.host
            ),
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

/// The host `grit auth` signs in to when no URL is given: grithub.
pub const GRITHUB_URL: &str = "https://grithub.dev";

/// Where `grit auth` (or `grit auth logout`) without a URL goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultHost {
    /// GitHub, with its device flow: the current repository's `origin` is on github.com.
    GitHub,
    /// A grit server: grithub, or `$GRIT_DEFAULT_HOST` when set.
    Grit(ServerUrl),
}

/// Pick the host for `grit auth` without a URL: GitHub when the current
/// repository's `origin` is a github.com URL, otherwise grithub
/// ([`GRITHUB_URL`], or the `GRIT_DEFAULT_HOST` environment variable).
///
/// # Errors
///
/// Fails when `GRIT_DEFAULT_HOST` is not an `http`/`https` URL.
pub fn default_host() -> Result<DefaultHost> {
    let config = load_config();
    if config
        .get("remote.origin.url")
        .is_some_and(|url| is_github_url(&url))
    {
        return Ok(DefaultHost::GitHub);
    }
    match grit_server(&default_grit_host_url())? {
        Some(server) => Ok(DefaultHost::Grit(server)),
        None => Ok(DefaultHost::GitHub),
    }
}

/// The grit server used when none is named: `$GRIT_DEFAULT_HOST`, else [`GRITHUB_URL`].
pub fn default_grit_host_url() -> String {
    crate::context::environment()
        .var("GRIT_DEFAULT_HOST")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| GRITHUB_URL.to_owned())
}

/// Whether `url` (HTTPS, `ssh://` or scp-style `git@github.com:owner/repo`) is on github.com.
fn is_github_url(url: &str) -> bool {
    if is_github_https_url(url) {
        return true;
    }
    let rest = url.strip_prefix("ssh://").unwrap_or(url);
    let authority = rest.split(['/', ':']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    host.eq_ignore_ascii_case(GITHUB_HOST) || host.eq_ignore_ascii_case("www.github.com")
}

/// Run the interactive `grit auth` device flow and store the token.
pub fn run() -> Result<AuthOutcome> {
    let mut config = load_config();
    let client_id = client_id(&config)?;

    // Make sure there's somewhere to put the token before we send the user off
    // to authorize — otherwise they'd do the dance and we'd silently drop it.
    // When none is configured, set up the best one installed.
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
        verified: true,
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

/// Run `grit auth <url>` for a `grit serve` host: sign in through the
/// server's web page, wait for the user to approve, then store the new token
/// with the credential helper under the server's scheme and host, with the
/// user's email address as the user name.
///
/// # Errors
///
/// Fails when no credential helper can be set up, the server cannot be
/// reached or is not a grit server, the user declines, or the code expires.
pub fn run_server_login(server: &ServerUrl) -> Result<AuthOutcome> {
    let http =
        UreqHttpClient::from_config(&load_config()).context("could not set up HTTP client")?;
    let flow = LoginFlow::new(&http, server);
    let login = flow.start().map_err(|e| server_error(server, e))?;
    let config = credential_config(server)?;
    let url = server.authorize_url(&login.user_code);

    // Like the GitHub flow, the instructions are interactive UX: stderr only.
    eprintln!("To sign in, open this page in your browser:\n");
    eprintln!("    {url}\n");
    eprintln!("and check that it shows the code:\n");
    eprintln!("    {}\n", login.user_code);
    if std::io::stderr().is_terminal() && open_browser(&url) {
        eprintln!("(Opened it for you.)");
    }
    eprintln!("Waiting for you to approve… (press Ctrl-C to cancel)");

    let grant = flow
        .wait(&login, &mut SystemPollClock)
        .map_err(|e| server_error(server, e))?;
    HelperCredentialProvider::new(config)
        .approve(&server.credential(Some(&grant.email), Some(&grant.token)))
        .context("storing the token with your credential helper")?;

    Ok(AuthOutcome {
        authenticated: true,
        host: server.host().to_owned(),
        user: Some(grant.email),
        verified: true,
    })
}

/// Run `grit auth <url> --token <token>` for a `grit serve` host: check
/// `token` against the server, then store it like [`run_server_login`].
///
/// # Errors
///
/// Fails when no credential helper can be set up, the server rejects the
/// token or cannot be reached, or the helper cannot store it.
pub fn run_server_token(server: &ServerUrl, token: &str) -> Result<AuthOutcome> {
    let http = UreqHttpClient::from_config(&load_config())
        .context("could not set up HTTP client")?
        .with_credential_provider(Box::new(TokenCredentialProvider::new(token)));
    let identity = fetch_identity(&http, server).map_err(|e| server_error(server, e))?;

    HelperCredentialProvider::new(credential_config(server)?)
        .approve(&server.credential(Some(&identity.email), Some(token)))
        .context("storing the token with your credential helper")?;

    Ok(AuthOutcome {
        authenticated: true,
        host: server.host().to_owned(),
        user: Some(identity.email),
        verified: true,
    })
}

/// Run `grit auth <url> --username <user> --token <token>`: store `token` as
/// `username`'s password for `url` with the credential helper, keyed the way a
/// fetch or push to `url` looks it up (see [`credential_for_url`]), setting up
/// a helper first when there is none. Works for any HTTP(S) host; nothing is
/// sent to it.
///
/// # Errors
///
/// Fails when `url` is not an `http`/`https` URL with a host, or no credential
/// helper can be set up or store the token.
pub fn store_token(url: &str, username: &str, token: &str) -> Result<AuthOutcome> {
    let to_store = |config: &ConfigSet| {
        credential_for_url(url, config, username, token).map_err(|e| anyhow!("{e}"))
    };
    let mut config = load_config();
    let mut credential = to_store(&config)?;
    if !has_token_helper(&config, &credential) {
        config = ensure_credential_helper()?;
        credential = to_store(&config)?;
    }
    HelperCredentialProvider::new(config)
        .approve(&credential)
        .context("storing the token with your credential helper")?;
    Ok(AuthOutcome {
        authenticated: true,
        host: credential.host.unwrap_or_default(),
        user: Some(username.to_owned()),
        verified: false,
    })
}

/// Config with a credential helper that will receive `server`'s token,
/// setting one up first when there is none.
fn credential_config(server: &ServerUrl) -> Result<ConfigSet> {
    let config = load_config();
    if has_token_helper(&config, &server.credential(None, None)) {
        return Ok(config);
    }
    ensure_credential_helper()
}

/// Describe a failed `grit serve` sign-in.
fn server_error(server: &ServerUrl, err: ServerAuthError) -> anyhow::Error {
    let root = server.root_url();
    match err {
        ServerAuthError::TokenRejected => anyhow!(
            "{root} rejected the token. Sign in with `grit auth {root}` instead, \
             or ask the server's admin for a new one (`grit serve token`)."
        ),
        ServerAuthError::LoginDenied => anyhow!("the sign-in was declined in the browser."),
        ServerAuthError::LoginExpired => {
            anyhow!("the code expired before you approved it. Run `grit auth {root}` to try again.")
        }
        ServerAuthError::NotGritServer(_) => anyhow!(
            "{root} is not a `grit serve` host; `grit auth <url>` signs in only to GitHub and grit serve.\n\
             Store a user name and token (or password) for it with \
             `grit auth {root} --username <user> --token <token>`, \
             or use a credential helper or GIT_ASKPASS."
        ),
        ServerAuthError::Http(source) => {
            anyhow::Error::new(source).context(format!("signing in to {root}"))
        }
        other => anyhow::Error::new(other),
    }
}

/// Open `url` in the default browser. Returns whether a browser was started.
fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
/// set the best installed helper ([`HelperCredentialProvider::suggested_helper`])
/// and say so; only when none is installed do we bail, since storing the token
/// would otherwise be silently lost. We use `grit manager` rather than Git Credential Manager because the
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
    let Some(helper) = HelperCredentialProvider::new(load_config()).suggested_helper() else {
        bail!(
            "no credential helper is configured, and none is installed to store the token in.\n\
             Install Git's credential helpers, or set one with \
             `grit config --global credential.helper <helper>`."
        )
    };
    crate::commands::config::set_global("credential.helper", helper)
        .context("setting up a credential helper")?;
    match helper {
        "store" => eprintln!(
            "Set credential.helper to `store`. It keeps tokens in plain text in ~/.git-credentials; \
             set a keychain helper instead if you have one."
        ),
        "osxkeychain" => eprintln!("Set credential.helper to `osxkeychain` (the macOS keychain)."),
        other => eprintln!("Set credential.helper to `{other}`."),
    }
    Ok(load_config())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls_in_every_form() {
        for url in [
            "https://github.com/o/r.git",
            "https://x-access-token@github.com/o/r",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "github.com:o/r",
        ] {
            assert!(is_github_url(url), "{url}");
        }
        for url in [
            "https://grithub.dev/o/r.git",
            "git@gitlab.com:o/r.git",
            "/srv/repos/r.git",
            "https://github.com.evil.example/o/r",
        ] {
            assert!(!is_github_url(url), "{url}");
        }
    }
}
