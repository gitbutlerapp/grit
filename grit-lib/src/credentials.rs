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
//!   `git-credential-*` binaries).
//!
//! ## Non-interactive by design
//!
//! Unlike the CLI, [`HelperCredentialProvider`] **never** prompts on a TTY or
//! via askpass. When the configured helpers cannot supply a usable
//! username/password, [`fill`](CredentialProvider::fill) returns a typed
//! [`Error::Message`] (see [`NON_INTERACTIVE_MESSAGE`]) rather than blocking on
//! `/dev/tty`. Interactive prompting is an explicitly opt-in concern an
//! embedder can layer on top.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::command_runner::{
    system_command_runner, CommandEnvironment, CommandRunner, CommandSpec, CommandStdin,
    CommandStdio, ShellInvocation,
};
use crate::config::{parse_bool, ConfigSet};
use crate::error::{Error, Result};

/// Message returned (as [`Error::Message`]) when credentials are required but
/// no configured helper could supply a complete username/password and
/// interactive prompting is disallowed.
pub const NON_INTERACTIVE_MESSAGE: &str = "credentials required but unavailable (non-interactive)";

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
/// **Never prompts.** If no helper yields a complete credential,
/// [`fill`](CredentialProvider::fill) returns [`Error::Message`] with
/// [`NON_INTERACTIVE_MESSAGE`].
pub struct HelperCredentialProvider {
    config: ConfigSet,
    command_runner: Arc<dyn CommandRunner>,
    probed_git_exec_path: OnceLock<Vec<PathBuf>>,
}

impl HelperCredentialProvider {
    /// Build a provider from a loaded [`ConfigSet`].
    pub fn new(config: ConfigSet) -> Self {
        Self {
            config,
            command_runner: system_command_runner(),
            probed_git_exec_path: OnceLock::new(),
        }
    }

    /// Override the subprocess runner (tests and embedders).
    #[must_use]
    pub fn with_command_runner(mut self, runner: Arc<dyn CommandRunner>) -> Self {
        self.command_runner = runner;
        self
    }

    /// The ordered helper list applicable to `target_url`.
    fn helpers(&self, target_url: Option<&str>) -> Vec<String> {
        credential_helpers(&self.config, target_url)
    }

    fn exec_path_candidates(&self) -> Vec<PathBuf> {
        credential_helper_exec_path_candidates(
            self.command_runner.as_ref(),
            &self.probed_git_exec_path,
        )
    }
}

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
                &filled,
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
        // No helper could complete the credential. The library default does NOT
        // fall back to an interactive prompt; surface a typed error instead.
        Err(Error::Message(NON_INTERACTIVE_MESSAGE.to_string()))
    }

    fn approve(&self, cred: &Credential) -> Result<()> {
        let target_url = cred.target_url();
        for helper in self.helpers(target_url.as_deref()) {
            invoke_helper(
                self.command_runner.as_ref(),
                &helper,
                "store",
                cred,
                &self.exec_path_candidates(),
            )?;
        }
        Ok(())
    }

    fn reject(&self, cred: &Credential) -> Result<()> {
        let target_url = cred.target_url();
        for helper in self.helpers(target_url.as_deref()) {
            invoke_helper(
                self.command_runner.as_ref(),
                &helper,
                "erase",
                cred,
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
/// (no scheme). The result is safe to show in errors and logs.
pub fn url_without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url
            .rsplit_once('@')
            .map_or(url, |(_, host)| host)
            .to_string();
    };
    rest.rsplit_once('@')
        .map_or_else(|| url.to_string(), |(_, host)| format!("{scheme}://{host}"))
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

    let mut stdin_body = creds.serialize();
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
}
