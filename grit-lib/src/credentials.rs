//! Credential layer — Git-compatible credential filling/approval/rejection for
//! library embedders.
//!
//! This is the reusable core lifted from the CLI's `grit credential` command
//! (`grit/src/commands/credential.rs`). It exposes:
//!
//! - [`Credential`] — a structured credential with the standard Git fields
//!   ([`protocol`](Credential::protocol), [`host`](Credential::host),
//!   [`path`](Credential::path), [`username`](Credential::username),
//!   [`password`](Credential::password), [`url`](Credential::url)) plus
//!   parsing/serialization in Git's `key=value\n…\n\n` credential wire format
//!   ([`Credential::parse`] / [`Credential::serialize`]).
//! - [`CredentialProvider`] — the pluggable seam an embedder implements (or
//!   wraps) to supply credentials.
//! - [`HelperCredentialProvider`] — the Git-compatible default that runs the
//!   configured `credential.helper` / `credential.<url>.helper` programs
//!   (shell `!cmd`, the built-in `store`/`cache` helpers, and external
//!   `git-credential-*` binaries), then falls back to an askpass program.
//!
//! ## Askpass, never a terminal
//!
//! When the configured helpers cannot supply a usable username/password,
//! [`HelperCredentialProvider`] asks the askpass program set with
//! [`HelperCredentialProvider::with_askpass`] for the missing fields, as Git
//! does. [`askpass_program`] resolves that program from `GIT_ASKPASS`,
//! `core.askPass` and `SSH_ASKPASS`; this is how CI systems inject tokens
//! non-interactively. `credential.interactive=false` (or `never`) disables
//! the fallback.
//!
//! The provider **never** prompts on a TTY. Without an askpass program, or
//! when it is disabled, [`fill`](CredentialProvider::fill) returns a typed
//! [`Error::Message`] (see [`NON_INTERACTIVE_MESSAGE`]) rather than blocking on
//! `/dev/tty`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::command_runner::{
    system_command_runner, CommandEnvironment, CommandRunner, CommandSpec, CommandStdin,
    CommandStdio, ShellInvocation,
};
use crate::config::{parse_bool, parse_path, ConfigSet};
use crate::environment::Environment;
use crate::error::{Error, Result};

/// Message returned (as [`Error::Message`]) when credentials are required but
/// no configured helper could supply a complete username/password and no
/// askpass program is available to ask for them.
pub const NON_INTERACTIVE_MESSAGE: &str = "credentials required but unavailable (non-interactive)";

/// The askpass program Git would run to ask for a missing username or
/// password.
///
/// The first of these that is set wins: the `GIT_ASKPASS` environment
/// variable, the `core.askPass` config value (with `~/` expanded), then the
/// `SSH_ASKPASS` environment variable. As in Git, a source that is set to an
/// empty value stops the search and disables askpass.
///
/// # Parameters
///
/// * `env` — the environment to read `GIT_ASKPASS` / `SSH_ASKPASS` (and `HOME`
///   for `~/`) from; pass [`Environment::empty`] to consult config only.
/// * `config` — merged config holding `core.askPass`.
///
/// # Returns
///
/// The program to pass to [`HelperCredentialProvider::with_askpass`], or
/// `None` when askpass is not configured.
#[must_use]
pub fn askpass_program(env: &Environment, config: &ConfigSet) -> Option<PathBuf> {
    let program = env
        .var_os("GIT_ASKPASS")
        .or_else(|| {
            config
                .get("core.askPass")
                .map(|value| parse_path(env, &value).into())
        })
        .or_else(|| env.var_os("SSH_ASKPASS"))?;
    (!program.is_empty()).then(|| PathBuf::from(program))
}

/// A structured Git credential.
///
/// Mirrors the fields Git's credential protocol exchanges. Round-trips through
/// the `key=value\n…\n\n` wire format via [`Credential::parse`] and
/// [`Credential::serialize`]. Any keys outside the named fields below
/// (`capability[]`, `authtype`, `password_expiry_utc`, …) are preserved in
/// [`extra`](Credential::extra) so they survive a parse/serialize round-trip
/// and are forwarded to helpers unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    /// `protocol` field (e.g. `https`, `http`, `ssh`).
    pub protocol: Option<String>,
    /// `host` field, optionally including a `:port` suffix.
    pub host: Option<String>,
    /// `path` field (repository path on the host).
    pub path: Option<String>,
    /// `username` field.
    pub username: Option<String>,
    /// `password` field (the secret).
    pub password: Option<String>,
    /// `url` field — a full URL Git can decompose into the fields above.
    ///
    /// [`HelperCredentialProvider`] uses it to match `credential.<url>.*`
    /// config (see [`target_url`](Credential::target_url)) and to fill a
    /// missing protocol/host, but never writes it to a helper: helpers such as
    /// `git-credential-store` would otherwise key the lookup on its path.
    pub url: Option<String>,
    /// Any additional `key=value` pairs, in wire order. Multi-valued keys such
    /// as `capability[]` may appear more than once.
    pub extra: Vec<(String, String)>,
}

impl Credential {
    /// Parse a credential from Git's `key=value\n…` wire format.
    ///
    /// Parsing stops at the first blank line (Git's record terminator) or at
    /// EOF. Trailing `\r` is stripped from each line so the parser accepts both
    /// LF and CRLF input. Lines without an `=` are ignored.
    pub fn parse(input: &str) -> Self {
        let mut cred = Credential::default();
        for line in input.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            cred.set(key, value);
        }
        cred
    }

    /// The credential Git requests for the repository at `url`.
    ///
    /// `protocol` and `host` (with any `:port`) come from `url`, and
    /// [`url`](Credential::url) keeps `url` itself so `credential.<url>.*`
    /// config still matches on the full repository path. For `http` and
    /// `https` the `path` is only included when `credential.useHttpPath`
    /// applies to `url`, so one host-level entry (as written by
    /// `git-credential-store` or a keychain helper) serves every repository on
    /// the host. Other protocols always carry the path. The path is
    /// percent-decoded and has surrounding slashes removed, as in Git.
    ///
    /// A `user[:password]@` in the URL becomes the percent-decoded
    /// [`username`](Credential::username) and
    /// [`password`](Credential::password), so a URL with both is already
    /// complete and a username-only URL is only asked for its password. The
    /// stored [`url`](Credential::url) has the userinfo removed, so the secret
    /// never reaches config matching or messages built from it.
    ///
    /// Pass the repository URL (`https://host/owner/repo.git`), not a
    /// smart-HTTP request URL with `/info/refs` and a query appended.
    ///
    /// # Returns
    ///
    /// `None` when `url` does not parse or has no host.
    #[must_use]
    pub fn from_url(url: &str, config: &ConfigSet) -> Option<Self> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str().filter(|host| !host.is_empty())?;
        let host = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let protocol = parsed.scheme();
        let url = url_without_userinfo(url);
        let is_http = matches!(protocol, "http" | "https");
        let path = (!is_http || use_http_path(config, Some(&url)))
            .then(|| percent_decode_lossy(parsed.path().trim_matches('/')))
            .filter(|path| !path.is_empty());
        let username = Some(parsed.username())
            .filter(|user| !user.is_empty())
            .map(percent_decode_lossy);
        Some(Credential {
            protocol: Some(protocol.to_owned()),
            host: Some(host),
            path,
            password: parsed.password().map(percent_decode_lossy),
            username,
            url: Some(url),
            ..Default::default()
        })
    }

    /// Parse from raw bytes (lossy UTF-8); convenience for helper stdout.
    pub fn parse_bytes(bytes: &[u8]) -> Self {
        Self::parse(&String::from_utf8_lossy(bytes))
    }

    /// Serialize to Git's `key=value\n` wire format (no trailing blank line).
    ///
    /// Fields are emitted in Git's canonical order
    /// (protocol, host, path, username, password, url) followed by any
    /// [`extra`](Credential::extra) entries in their stored order.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for (key, value) in self.iter_pairs() {
            out.push_str(&key);
            out.push('=');
            out.push_str(&value);
            out.push('\n');
        }
        out
    }

    /// Set a field by its Git key name. Unknown keys land in
    /// [`extra`](Credential::extra). Multi-valued keys (those ending in `[]`)
    /// always append; named keys overwrite.
    fn set(&mut self, key: &str, value: &str) {
        match key {
            "protocol" => self.protocol = Some(value.to_string()),
            "host" => self.host = Some(value.to_string()),
            "path" => self.path = Some(value.to_string()),
            "username" => self.username = Some(value.to_string()),
            "password" => self.password = Some(value.to_string()),
            "url" => self.url = Some(value.to_string()),
            _ => {
                if key.ends_with("[]") {
                    self.extra.push((key.to_string(), value.to_string()));
                } else if let Some(slot) = self
                    .extra
                    .iter_mut()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v)
                {
                    *slot = value.to_string();
                } else {
                    self.extra.push((key.to_string(), value.to_string()));
                }
            }
        }
    }

    /// Look up an `extra` key (first match).
    fn extra_get(&self, key: &str) -> Option<&str> {
        self.extra
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Iterate the credential's key/value pairs in Git's canonical wire order.
    fn iter_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        if let Some(v) = &self.protocol {
            pairs.push(("protocol".to_string(), v.clone()));
        }
        if let Some(v) = &self.host {
            pairs.push(("host".to_string(), v.clone()));
        }
        if let Some(v) = &self.path {
            pairs.push(("path".to_string(), v.clone()));
        }
        if let Some(v) = &self.username {
            pairs.push(("username".to_string(), v.clone()));
        }
        if let Some(v) = &self.password {
            pairs.push(("password".to_string(), v.clone()));
        }
        if let Some(v) = &self.url {
            pairs.push(("url".to_string(), v.clone()));
        }
        for (k, v) in &self.extra {
            pairs.push((k.clone(), v.clone()));
        }
        pairs
    }

    /// True when the credential carries a usable username **and** password.
    pub fn is_complete(&self) -> bool {
        self.username.as_deref().is_some_and(|s| !s.is_empty())
            && self.password.as_deref().is_some_and(|s| !s.is_empty())
    }

    /// The URL this credential authenticates against, for matching
    /// `credential.<url>.helper` config entries. Prefers an explicit
    /// [`url`](Credential::url); otherwise reconstructs it from
    /// protocol/host/path (matching Git's `credential_apply_config`).
    pub fn target_url(&self) -> Option<String> {
        if let Some(u) = self.url.as_deref().filter(|u| !u.trim().is_empty()) {
            return Some(u.to_string());
        }
        let protocol = self.protocol.as_deref()?;
        let host = self.host.as_deref()?;
        let mut url = format!("{protocol}://");
        if let Some(username) = self.username.as_deref().filter(|u| !u.is_empty()) {
            url.push_str(username);
            url.push('@');
        }
        url.push_str(host);
        if let Some(path) = self.path.as_deref().filter(|p| !p.is_empty()) {
            if !path.starts_with('/') {
                url.push('/');
            }
            url.push_str(path);
        }
        Some(url)
    }

    /// The askpass prompt for `field` (`Username` or `Password`), in the
    /// `<field> for '<protocol>://[<user>@]<host>[/<path>]': ` form askpass
    /// scripts match on. With `sanitize` (Git's `credential.sanitizePrompt`,
    /// which may be URL-scoped and is on by default) control and URL-unsafe characters in the user, host and
    /// path are percent-encoded.
    fn prompt(&self, field: &str, sanitize: bool) -> String {
        let Some(protocol) = self.protocol.as_deref() else {
            return format!("{field}: ");
        };
        let part = |value: &str, escape: fn(u8) -> bool| {
            if sanitize {
                percent_encode_where(value, escape)
            } else {
                value.to_owned()
            }
        };
        let mut target = format!("{protocol}://");
        if let Some(user) = self.username.as_deref().filter(|u| !u.is_empty()) {
            target.push_str(&part(user, |b| {
                b == b'/' || URL_UNSAFE_PROMPT_BYTES.contains(&b)
            }));
            target.push('@');
        }
        if let Some(host) = self.host.as_deref() {
            target.push_str(&part(host, |b| {
                !b.is_ascii_alphanumeric() && !b"-.:[]".contains(&b)
            }));
        }
        if let Some(path) = self.path.as_deref() {
            target.push('/');
            target.push_str(&part(path, |b| URL_UNSAFE_PROMPT_BYTES.contains(&b)));
        }
        format!("{field} for '{target}': ")
    }

    /// Merge a helper's `get` response into this credential: fill any missing
    /// username/password (and other recognized fields) without clobbering
    /// values we already hold. `quit` and `capability[]` are tracked in
    /// `extra` for the caller to inspect.
    fn merge_response(&mut self, response: &Credential) {
        if self.username.is_none() {
            self.username = response.username.clone();
        }
        if self.password.is_none() {
            self.password = response.password.clone();
        }
        if self.protocol.is_none() {
            self.protocol = response.protocol.clone();
        }
        if self.host.is_none() {
            self.host = response.host.clone();
        }
        if self.path.is_none() {
            self.path = response.path.clone();
        }
        for (k, v) in &response.extra {
            // Preserve quit signalling for callers; other extras are advisory.
            if k == "quit" {
                self.set(k, v);
            }
        }
    }

    /// Did a helper signal `quit=1`/`quit=true` (stop querying further helpers)?
    fn wants_quit(&self) -> bool {
        matches!(self.extra_get("quit"), Some("1") | Some("true"))
    }
}

/// The pluggable credential seam an embedder implements (or wraps).
///
/// All three methods take a (partial) [`Credential`] describing the target and
/// return a result; transports call [`fill`](CredentialProvider::fill) before a
/// request and [`approve`](CredentialProvider::approve) /
/// [`reject`](CredentialProvider::reject) after, mirroring Git's
/// `credential_fill` / `credential_approve` / `credential_reject`.
pub trait CredentialProvider {
    /// Fill in missing fields (typically username/password) for `input`,
    /// returning a more-complete [`Credential`]. Implementations that cannot
    /// supply a usable credential should return a typed [`Error`] rather than
    /// block on interactive input.
    fn fill(&self, input: &Credential) -> Result<Credential>;

    /// Mark `cred` as known-good (helpers `store` it).
    fn approve(&self, cred: &Credential) -> Result<()>;

    /// Mark `cred` as known-bad (helpers `erase` it).
    fn reject(&self, cred: &Credential) -> Result<()>;
}

/// Git-compatible [`CredentialProvider`] that runs the configured
/// `credential.helper` programs.
///
/// Built from a [`ConfigSet`]; it resolves the helper list per the target URL
/// (so `credential.<url>.helper` entries are honored) and invokes each helper
/// with `get` (for [`fill`](CredentialProvider::fill)), `store` (for
/// [`approve`](CredentialProvider::approve)), or `erase` (for
/// [`reject`](CredentialProvider::reject)) exactly as Git does.
///
/// If no helper yields a complete credential, [`fill`](CredentialProvider::fill)
/// asks the [askpass program](HelperCredentialProvider::with_askpass) for the
/// missing username and password. It never prompts on a terminal: without an
/// askpass program it returns [`Error::Message`] with
/// [`NON_INTERACTIVE_MESSAGE`].
pub struct HelperCredentialProvider {
    config: ConfigSet,
    command_runner: Arc<dyn CommandRunner>,
    probed_git_exec_path: OnceLock<Vec<PathBuf>>,
    askpass: Option<PathBuf>,
}

impl HelperCredentialProvider {
    /// Build a provider from a loaded [`ConfigSet`], with no askpass program.
    pub fn new(config: ConfigSet) -> Self {
        Self {
            config,
            command_runner: system_command_runner(),
            probed_git_exec_path: OnceLock::new(),
            askpass: None,
        }
    }

    /// Override the subprocess runner (tests and embedders).
    #[must_use]
    pub fn with_command_runner(mut self, runner: Arc<dyn CommandRunner>) -> Self {
        self.command_runner = runner;
        self
    }

    /// Ask `program` for a username and password the helpers could not
    /// supply, usually the result of [`askpass_program`].
    ///
    /// `program` runs once per missing field with the prompt (for example
    /// `Username for 'https://example.com': `) as its only argument, and its
    /// stdout up to the first line break is the answer. Credentials obtained
    /// this way are stored with the helpers when the transport
    /// [approves](CredentialProvider::approve) them. `None` disables the
    /// fallback. `credential.interactive=false` or `never` also disables it.
    #[must_use]
    pub fn with_askpass(mut self, program: Option<PathBuf>) -> Self {
        self.askpass = program;
        self
    }

    /// The `credential.helper` programs this provider runs for `cred`, in the
    /// order [`fill`](CredentialProvider::fill),
    /// [`approve`](CredentialProvider::approve) and
    /// [`reject`](CredentialProvider::reject) invoke them.
    ///
    /// Includes `credential.<url>.helper` entries whose URL pattern matches
    /// [`Credential::target_url`]; an empty value resets the list, as in Git.
    /// An empty result means storing `cred` would be a no-op.
    #[must_use]
    pub fn helpers_for(&self, cred: &Credential) -> Vec<String> {
        self.helpers(cred.target_url().as_deref())
    }

    /// The ordered helper list applicable to `target_url`.
    fn helpers(&self, target_url: Option<&str>) -> Vec<String> {
        credential_helpers(&self.config, target_url)
    }

    /// The fields a helper receives for `cred`: an explicit
    /// [`url`](Credential::url) fills a missing protocol/host/path the way
    /// [`Credential::from_url`] does and is then dropped, because Git never
    /// writes `url` to helpers.
    fn helper_request(&self, cred: &Credential) -> Credential {
        let mut request = cred.clone();
        let Some(url) = request.url.take() else {
            return request;
        };
        if request.protocol.is_none() && request.host.is_none() {
            if let Some(from_url) = Credential::from_url(&url, &self.config) {
                request.protocol = from_url.protocol;
                request.host = from_url.host;
                request.path = request.path.or(from_url.path);
            }
        }
        request
    }

    /// False when `credential.interactive` is `false` or `never`.
    fn prompts_allowed(&self) -> bool {
        self.config
            .get("credential.interactive")
            .is_none_or(|value| value != "never" && parse_bool(&value).unwrap_or(true))
    }

    fn exec_path_candidates(&self) -> Vec<PathBuf> {
        credential_helper_exec_path_candidates(
            self.command_runner.as_ref(),
            &self.probed_git_exec_path,
        )
    }

    /// Whether the helper program for the bare helper name `name` (so
    /// `git-credential-<name>`) is installed in Git's exec-path or on `PATH`.
    #[must_use]
    pub fn helper_installed(&self, name: &str) -> bool {
        let program = format!("git-credential-{name}");
        let on_path = crate::environment::Environment::empty()
            .var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        self.exec_path_candidates()
            .iter()
            .chain(on_path.iter())
            .any(|dir| credential_helper_executable_at(&dir.join(&program)).is_some())
    }

    /// The helper to configure when `credential.helper` is unset: the first of
    /// [`SUGGESTED_HELPERS`] that is installed.
    ///
    /// That is the platform's secure store when it is available (the macOS
    /// keychain, libsecret on Linux), otherwise `store`, which keeps
    /// credentials in a plaintext file. `None` when none is installed.
    #[must_use]
    pub fn suggested_helper(&self) -> Option<&'static str> {
        SUGGESTED_HELPERS
            .iter()
            .copied()
            .find(|name| self.helper_installed(name))
    }
}

/// Helper names [`HelperCredentialProvider::suggested_helper`] tries, best
/// first: the platform's secure store, then the plaintext `store`.
#[cfg(target_os = "macos")]
pub const SUGGESTED_HELPERS: &[&str] = &["osxkeychain", "store"];
/// Helper names [`HelperCredentialProvider::suggested_helper`] tries, best
/// first: the platform's secure store, then the plaintext `store`.
#[cfg(all(unix, not(target_os = "macos")))]
pub const SUGGESTED_HELPERS: &[&str] = &["libsecret", "store"];
/// Helper names [`HelperCredentialProvider::suggested_helper`] tries, best
/// first: the platform's secure store, then the plaintext `store`.
#[cfg(not(unix))]
pub const SUGGESTED_HELPERS: &[&str] = &["manager", "store"];

impl CredentialProvider for HelperCredentialProvider {
    fn fill(&self, input: &Credential) -> Result<Credential> {
        let mut filled = input.clone();
        if filled.is_complete() {
            return Ok(filled);
        }
        let target_url = filled.target_url();
        let helper_list = self.helpers(target_url.as_deref());
        for helper in helper_list {
            let response = invoke_helper(
                self.command_runner.as_ref(),
                &helper,
                "get",
                &self.helper_request(&filled),
                &self.exec_path_candidates(),
            )?;
            if response.wants_quit() {
                return Err(Error::Message(format!(
                    "credential helper '{helper}' told us to quit"
                )));
            }
            filled.merge_response(&response);
            if filled.is_complete() {
                return Ok(filled);
            }
        }
        let Some(askpass) = self.askpass.as_deref().filter(|_| self.prompts_allowed()) else {
            return Err(Error::Message(NON_INTERACTIVE_MESSAGE.to_string()));
        };
        let sanitize =
            credential_config_value(&self.config, target_url.as_deref(), "sanitizePrompt")
                .is_none_or(|value| parse_bool(&value).unwrap_or(true));
        if filled.username.is_none() {
            let prompt = filled.prompt("Username", sanitize);
            filled.username = Some(run_askpass(self.command_runner.as_ref(), askpass, &prompt)?);
        }
        if filled.password.is_none() {
            let prompt = filled.prompt("Password", sanitize);
            filled.password = Some(run_askpass(self.command_runner.as_ref(), askpass, &prompt)?);
        }
        Ok(filled)
    }

    fn approve(&self, cred: &Credential) -> Result<()> {
        let target_url = cred.target_url();
        let request = self.helper_request(cred);
        for helper in self.helpers(target_url.as_deref()) {
            invoke_helper(
                self.command_runner.as_ref(),
                &helper,
                "store",
                &request,
                &self.exec_path_candidates(),
            )?;
        }
        Ok(())
    }

    fn reject(&self, cred: &Credential) -> Result<()> {
        let target_url = cred.target_url();
        let request = self.helper_request(cred);
        for helper in self.helpers(target_url.as_deref()) {
            invoke_helper(
                self.command_runner.as_ref(),
                &helper,
                "erase",
                &request,
                &self.exec_path_candidates(),
            )?;
        }
        Ok(())
    }
}

/// Build the effective `credential.helper` list in Git order.
///
/// Git walks every `credential.helper` and `credential.<URL>.helper` config
/// entry in load order. URL-scoped entries only apply when the subsection
/// pattern matches `target_url` (per Git's URL-match rules). For every
/// applicable entry, a non-empty value is appended to the helper list and an
/// empty value resets it (Git's `string_list_clear` semantics in
/// `credential_apply_config_cb`).
///
/// `target_url` is the URL we're authenticating against (e.g.
/// `https://github.com/owner/repo.git`). When `None`, only unscoped
/// `credential.helper` entries contribute.
fn credential_helpers(config: &ConfigSet, target_url: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for entry in config.entries() {
        let key = &entry.key;
        if key.contains('\n') || key.to_ascii_lowercase().contains("%0a") {
            continue;
        }
        let Some(first_dot) = key.find('.') else {
            continue;
        };
        let Some(last_dot) = key.rfind('.') else {
            continue;
        };
        let section = &key[..first_dot];
        let variable = &key[last_dot + 1..];
        if !section.eq_ignore_ascii_case("credential") || !variable.eq_ignore_ascii_case("helper") {
            continue;
        }
        if first_dot != last_dot {
            let subsection = &key[first_dot + 1..last_dot];
            if percent_decode_lossy(subsection).contains('\n') {
                continue;
            }
            let Some(target) = target_url else {
                continue;
            };
            if !credential_url_matches(subsection, target) {
                continue;
            }
        }
        let value = entry.value.as_deref().unwrap_or("");
        if value.trim().is_empty() {
            out.clear();
        } else {
            out.push(value.to_string());
        }
    }
    out
}

fn credential_url_matches(pattern: &str, target: &str) -> bool {
    let pattern = percent_decode_lossy(pattern);
    if pattern.contains('\n') {
        return false;
    }
    let pattern = pattern.trim_end_matches('/');
    let pattern_no_user = url_without_userinfo(pattern);
    let pattern_after_user = pattern.rsplit_once('@').map(|(_, host)| host);
    let target = target.trim_end_matches('/');
    let target_no_user = url_without_userinfo(target);
    let target_no_scheme = strip_url_scheme(target);
    let target_no_scheme_no_user = strip_url_scheme(&target_no_user);
    let target_path = target_path_component(target);

    let matches = |pattern: &str| {
        if pattern.starts_with('/') {
            return credential_prefix_matches(pattern, target_path);
        }
        if pattern.ends_with("://") {
            return target.starts_with(pattern) || target_no_user.starts_with(pattern);
        }
        if pattern.contains('*') {
            return credential_wildcard_matches(pattern, target)
                || credential_wildcard_matches(pattern, &target_no_user)
                || credential_wildcard_matches(pattern, target_no_scheme)
                || credential_wildcard_matches(pattern, target_no_scheme_no_user);
        }
        credential_prefix_matches(pattern, target)
            || credential_prefix_matches(pattern, &target_no_user)
            || credential_prefix_matches(pattern, target_no_scheme)
            || credential_prefix_matches(pattern, target_no_scheme_no_user)
    };
    matches(pattern)
        || (pattern_no_user != pattern && matches(&pattern_no_user))
        || pattern_after_user.is_some_and(matches)
}

fn credential_prefix_matches(pattern: &str, candidate: &str) -> bool {
    candidate
        .strip_prefix(pattern)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || pattern.ends_with("://"))
}

fn credential_wildcard_matches(pattern: &str, candidate: &str) -> bool {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return false;
    };
    let Some(rest) = candidate.strip_prefix(prefix) else {
        return false;
    };
    rest.find(suffix).is_some_and(|idx| {
        let after = &rest[idx + suffix.len()..];
        after.is_empty() || after.starts_with('/')
    })
}

fn strip_url_scheme(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// Return `url` with any `user:pass@` userinfo removed.
///
/// Handles `scheme://user:pass@host/...` and scp-style `user:pass@host:path`
/// (no scheme). Userinfo is stripped only from the URL authority (the part
/// before the first `/`, `?`, or `#`); `@` in paths, queries, and fragments is
/// preserved. The result is safe to show in errors and logs.
pub fn url_without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        let (authority, suffix) = split_authority_suffix(url);
        let authority = strip_userinfo_from_authority(authority);
        return format!("{authority}{suffix}");
    };
    let (authority, suffix) = split_authority_suffix(rest);
    let authority = strip_userinfo_from_authority(authority);
    format!("{scheme}://{authority}{suffix}")
}

fn split_authority_suffix(url: &str) -> (&str, &str) {
    let authority_end = url
        .char_indices()
        .find(|(_, ch)| matches!(ch, '/' | '?' | '#'))
        .map(|(idx, _)| idx)
        .unwrap_or(url.len());
    url.split_at(authority_end)
}

fn strip_userinfo_from_authority(authority: &str) -> &str {
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host_port)| host_port)
}

/// Redact credentials from URL-like `http://` / `https://` substrings in `message`.
pub fn redact_embedded_url_credentials(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(scheme_sep) = rest.find("://") {
        let before = &rest[..scheme_sep];
        let scheme_start = before
            .char_indices()
            .rfind(|(_, c)| !c.is_ascii_alphanumeric() && *c != '-' && *c != '+' && *c != '.')
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        out.push_str(&rest[..scheme_start]);
        let url_part = &rest[scheme_start..];
        let url_end = url_part
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | ','))
            .unwrap_or(url_part.len());
        let url = &url_part[..url_end];
        out.push_str(&url_without_userinfo(url));
        rest = &url_part[url_end..];
    }
    out.push_str(rest);
    out
}

fn target_path_component(url: &str) -> &str {
    let rest = strip_url_scheme(url);
    let idx = rest
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '/' | '?' | '#').then_some(idx))
        .unwrap_or(rest.len());
    &rest[idx..]
}

fn percent_decode_lossy(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2])) {
                out.push((hi << 4) | lo);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bytes a sanitized askpass prompt percent-encodes in the user and path.
const URL_UNSAFE_PROMPT_BYTES: &[u8] = b" <>\"%{}|\\^`:?#[]@!$&'()*+,;=";

/// Percent-encode control and non-ASCII bytes of `value`, plus every byte for
/// which `escape` returns true.
fn percent_encode_where(value: &str, escape: fn(u8) -> bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte <= 0x1f || byte >= 0x7f || escape(byte) {
            let _ = write!(out, "%{byte:02X}");
        } else {
            out.push(char::from(byte));
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Extra `bin` directories to search for credential helpers given Git's `--exec-path`.
///
/// Git for Windows uses `{root}/mingw64/libexec/git-core` for exec-path while Git
/// Credential Manager lives in `{root}/mingw64/bin`, not `{root}/mingw64/libexec/bin`.
fn credential_helper_bin_dirs_from_exec_path(exec_path: &Path) -> Vec<PathBuf> {
    let mut bins = Vec::new();
    let Some(parent) = exec_path.parent() else {
        return bins;
    };
    bins.push(parent.join("bin"));
    if exec_path.file_name().is_some_and(|name| name == "git-core")
        && parent.file_name().is_some_and(|name| name == "libexec")
    {
        if let Some(toolchain_root) = parent.parent() {
            bins.push(toolchain_root.join("bin"));
        }
    }
    bins
}

fn push_exec_path_candidate(out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, dir: PathBuf) {
    if !dir.is_dir() || !seen.insert(dir.clone()) {
        return;
    }
    out.push(dir.clone());
    for bin in credential_helper_bin_dirs_from_exec_path(&dir) {
        if bin.is_dir() && seen.insert(bin.clone()) {
            out.push(bin);
        }
    }
}

/// Directories to search for `git-credential-*` the way Git does
/// (exec-path before `PATH`). Git installs helpers under e.g.
/// `/usr/libexec/git-core`, which is not on `PATH`.
fn credential_helper_exec_path_candidates(
    runner: &dyn CommandRunner,
    probed_git_exec_path: &OnceLock<Vec<PathBuf>>,
) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let mut seen = HashSet::new();
    if let Some(ep) = crate::environment::Environment::empty().var("GIT_EXEC_PATH") {
        push_exec_path_candidate(&mut v, &mut seen, PathBuf::from(ep.trim()));
    }
    for dir in probed_git_exec_path_dirs(runner, probed_git_exec_path) {
        push_exec_path_candidate(&mut v, &mut seen, dir);
    }
    for candidate in [
        "/usr/libexec/git-core",
        "/Library/Developer/CommandLineTools/usr/libexec/git-core",
        "/opt/homebrew/opt/git/libexec/git-core",
        "/opt/homebrew/libexec/git-core",
        "/usr/lib/git-core",
        "/usr/local/libexec/git-core",
    ] {
        push_exec_path_candidate(&mut v, &mut seen, PathBuf::from(candidate));
    }
    v
}

fn probed_git_exec_path_dirs(
    runner: &dyn CommandRunner,
    probed_git_exec_path: &OnceLock<Vec<PathBuf>>,
) -> Vec<PathBuf> {
    probed_git_exec_path
        .get_or_init(|| probe_git_exec_path(runner))
        .clone()
}

fn probe_git_exec_path(runner: &dyn CommandRunner) -> Vec<PathBuf> {
    let spec = CommandSpec {
        program: PathBuf::from("git"),
        args: vec![OsString::from("--exec-path")],
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Null,
    };
    let Ok(child) = runner.spawn(&spec) else {
        return Vec::new();
    };
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    if !output.status.success {
        return Vec::new();
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if dir.is_dir() {
        vec![dir]
    } else {
        Vec::new()
    }
}

fn credential_helper_executable_at(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    #[cfg(windows)]
    {
        let with_exe = path.with_extension("exe");
        if with_exe.is_file() {
            return Some(with_exe);
        }
    }
    None
}

/// Resolve a helper program name to an executable path. A bare
/// `git-credential-<name>` is looked up across Git's exec-path candidates
/// before falling back to `PATH`.
fn resolve_credential_helper_executable(
    helper_program: &str,
    exec_path_candidates: &[PathBuf],
) -> PathBuf {
    if helper_program.contains('/') {
        return PathBuf::from(helper_program);
    }
    if let Some(suffix) = helper_program.strip_prefix("git-credential-") {
        let exe_name = format!("git-credential-{suffix}");
        for ep in exec_path_candidates {
            let candidate = ep.join(&exe_name);
            if let Some(resolved) = credential_helper_executable_at(&candidate) {
                return resolved;
            }
        }
    }
    PathBuf::from(helper_program)
}

/// Invoke an external credential helper program.
///
/// The helper may be:
/// - shell form: `!command ...` (executed by `sh -c`)
/// - absolute/relative path containing `/`
/// - bare helper name (expanded to `git-credential-<name>`)
/// - already-expanded binary (`git-credential-...`)
///
/// Bare helper names such as `store` and `cache` expand to `git-credential-*`
/// and are resolved via Git's exec-path (and `PATH`) like other helpers.
///
/// The helper is invoked with one action argument (`get`, `store`, `erase`)
/// after any arguments from the configured helper string. Credential fields are
/// written to stdin as `key=value` lines followed by a blank line; stdout is
/// parsed back into a [`Credential`].
/// The fields sent to a helper for `creds`.
///
/// Like Git, a credential that already has its protocol and host goes to
/// helpers without `url`: the URL is only used to pick helpers. Helpers parse a
/// `url` into protocol, host *and path*, so sending the request URL (say
/// `…/repo.git/info/refs?service=…`) would make every host-wide stored
/// credential miss.
fn helper_request(creds: &Credential) -> std::borrow::Cow<'_, Credential> {
    if creds.url.is_none() || creds.protocol.is_none() || creds.host.is_none() {
        return std::borrow::Cow::Borrowed(creds);
    }
    let mut request = creds.clone();
    request.url = None;
    std::borrow::Cow::Owned(request)
}

fn invoke_helper(
    runner: &dyn CommandRunner,
    helper: &str,
    action: &str,
    creds: &Credential,
    exec_path_candidates: &[PathBuf],
) -> Result<Credential> {
    let helper_words = shell_words::split(helper)
        .map_err(|e| Error::Message(format!("invalid credential.helper '{helper}': {e}")))?;
    let (first_word, extra_args) = match helper_words.split_first() {
        Some((first, rest)) => (first.as_str(), rest),
        None => ("", &[][..]),
    };

    let mut stdin_body = helper_request(creds).serialize();
    stdin_body.push('\n');
    let stdin_bytes = stdin_body.into_bytes();

    let spec = if let Some(shell_cmd) = helper.strip_prefix('!') {
        CommandSpec {
            program: "sh".into(),
            args: Vec::new(),
            shell: Some(ShellInvocation::DashC {
                script: format!("{shell_cmd} {action}"),
                argv0: None,
                args: Vec::new(),
            }),
            cwd: None,
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Pipe(stdin_bytes),
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Inherit,
        }
    } else {
        let helper_program =
            if first_word.contains('/') || first_word.starts_with("git-credential-") {
                first_word.to_string()
            } else {
                format!("git-credential-{first_word}")
            };
        let resolved = resolve_credential_helper_executable(&helper_program, exec_path_candidates);
        let mut args: Vec<OsString> = extra_args
            .iter()
            .map(|a| OsString::from(a.as_str()))
            .collect();
        args.push(OsString::from(action));
        CommandSpec {
            program: resolved,
            args,
            shell: None,
            cwd: None,
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Pipe(stdin_bytes),
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Inherit,
        }
    };

    let output = runner
        .spawn(&spec)
        .map_err(|e| Error::Message(format!("failed to run credential helper '{helper}': {e}")))?
        .wait_with_output()
        .map_err(|e| Error::Message(format!("credential helper '{helper}' failed: {e}")))?;
    if !output.status.success {
        return Err(Error::Message(format!(
            "credential helper '{helper}' exited with status {}",
            output.status.code.unwrap_or(-1)
        )));
    }

    Ok(Credential::parse_bytes(&output.stdout))
}

/// Run the askpass `program` with `prompt` as its argument and return its
/// answer: stdout up to the first `\r` or `\n`.
fn run_askpass(runner: &dyn CommandRunner, program: &Path, prompt: &str) -> Result<String> {
    let spec = CommandSpec {
        program: program.to_path_buf(),
        args: vec![OsString::from(prompt)],
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Inherit,
    };
    let output = runner
        .spawn(&spec)
        .and_then(|child| child.wait_with_output())
        .map_err(|e| {
            Error::Message(format!(
                "could not run askpass program '{}': {e}",
                program.display()
            ))
        })?;
    if !output.status.success {
        return Err(Error::Message(format!(
            "askpass program '{}' exited with status {}",
            program.display(),
            output.status.code.unwrap_or(-1)
        )));
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    Ok(answer
        .split(['\r', '\n'])
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// Whether `credential.useHttpPath` (optionally URL-scoped) is enabled.
///
/// Exposed so embedders can decide whether to include the `path` field when
/// constructing a [`Credential`] for an HTTP(S) target, matching Git.
pub fn use_http_path(config: &ConfigSet, target_url: Option<&str>) -> bool {
    credential_config_value(config, target_url, "useHttpPath")
        .as_deref()
        .map(|value| parse_bool(value).unwrap_or(false))
        .unwrap_or(false)
}

fn credential_config_value(
    config: &ConfigSet,
    target_url: Option<&str>,
    variable_name: &str,
) -> Option<String> {
    let mut out = None;
    for entry in config.entries() {
        let key = &entry.key;
        if key.contains('\n') || key.to_ascii_lowercase().contains("%0a") {
            continue;
        }
        let Some(first_dot) = key.find('.') else {
            continue;
        };
        let Some(last_dot) = key.rfind('.') else {
            continue;
        };
        let section = &key[..first_dot];
        let variable = &key[last_dot + 1..];
        if !section.eq_ignore_ascii_case("credential")
            || !variable.eq_ignore_ascii_case(variable_name)
        {
            continue;
        }
        if first_dot != last_dot {
            let subsection = &key[first_dot + 1..last_dot];
            if percent_decode_lossy(subsection).contains('\n') {
                continue;
            }
            let Some(target) = target_url else {
                continue;
            };
            if !credential_url_matches(subsection, target) {
                continue;
            }
        }
        out = entry.value.clone();
    }
    out
}

/// Windows Credential Manager backing store for the built-in `manager`
/// credential helper (`gs manager`).
///
/// Git for Windows bundles Git Credential Manager (the `manager` helper), but it
/// is only present when Git for Windows is installed. These functions let `gs`
/// store and retrieve secrets directly from the Windows Credential Manager
/// without that dependency. They are only compiled on Windows; the `gs manager`
/// command errors elsewhere.
///
/// Secrets are stored as `CRED_TYPE_GENERIC` entries keyed by a target name of
/// the form `git:<protocol>://<host>`, matching Git's own `wincred` helper so
/// the entries are recognizable in the Credential Manager UI.
#[cfg(windows)]
pub mod windows_store {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    use super::Credential;
    use crate::error::{Error, Result};

    /// The Credential Manager target name a credential is stored under.
    fn target_name(cred: &Credential) -> Result<String> {
        let protocol = cred
            .protocol
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::Message("credential is missing its protocol".to_owned()))?;
        let host = cred
            .host
            .as_deref()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::Message("credential is missing its host".to_owned()))?;
        Ok(format!("git:{protocol}://{host}"))
    }

    /// Encode a string as a NUL-terminated UTF-16 buffer for the wide Win32 API.
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Read a NUL-terminated wide string from a (possibly null) pointer.
    ///
    /// # Safety
    /// `ptr` must be null or point to a NUL-terminated UTF-16 string owned by a
    /// live `CREDENTIALW` returned by `CredReadW`.
    unsafe fn wstr_to_string(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }

    /// Look up the stored username/password for `cred`'s host, if any.
    pub fn get(cred: &Credential) -> Result<Option<Credential>> {
        let target = wide(&target_name(cred)?);
        let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `target` is a valid NUL-terminated wide string and `pcred` is a
        // valid out-pointer; on success Windows allocates the credential for us.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pcred) };
        if ok == 0 {
            // SAFETY: `GetLastError` is always safe to call after a failed Win32 call.
            let err = unsafe { GetLastError() };
            if err == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(Error::Message(format!(
                "reading from the Windows Credential Manager failed (error {err})"
            )));
        }

        // SAFETY: `CredReadW` succeeded, so `pcred` points to a live credential we own
        // until `CredFree`. We copy out the fields before freeing it.
        let (username, password) = unsafe {
            let c = &*pcred;
            let username = wstr_to_string(c.UserName);
            let password = if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
                String::new()
            } else {
                let bytes =
                    std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
                String::from_utf8_lossy(bytes).into_owned()
            };
            (username, password)
        };
        // SAFETY: `pcred` came from `CredReadW` and has not been freed yet.
        unsafe { CredFree(pcred as *const core::ffi::c_void) };

        let mut out = Credential::default();
        if !username.is_empty() {
            out.username = Some(username);
        }
        if !password.is_empty() {
            out.password = Some(password);
        }
        Ok(Some(out))
    }

    /// Store `cred`'s password (and username) for its host. A credential with no
    /// password is a no-op.
    pub fn store(cred: &Credential) -> Result<()> {
        let password = match cred.password.as_deref() {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(()),
        };
        let mut target = wide(&target_name(cred)?);
        let mut username = wide(cred.username.as_deref().unwrap_or(""));
        let blob = password.as_bytes();

        // SAFETY: zeroing is valid for this plain-old-data struct; we populate the
        // fields the API reads below.
        let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target.as_mut_ptr();
        credential.CredentialBlobSize = blob.len() as u32;
        credential.CredentialBlob = blob.as_ptr() as *mut u8;
        credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
        credential.UserName = username.as_mut_ptr();

        // SAFETY: every pointer field references a buffer that outlives the call.
        let ok = unsafe { CredWriteW(&credential as *const CREDENTIALW, 0) };
        if ok == 0 {
            // SAFETY: always safe after a failed Win32 call.
            let err = unsafe { GetLastError() };
            return Err(Error::Message(format!(
                "writing to the Windows Credential Manager failed (error {err})"
            )));
        }
        Ok(())
    }

    /// Delete the stored credential for `cred`'s host. Succeeds if none exists.
    pub fn erase(cred: &Credential) -> Result<()> {
        let target = wide(&target_name(cred)?);
        // SAFETY: `target` is a valid NUL-terminated wide string.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            // SAFETY: always safe after a failed Win32 call.
            let err = unsafe { GetLastError() };
            if err != ERROR_NOT_FOUND {
                return Err(Error::Message(format!(
                    "deleting from the Windows Credential Manager failed (error {err})"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_runner::{RecordedResponse, RecordingRunner};

    #[test]
    fn helpers_get_decomposed_fields_without_the_request_url() {
        let request = Credential {
            protocol: Some("http".into()),
            host: Some("127.0.0.1:8080".into()),
            url: Some("http://127.0.0.1:8080/app.git/info/refs?service=git-upload-pack".into()),
            ..Default::default()
        };
        assert_eq!(
            helper_request(&request).serialize(),
            "protocol=http\nhost=127.0.0.1:8080\n"
        );

        // A bare `url` is all a helper could go on, so it stays.
        let url_only = Credential {
            url: Some("https://example.com/".into()),
            ..Default::default()
        };
        assert_eq!(
            helper_request(&url_only).serialize(),
            "url=https://example.com/\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_that_exits_without_reading_stdin_still_answers() {
        // A request larger than any pipe buffer guarantees the write hits a closed pipe once
        // the helper has exited without reading it.
        let creds = Credential {
            protocol: Some("https".into()),
            host: Some("example.com".into()),
            password: Some("x".repeat(1 << 20)),
            ..Default::default()
        };
        let response = invoke_helper(
            system_command_runner().as_ref(),
            "!echo quit=1; exit 0; :",
            "get",
            &creds,
            &[],
        )
        .expect("helper");
        assert!(response.wants_quit());
    }

    #[test]
    fn url_without_userinfo_strips_https_credentials() {
        let raw = "https://x-access-token:SECRETTOKEN@github.com/octocat/repo.git/info/refs?service=git-upload-pack";
        let redacted = url_without_userinfo(raw);
        assert_eq!(
            redacted,
            "https://github.com/octocat/repo.git/info/refs?service=git-upload-pack"
        );
        assert!(!redacted.contains("SECRETTOKEN"));
        assert!(!redacted.contains("x-access-token"));
    }

    #[test]
    fn url_without_userinfo_preserves_at_in_path_query_and_fragment() {
        let path_at = "https://example.com/org/repo@v1.git/info/refs?service=git-upload-pack";
        assert_eq!(
            url_without_userinfo(path_at),
            "https://example.com/org/repo@v1.git/info/refs?service=git-upload-pack"
        );

        let query_at = "https://example.com/repo.git/info/refs?notify=user@example.net";
        assert_eq!(
            url_without_userinfo(query_at),
            "https://example.com/repo.git/info/refs?notify=user@example.net"
        );

        let fragment_at = "https://example.com/repo.git#section@anchor";
        assert_eq!(
            url_without_userinfo(fragment_at),
            "https://example.com/repo.git#section@anchor"
        );
    }

    #[test]
    fn url_without_userinfo_strips_credentials_but_keeps_path_at() {
        let raw =
            "https://token:SECRET@127.0.0.1:8080/org/repo@v1.git/info/refs?service=git-upload-pack";
        let redacted = url_without_userinfo(raw);
        assert_eq!(
            redacted,
            "https://127.0.0.1:8080/org/repo@v1.git/info/refs?service=git-upload-pack"
        );
        assert!(!redacted.contains("SECRET"));
        assert!(!redacted.contains("token"));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn redact_embedded_url_credentials_in_message() {
        let msg = "GET failed: https://user:pass@example.com/repo.git (timeout)";
        let out = redact_embedded_url_credentials(msg);
        assert!(out.contains("https://example.com/repo.git"));
        assert!(!out.contains("user:pass"));
    }

    #[test]
    fn parse_round_trips_named_fields() {
        let input =
            "protocol=https\nhost=example.com\nusername=alice\npassword=secret\n\nignored=x\n";
        let cred = Credential::parse(input);
        assert_eq!(cred.protocol.as_deref(), Some("https"));
        assert_eq!(cred.host.as_deref(), Some("example.com"));
        assert_eq!(cred.username.as_deref(), Some("alice"));
        assert_eq!(cred.password.as_deref(), Some("secret"));
        // Parsing stops at the blank line.
        assert!(cred.extra.is_empty());
    }

    #[test]
    fn serialize_uses_canonical_order() {
        let cred = Credential {
            protocol: Some("https".into()),
            host: Some("h".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        };
        assert_eq!(
            cred.serialize(),
            "protocol=https\nhost=h\nusername=u\npassword=p\n"
        );
    }

    #[test]
    fn target_url_reconstructed_from_fields() {
        let cred = Credential {
            protocol: Some("https".into()),
            host: Some("github.com".into()),
            path: Some("o/r.git".into()),
            ..Default::default()
        };
        assert_eq!(
            cred.target_url().as_deref(),
            Some("https://github.com/o/r.git")
        );
    }

    #[test]
    fn from_url_omits_http_path_unless_use_http_path_applies() {
        let plain = ConfigSet::new();
        let cred = Credential::from_url("http://127.0.0.1:9418/hw.git", &plain).expect("cred");
        assert_eq!(cred.protocol.as_deref(), Some("http"));
        assert_eq!(cred.host.as_deref(), Some("127.0.0.1:9418"));
        assert_eq!(cred.path, None);
        assert_eq!(cred.url.as_deref(), Some("http://127.0.0.1:9418/hw.git"));

        let mut scoped = ConfigSet::new();
        scoped
            .add_command_override("credential.https://example.com/team.useHttpPath", "true")
            .unwrap();
        let in_scope =
            Credential::from_url("https://example.com/team/a%20b.git/", &scoped).expect("cred");
        assert_eq!(in_scope.path.as_deref(), Some("team/a b.git"));
        let out_of_scope =
            Credential::from_url("https://example.com/other/repo.git", &scoped).expect("cred");
        assert_eq!(out_of_scope.path, None);
    }

    #[test]
    fn from_url_keeps_path_for_non_http_protocols_and_rejects_hostless_urls() {
        let cfg = ConfigSet::new();
        let cred = Credential::from_url("ftp://files.example/pub/repo.git", &cfg).expect("cred");
        assert_eq!(cred.path.as_deref(), Some("pub/repo.git"));
        assert_eq!(Credential::from_url("not a url", &cfg), None);
        assert_eq!(Credential::from_url("file:///tmp/repo.git", &cfg), None);
    }

    #[test]
    fn helper_request_drops_url_and_fills_missing_fields_from_it() {
        let provider = HelperCredentialProvider::new(ConfigSet::new());
        let decomposed =
            Credential::from_url("http://127.0.0.1:9418/hw.git", &ConfigSet::new()).expect("cred");
        assert_eq!(
            provider.helper_request(&decomposed).serialize(),
            "protocol=http\nhost=127.0.0.1:9418\n"
        );

        let url_only = Credential {
            url: Some("https://example.com/o/r.git".into()),
            username: Some("alice".into()),
            ..Default::default()
        };
        assert_eq!(
            provider.helper_request(&url_only).serialize(),
            "protocol=https\nhost=example.com\nusername=alice\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn helpers_never_receive_the_url_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("helper.log");
        let mut cfg = ConfigSet::new();
        cfg.add_command_override(
            "credential.http://127.0.0.1:9418/hw.git.helper",
            &format!(
                "!cat >>'{}'; echo username=u; echo password=p; :",
                log.display()
            ),
        )
        .unwrap();
        let provider = HelperCredentialProvider::new(cfg.clone());
        let input = Credential::from_url("http://127.0.0.1:9418/hw.git", &cfg).expect("cred");

        let filled = provider
            .fill(&input)
            .expect("path-scoped helper still matches");
        assert_eq!(filled.username.as_deref(), Some("u"));
        provider.approve(&filled).expect("store");

        assert_eq!(
            std::fs::read_to_string(&log).expect("helper log"),
            "protocol=http\nhost=127.0.0.1:9418\n\n\
             protocol=http\nhost=127.0.0.1:9418\nusername=u\npassword=p\n\n"
        );
    }

    fn env_with(vars: &[(&str, &str)]) -> Environment {
        Environment::from_vars(
            vars.iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
            PathBuf::from("/"),
        )
    }

    fn config_with(entries: &[(&str, &str)]) -> ConfigSet {
        let mut cfg = ConfigSet::new();
        for (key, value) in entries {
            cfg.add_command_override(key, value).unwrap();
        }
        cfg
    }

    #[test]
    fn askpass_program_prefers_git_askpass_then_core_askpass_then_ssh_askpass() {
        let cfg = config_with(&[("core.askPass", "/cfg/askpass")]);
        let all = env_with(&[("GIT_ASKPASS", "/env/git"), ("SSH_ASKPASS", "/env/ssh")]);
        assert_eq!(askpass_program(&all, &cfg), Some(PathBuf::from("/env/git")));
        let ssh_only = env_with(&[("SSH_ASKPASS", "/env/ssh")]);
        assert_eq!(
            askpass_program(&ssh_only, &cfg),
            Some(PathBuf::from("/cfg/askpass"))
        );
        assert_eq!(
            askpass_program(&ssh_only, &ConfigSet::new()),
            Some(PathBuf::from("/env/ssh"))
        );
        assert_eq!(
            askpass_program(&Environment::empty(), &ConfigSet::new()),
            None
        );
    }

    #[test]
    fn askpass_program_empty_value_disables_askpass_and_core_askpass_expands_home() {
        let cfg = config_with(&[("core.askPass", "~/bin/askpass")]);
        let empty_git = env_with(&[("GIT_ASKPASS", ""), ("SSH_ASKPASS", "/env/ssh")]);
        assert_eq!(askpass_program(&empty_git, &cfg), None);
        let home = env_with(&[("HOME", "/home/ci")]);
        assert_eq!(
            askpass_program(&home, &cfg),
            Some(PathBuf::from("/home/ci/bin/askpass"))
        );
        let empty_cfg = config_with(&[("core.askPass", "")]);
        assert_eq!(
            askpass_program(&env_with(&[("SSH_ASKPASS", "/env/ssh")]), &empty_cfg),
            None
        );
    }

    fn askpass_provider(
        cfg: ConfigSet,
        responses: Vec<RecordedResponse>,
    ) -> (HelperCredentialProvider, Arc<RecordingRunner>) {
        let runner = Arc::new(RecordingRunner::default());
        for response in responses {
            runner.push_response(response);
        }
        let provider = HelperCredentialProvider::new(cfg)
            .with_command_runner(runner.clone())
            .with_askpass(Some(PathBuf::from("/ci/askpass")));
        (provider, runner)
    }

    fn answer(text: &str) -> RecordedResponse {
        RecordedResponse::Output {
            stdout: text.as_bytes().to_vec(),
            code: 0,
        }
    }

    fn askpass_prompts(runner: &RecordingRunner) -> Vec<String> {
        runner
            .specs()
            .iter()
            .map(|spec| {
                assert_eq!(spec.program, PathBuf::from("/ci/askpass"));
                assert!(spec.shell.is_none(), "askpass runs without a shell");
                assert!(matches!(spec.stdin, CommandStdin::Null));
                assert_eq!(spec.args.len(), 1, "the prompt is the only argument");
                spec.args[0].to_string_lossy().into_owned()
            })
            .collect()
    }

    #[test]
    fn fill_asks_askpass_for_username_then_password_and_keeps_the_first_line() {
        let (provider, runner) = askpass_provider(
            ConfigSet::new(),
            vec![answer("ci\nignored\n"), answer("s3cret\r\n")],
        );
        let input =
            Credential::from_url("http://127.0.0.1:9431/hello.git", &ConfigSet::new()).unwrap();
        let filled = provider.fill(&input).expect("askpass fills");
        assert_eq!(filled.username.as_deref(), Some("ci"));
        assert_eq!(filled.password.as_deref(), Some("s3cret"));
        assert_eq!(
            askpass_prompts(&runner),
            [
                "Username for 'http://127.0.0.1:9431': ",
                "Password for 'http://ci@127.0.0.1:9431': ",
            ]
        );
    }

    #[test]
    fn fill_asks_only_for_the_password_when_the_url_names_the_user() {
        let cfg = config_with(&[("credential.useHttpPath", "true")]);
        let (provider, runner) = askpass_provider(cfg.clone(), vec![answer("tok")]);
        let input = Credential::from_url("https://ci@example.com/o/r.git", &cfg).unwrap();
        let filled = provider.fill(&input).expect("askpass fills");
        assert_eq!(filled.username.as_deref(), Some("ci"));
        assert_eq!(filled.password.as_deref(), Some("tok"));
        assert_eq!(
            askpass_prompts(&runner),
            ["Password for 'https://ci@example.com/o/r.git': "]
        );
    }

    #[test]
    fn askpass_prompt_is_sanitized_unless_disabled() {
        let cred = Credential {
            protocol: Some("https".into()),
            host: Some("example.com".into()),
            username: Some("a b/c\x1b".into()),
            path: Some("o/r e.git".into()),
            ..Default::default()
        };
        assert_eq!(
            cred.prompt("Password", true),
            "Password for 'https://a%20b%2Fc%1B@example.com/o/r%20e.git': "
        );
        assert_eq!(
            cred.prompt("Password", false),
            "Password for 'https://a b/c\x1b@example.com/o/r e.git': "
        );
        assert_eq!(Credential::default().prompt("Username", true), "Username: ");

        let (provider, runner) = askpass_provider(
            config_with(&[("credential.sanitizePrompt", "false")]),
            vec![answer("p")],
        );
        let input = Credential {
            username: Some("a b".into()),
            ..cred
        };
        provider.fill(&input).expect("askpass fills");
        assert_eq!(
            askpass_prompts(&runner),
            ["Password for 'https://a b@example.com/o/r e.git': "]
        );
    }

    #[test]
    fn url_scoped_sanitize_prompt_applies_only_to_matching_urls() {
        let cfg = config_with(&[("credential.https://example.com.sanitizePrompt", "false")]);
        for (url, expected) in [
            (
                "https://a%20b@example.com/r.git",
                "Password for 'https://a b@example.com': ",
            ),
            (
                "https://a%20b@other.example/r.git",
                "Password for 'https://a%20b@other.example': ",
            ),
        ] {
            let (provider, runner) = askpass_provider(cfg.clone(), vec![answer("p")]);
            let input = Credential::from_url(url, &cfg).unwrap();
            provider.fill(&input).expect("askpass fills");
            assert_eq!(askpass_prompts(&runner), [expected], "{url}");
        }
    }

    #[test]
    fn empty_askpass_answers_are_kept_like_git() {
        let (provider, runner) = askpass_provider(ConfigSet::new(), vec![answer("\n"), answer("")]);
        let input = Credential::from_url("https://example.com/r.git", &ConfigSet::new()).unwrap();
        let filled = provider
            .fill(&input)
            .expect("empty answers are still answers");
        assert_eq!(filled.username.as_deref(), Some(""));
        assert_eq!(filled.password.as_deref(), Some(""));
        assert_eq!(runner.specs().len(), 2, "each field is asked once");
    }

    #[test]
    fn failing_askpass_is_an_error_naming_the_program() {
        let (provider, _runner) =
            askpass_provider(ConfigSet::new(), vec![RecordedResponse::Exit(3)]);
        let input = Credential::from_url("https://example.com/r.git", &ConfigSet::new()).unwrap();
        let err = provider.fill(&input).expect_err("askpass failed");
        let message = err.to_string();
        assert!(message.contains("/ci/askpass"), "{message}");
        assert!(message.contains("status 3"), "{message}");
    }

    #[test]
    fn no_askpass_or_credential_interactive_never_keeps_fill_non_interactive() {
        let input = Credential::from_url("https://example.com/r.git", &ConfigSet::new()).unwrap();
        let err = HelperCredentialProvider::new(ConfigSet::new())
            .fill(&input)
            .expect_err("nothing can answer");
        assert_eq!(err.to_string(), NON_INTERACTIVE_MESSAGE);

        for value in ["never", "false"] {
            let (provider, runner) = askpass_provider(
                config_with(&[("credential.interactive", value)]),
                vec![answer("unused")],
            );
            let err = provider.fill(&input).expect_err("askpass disabled");
            assert_eq!(err.to_string(), NON_INTERACTIVE_MESSAGE);
            assert!(
                runner.specs().is_empty(),
                "askpass must not run for {value}"
            );
        }
    }

    #[test]
    fn from_url_takes_decoded_userinfo_and_keeps_it_out_of_the_url() {
        let cfg = ConfigSet::new();
        let cred =
            Credential::from_url("https://ci%40corp:p%3Ass@example.com/r.git", &cfg).unwrap();
        assert_eq!(cred.username.as_deref(), Some("ci@corp"));
        assert_eq!(cred.password.as_deref(), Some("p:ss"));
        assert_eq!(cred.url.as_deref(), Some("https://example.com/r.git"));
        assert!(cred.is_complete());

        let user_only = Credential::from_url("https://ci@example.com/r.git", &cfg).unwrap();
        assert_eq!(user_only.username.as_deref(), Some("ci"));
        assert_eq!(user_only.password, None);
    }

    #[cfg(unix)]
    #[test]
    fn helpers_answer_before_askpass_runs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let askpass = tmp.path().join("askpass");
        let log = tmp.path().join("askpass.log");
        std::fs::write(
            &askpass,
            format!(
                "#!/bin/sh\necho \"$1\" >>'{}'\necho from-askpass\n",
                log.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&askpass).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&askpass, perms).unwrap();

        let cfg = config_with(&[("credential.helper", "!echo username=helper-user; :")]);
        let provider = HelperCredentialProvider::new(cfg.clone()).with_askpass(Some(askpass));
        let input = Credential::from_url("https://example.com/r.git", &cfg).unwrap();
        let filled = provider.fill(&input).expect("helper plus askpass");
        assert_eq!(filled.username.as_deref(), Some("helper-user"));
        assert_eq!(filled.password.as_deref(), Some("from-askpass"));
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "Password for 'https://helper-user@example.com': \n"
        );

        let complete =
            config_with(&[("credential.helper", "!echo username=u; echo password=p; :")]);
        let provider = HelperCredentialProvider::new(complete.clone())
            .with_askpass(Some(tmp.path().join("askpass")));
        provider.fill(&input).expect("helper answers");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().lines().count(),
            1,
            "askpass must not run when a helper completes the credential"
        );
    }

    #[test]
    fn git_for_windows_exec_path_adds_mingw64_bin_not_libexec_bin() {
        // Forward slashes so the path parses on Unix CI as well as on Windows.
        let exec_path = PathBuf::from("C:/Program Files/Git/mingw64/libexec/git-core");
        let bins = credential_helper_bin_dirs_from_exec_path(&exec_path);
        let mingw_bin = PathBuf::from("C:/Program Files/Git/mingw64/bin");
        assert!(
            bins.contains(&mingw_bin),
            "GCM lives in mingw64/bin, got {bins:?}"
        );
        let libexec_bin = PathBuf::from("C:/Program Files/Git/mingw64/libexec/bin");
        assert!(
            bins.contains(&libexec_bin),
            "libexec/bin is still listed for other layouts: {bins:?}"
        );
    }

    #[test]
    fn resolve_manager_finds_helper_in_git_for_windows_layout() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let git_core = tmp.path().join("mingw64/libexec/git-core");
        let mingw_bin = tmp.path().join("mingw64/bin");
        fs::create_dir_all(&git_core).expect("git-core dir");
        fs::create_dir_all(&mingw_bin).expect("mingw bin dir");
        fs::write(mingw_bin.join("git-credential-manager"), b"").expect("fake gcm");

        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        push_exec_path_candidate(&mut candidates, &mut seen, git_core);

        let resolved = candidates.iter().find_map(|ep| {
            let candidate = ep.join("git-credential-manager");
            credential_helper_executable_at(&candidate)
        });
        assert_eq!(
            resolved,
            Some(mingw_bin.join("git-credential-manager")),
            "manager helper must resolve via mingw64/bin, searched {candidates:?}"
        );
    }

    #[test]
    fn installed_helpers_are_found_in_the_exec_path() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("git-credential-osxkeychain");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        let provider = HelperCredentialProvider::new(ConfigSet::default());
        provider
            .probed_git_exec_path
            .set(vec![dir.path().to_path_buf()])
            .unwrap();
        assert!(provider.helper_installed("osxkeychain"));
        assert!(!provider.helper_installed("no-such-helper-anywhere"));
        let suggested = provider.suggested_helper();
        assert!(
            suggested.is_none_or(|name| SUGGESTED_HELPERS.contains(&name)),
            "{suggested:?}"
        );
        #[cfg(target_os = "macos")]
        assert_eq!(suggested, Some("osxkeychain"));
    }
}
