//! `grit credential` — retrieve and store user credentials.
//!
//! Implements the Git credential helper protocol:
//! - `fill`    — read credential spec from stdin, output filled credentials
//! - `approve` — mark credentials as good (`store` in helpers)
//! - `reject`  — mark credentials as bad (`erase` in helpers)
//!
//! Reads key=value pairs (protocol, host, username, password, path) from
//! stdin and passes them through the configured credential helpers.

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use time::OffsetDateTime;
use url::Url;

/// Arguments for `grit credential`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: CredentialAction,
}

#[derive(Debug, Subcommand)]
pub enum CredentialAction {
    /// Read credential spec from stdin, output filled credentials.
    Fill,
    /// Mark credentials as good.
    Approve,
    /// Mark credentials as bad.
    Reject,
    /// Announce supported credential protocol capabilities.
    Capability,
}

#[derive(Clone, Debug, Default)]
struct Credential {
    entries: Vec<CredentialEntry>,
}

#[derive(Clone, Debug)]
struct CredentialEntry {
    key: String,
    value: String,
}

impl Credential {
    fn read_from_stdin() -> Result<Self> {
        let stdin = io::stdin();
        Self::read_from_lines(stdin.lock().lines())
    }

    fn read_from_bytes(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        let lines = text.lines().map(|line| Ok(line.to_string()));
        Self::read_from_lines(lines).unwrap_or_default()
    }

    fn read_from_lines<I>(lines: I) -> Result<Self>
    where
        I: IntoIterator<Item = std::io::Result<String>>,
    {
        let mut out = Self::default();
        for line in lines {
            let line = line?;
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once('=') {
                out.apply_entry(key, value);
            }
        }
        Ok(out)
    }

    fn apply_entry(&mut self, key: &str, value: &str) {
        if key.ends_with("[]") {
            if value.is_empty() {
                self.remove_all(key);
            } else {
                self.entries.push(CredentialEntry {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
            return;
        }
        self.set(key, value.to_string());
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    fn values(&self, key: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
            .collect()
    }

    fn has_key(&self, key: &str) -> bool {
        self.entries.iter().any(|entry| entry.key == key)
    }

    fn has_capability(&self, capability: &str) -> bool {
        self.values("capability[]").contains(&capability)
    }

    fn set(&mut self, key: &str, value: String) {
        self.remove_all(key);
        let insert_at = self
            .preferred_insert_position(key)
            .unwrap_or(self.entries.len());
        self.entries.insert(
            insert_at,
            CredentialEntry {
                key: key.to_string(),
                value,
            },
        );
    }

    fn set_if_missing(&mut self, key: &str, value: String) {
        if !self.has_key(key) {
            self.set(key, value);
        }
    }

    fn remove_all(&mut self, key: &str) {
        self.entries.retain(|entry| entry.key != key);
    }

    fn remove_path_for_http(&mut self) {
        if matches!(self.get("protocol"), Some("http" | "https")) {
            self.remove_all("path");
        }
    }

    fn sanitize_helper_response(&mut self, caller: &Credential) {
        if !caller.has_capability("authtype") {
            self.remove_all("authtype");
            self.remove_all("credential");
            self.remove_all("ephemeral");
            self.remove_capability("authtype");
        }
        if !caller.has_capability("state") {
            self.remove_all("state[]");
            self.remove_all("continue");
            self.remove_capability("state");
        }
    }

    fn remove_capability(&mut self, capability: &str) {
        self.entries
            .retain(|entry| !(entry.key == "capability[]" && entry.value == capability));
    }

    fn set_output_capabilities(&mut self, capabilities: &[String]) {
        self.remove_all("capability[]");
        for capability in capabilities.iter().rev() {
            self.entries.insert(
                0,
                CredentialEntry {
                    key: "capability[]".to_string(),
                    value: capability.clone(),
                },
            );
        }
    }

    fn merge_helper_response(&mut self, helper_output: &Credential, caller: &Credential) {
        for entry in &helper_output.entries {
            if matches!(entry.key.as_str(), "authtype" | "credential" | "ephemeral")
                && !caller.has_capability("authtype")
            {
                continue;
            }
            if matches!(entry.key.as_str(), "state[]" | "continue")
                && !caller.has_capability("state")
            {
                continue;
            }
            if entry.key == "capability[]" {
                continue;
            }
            self.apply_entry(&entry.key, &entry.value);
        }
        self.sanitize_helper_response(caller);
    }

    fn is_complete(&self) -> bool {
        self.has_preencoded_credential()
            || (self.get("username").is_some_and(|s| !s.is_empty())
                && self.get("password").is_some_and(|s| !s.is_empty()))
    }

    fn has_preencoded_credential(&self) -> bool {
        self.get("authtype").is_some_and(|s| !s.is_empty())
            && self.get("credential").is_some_and(|s| !s.is_empty())
    }

    fn password_expired(&self, now_utc: i64) -> bool {
        self.get("password_expiry_utc")
            .and_then(|value| value.parse::<i64>().ok())
            .is_some_and(|expiry| expiry <= now_utc)
    }

    fn remove_secret_fields(&mut self) {
        self.remove_all("password");
        self.remove_all("password_expiry_utc");
        self.remove_all("oauth_refresh_token");
    }

    fn should_store(&self, now_utc: i64) -> bool {
        if self.password_expired(now_utc) {
            return false;
        }
        self.has_key("password") || self.has_key("credential")
    }

    fn target_url(&self) -> Option<String> {
        if let Some(u) = self.get("url").filter(|u| !u.trim().is_empty()) {
            return Some(u.to_string());
        }
        let protocol = self.get("protocol")?;
        let host = self.get("host")?;
        let mut url = format!("{protocol}://");
        if let Some(username) = self.get("username").filter(|u| !u.is_empty()) {
            url.push_str(username);
            url.push('@');
        }
        url.push_str(host);
        if let Some(path) = self.get("path").filter(|p| !p.is_empty()) {
            if !path.starts_with('/') {
                url.push('/');
            }
            url.push_str(path);
        }
        Some(url)
    }

    fn write_to(&self, mut out: impl Write) -> Result<()> {
        for entry in &self.entries {
            writeln!(out, "{}={}", entry.key, entry.value)?;
        }
        Ok(())
    }

    fn write_to_child_stdin(&self, stdin: &mut impl Write) -> Result<()> {
        for entry in &self.entries {
            writeln!(stdin, "{}={}", entry.key, entry.value)?;
        }
        Ok(())
    }

    fn preferred_insert_position(&self, key: &str) -> Option<usize> {
        if key == "capability[]" {
            return Some(
                self.entries
                    .iter()
                    .position(|entry| entry.key != "capability[]")
                    .unwrap_or(self.entries.len()),
            );
        }
        let rank = field_rank(key);
        self.entries
            .iter()
            .position(|entry| field_rank(&entry.key) > rank)
    }
}

fn field_rank(key: &str) -> u8 {
    match key {
        "capability[]" => 0,
        "authtype" => 10,
        "credential" => 11,
        "ephemeral" => 12,
        "protocol" => 20,
        "host" => 21,
        "path" => 22,
        "username" => 30,
        "password" => 31,
        "password_expiry_utc" => 32,
        "oauth_refresh_token" => 33,
        "state[]" => 40,
        "continue" => 41,
        "quit" => 90,
        _ => 80,
    }
}

/// Parse credential key=value pairs from stdin until a blank line or EOF.
fn read_credential_input() -> Result<Credential> {
    Credential::read_from_stdin()
}

fn host_header_value(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

/// Normalize `url=<scheme>://...` into protocol/host/path/username/password fields.
///
/// Git helpers commonly receive either split fields or a single `url=...` input.
fn normalize_url_field(creds: &mut Credential, config: &grit_lib::config::ConfigSet) -> Result<()> {
    let Some(raw_url) = creds.get("url").map(ToOwned::to_owned) else {
        return Ok(());
    };
    reject_url_with_newline(&raw_url)?;
    check_raw_url_protected_values(&raw_url, config)?;
    normalize_url_field_lenient(creds, &raw_url)?;
    check_protected_credential_values(creds, config)
}

fn normalize_url_field_lenient(creds: &mut Credential, raw_url: &str) -> Result<()> {
    let (protocol, rest) = raw_url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("credential url cannot be parsed: {raw_url}"))?;
    creds.set_if_missing("protocol", protocol.to_string());
    let (authority, path_part) = split_url_authority_and_path(rest);
    let (userinfo, host) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(userinfo, host)| (Some(userinfo), host));
    creds.set_if_missing("host", percent_decode_lossy(host));
    if let Some(userinfo) = userinfo {
        let (username, password) = userinfo
            .split_once(':')
            .map_or((userinfo, None), |(u, p)| (u, Some(p)));
        if !creds.has_key("username") && !username.contains('%') {
            creds.set("username", username.to_string());
        }
        if !creds.has_key("password") {
            if let Some(password) = password {
                creds.set("password", percent_decode_lossy(password));
            }
        }
    }
    let path = path_part.trim_start_matches('/');
    if !path.is_empty() {
        creds.set_if_missing("path", percent_decode_lossy(path));
    }
    creds.remove_all("url");
    Ok(())
}

fn split_url_authority_and_path(rest: &str) -> (&str, &str) {
    let idx = rest
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '/' | '?' | '#').then_some(idx))
        .unwrap_or(rest.len());
    (&rest[..idx], &rest[idx..])
}

fn reject_url_with_newline(raw_url: &str) -> Result<()> {
    let decoded = percent_decode_lossy(raw_url);
    if decoded.contains('\n') {
        eprintln!("warning: url contains a newline in its path component: {raw_url}");
        bail!("fatal: credential url cannot be parsed: {raw_url}");
    }
    Ok(())
}

fn check_raw_url_protected_values(
    raw_url: &str,
    config: &grit_lib::config::ConfigSet,
) -> Result<()> {
    if !credential_protect_protocol(config, None) {
        return Ok(());
    }
    let decoded = percent_decode_lossy(raw_url);
    let Some((protocol, rest)) = decoded.split_once("://") else {
        return Ok(());
    };
    if protocol.contains('\r') {
        bail!(
            "fatal: credential value for protocol contains carriage return\nIf this is intended, set `credential.protectProtocol=false`"
        );
    }
    let (authority, _) = split_url_authority_and_path(rest);
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if host.contains('\r') {
        bail!(
            "fatal: credential value for host contains carriage return\nIf this is intended, set `credential.protectProtocol=false`"
        );
    }
    Ok(())
}

fn check_protected_credential_values(
    creds: &Credential,
    config: &grit_lib::config::ConfigSet,
) -> Result<()> {
    if !credential_protect_protocol(config, creds.target_url().as_deref()) {
        return Ok(());
    }
    for key in ["protocol", "host"] {
        let Some(value) = creds.get(key) else {
            continue;
        };
        if value.contains('\r') {
            bail!(
                "fatal: credential value for {key} contains carriage return\nIf this is intended, set `credential.protectProtocol=false`"
            );
        }
        if value.contains('\n') {
            bail!(
                "fatal: credential value for {key} contains newline\nIf this is intended, set `credential.protectProtocol=false`"
            );
        }
    }
    Ok(())
}

fn credential_protect_protocol(
    config: &grit_lib::config::ConfigSet,
    target_url: Option<&str>,
) -> bool {
    credential_config_value(config, target_url, "protectProtocol")
        .as_deref()
        .map(|value| grit_lib::config::parse_bool(value).unwrap_or(true))
        .unwrap_or(true)
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

/// Discover the `.git` directory by walking up from the current directory.
fn find_git_dir() -> Option<std::path::PathBuf> {
    // Check GIT_DIR env var first
    if let Ok(d) = std::env::var("GIT_DIR") {
        let p = std::path::PathBuf::from(&d);
        if p.is_dir() {
            return Some(p);
        }
    }
    // Walk up from cwd looking for .git
    if let Ok(mut dir) = std::env::current_dir() {
        loop {
            let dot_git = dir.join(".git");
            if dot_git.is_dir() {
                return Some(dot_git);
            }
            // Bare repo check
            if dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
                return Some(dir);
            }
            if !dir.pop() {
                break;
            }
        }
    }
    None
}

/// Build the effective `credential.helper` list in Git order.
///
/// Git walks every `credential.helper` and `credential.<URL>.helper` config entry in
/// load order. URL-scoped entries only apply when the subsection pattern matches
/// `target_url` (per Git's URL-match rules). For every applicable entry, a non-empty
/// value is appended to the helper list and an empty value resets it (Git's
/// `string_list_clear` semantics in `credential_apply_config_cb`).
///
/// `target_url` is the URL we're authenticating against (e.g.
/// `https://github.com/owner/repo.git`). When `None`, only unscoped
/// `credential.helper` entries contribute.
fn credential_helpers(
    config: &grit_lib::config::ConfigSet,
    target_url: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    for entry in config.entries() {
        let key = &entry.key;
        if key.contains('\n') || key.to_ascii_lowercase().contains("%0a") {
            eprintln!("warning: skipping credential lookup for key with newline");
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
            if credential_config_subsection_invalid(subsection) {
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

fn credential_config_value(
    config: &grit_lib::config::ConfigSet,
    target_url: Option<&str>,
    variable_name: &str,
) -> Option<String> {
    let mut out = None;
    for entry in config.entries() {
        let key = &entry.key;
        if key.contains('\n') || key.to_ascii_lowercase().contains("%0a") {
            eprintln!("warning: skipping credential lookup for key with newline");
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
            if credential_config_subsection_invalid(subsection) {
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

fn credential_url_matches(pattern: &str, target: &str) -> bool {
    let pattern = percent_decode_lossy(pattern);
    if pattern.contains('\n') {
        eprintln!("warning: skipping credential lookup for key with newline");
        return false;
    }
    let pattern = pattern.trim_end_matches('/');
    let pattern_no_user = strip_url_userinfo(pattern);
    let pattern_after_user = pattern.rsplit_once('@').map(|(_, host)| host);
    let target = target.trim_end_matches('/');
    let target_no_user = strip_url_userinfo(target);
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

fn credential_config_subsection_invalid(subsection: &str) -> bool {
    if percent_decode_lossy(subsection).contains('\n') {
        eprintln!("warning: skipping credential lookup for key with newline");
        return true;
    }
    false
}

fn warn_invalid_credential_config_env() {
    if std::env::var("GIT_CONFIG_PARAMETERS").is_ok_and(|params| {
        let lower = params.to_ascii_lowercase();
        params.contains('\n') || lower.contains("%0a") || lower.contains("credential.with")
    }) {
        eprintln!("warning: skipping credential lookup for key with newline");
    }
    if let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT") {
        if let Ok(count) = count_str.parse::<usize>() {
            for i in 0..count {
                let key_var = format!("GIT_CONFIG_KEY_{i}");
                if std::env::var(&key_var)
                    .is_ok_and(|key| key.contains('\n') || key.to_ascii_lowercase().contains("%0a"))
                {
                    eprintln!("warning: skipping credential lookup for key with newline");
                    break;
                }
            }
        }
    }
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

fn strip_url_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url
            .rsplit_once('@')
            .map_or(url, |(_, host)| host)
            .to_string();
    };
    rest.rsplit_once('@')
        .map_or_else(|| url.to_string(), |(_, host)| format!("{scheme}://{host}"))
}

fn target_path_component(url: &str) -> &str {
    let rest = strip_url_scheme(url);
    split_url_authority_and_path(rest).1
}

/// Directories to search for `git-credential-*` the same way Git does (`exec_path` before `PATH`).
///
/// Git installs helpers under `/usr/libexec/git-core` on macOS; they are not on `PATH`, so a bare
/// [`Command::new`] lookup fails while `git credential` still works.
fn credential_helper_exec_path_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(ep) = std::env::var("GIT_EXEC_PATH") {
        let p = PathBuf::from(ep.trim());
        if p.is_dir() {
            v.push(p);
        }
    }
    for candidate in [
        "/usr/libexec/git-core",
        "/Library/Developer/CommandLineTools/usr/libexec/git-core",
        "/opt/homebrew/opt/git/libexec/git-core",
        "/opt/homebrew/libexec/git-core",
        "/usr/lib/git-core",
        "/usr/local/libexec/git-core",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_dir() {
            v.push(p);
        }
    }
    if let Some(p) = crate::git_exec_path_for_helpers(None) {
        v.push(p);
    }
    v
}

fn resolve_credential_helper_executable(helper_program: &str) -> PathBuf {
    if helper_program.contains('/') {
        return PathBuf::from(helper_program);
    }
    if helper_program.starts_with("git-credential-") {
        let cmd = helper_program
            .strip_prefix("git-")
            .unwrap_or(helper_program);
        for ep in credential_helper_exec_path_candidates() {
            if let Some(p) = crate::alias::find_git_external_helper(cmd, Some(&ep)) {
                return p;
            }
        }
        if let Some(p) = crate::alias::find_git_external_helper(cmd, None) {
            return p;
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
/// The helper is invoked with one action argument (`get`, `store`, `erase`)
/// after any arguments from the configured helper string.
/// Credential fields are written to stdin as `key=value` lines followed by a
/// blank line; stdout is parsed back into key/value pairs.
fn invoke_helper(helper: &str, action: &str, creds: &Credential) -> Result<Credential> {
    let helper_words = shell_words::split(helper)
        .map_err(|e| anyhow::anyhow!("invalid credential.helper '{helper}': {e}"))?;
    let (first_word, extra_args) = if let Some((first, rest)) = helper_words.split_first() {
        (first.as_str(), rest)
    } else {
        ("", &[][..])
    };

    let mut child = if let Some(shell_cmd) = helper.strip_prefix('!') {
        Command::new("sh")
            .arg("-c")
            .arg(format!("{shell_cmd} {action}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to run credential helper shell '{helper}': {e}"))?
    } else if matches!(
        first_word,
        "store" | "cache" | "git-credential-store" | "git-credential-cache"
    ) {
        let subcmd = if first_word.ends_with("store") {
            "credential-store"
        } else {
            "credential-cache"
        };
        let exe = std::env::current_exe().context("resolve current executable")?;
        let mut cmd = Command::new(exe);
        cmd.arg(subcmd);
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.arg(action);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!("failed to run built-in credential helper '{subcmd}': {e}")
            })?
    } else {
        let helper_program =
            if first_word.contains('/') || first_word.starts_with("git-credential-") {
                first_word.to_string()
            } else {
                format!("git-credential-{first_word}")
            };
        let resolved = resolve_credential_helper_executable(&helper_program);
        let mut cmd = Command::new(&resolved);
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.arg(action);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!("failed to run credential helper '{helper_program}': {e}")
            })?
    };

    // Write credential fields to helper's stdin, followed by blank line.
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("credential helper missing stdin"))?;
        creds.write_to_child_stdin(stdin)?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "credential helper '{}' exited with status {}",
            helper,
            output.status
        );
    }

    Ok(Credential::read_from_bytes(&output.stdout))
}

fn askpass_program(config: &grit_lib::config::ConfigSet) -> Option<String> {
    std::env::var("GIT_ASKPASS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| config.get("core.askpass"))
        .or_else(|| {
            std::env::var("SSH_ASKPASS")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
}

fn run_askpass(config: &grit_lib::config::ConfigSet, prompt: &str) -> Result<String> {
    let Some(program) = askpass_program(config) else {
        return prompt_terminal(prompt);
    };
    let out = Command::new(&program)
        .arg(prompt)
        .output()
        .with_context(|| format!("run askpass ({program})"))?;
    if !out.stderr.is_empty() {
        let _ = std::io::stderr().write_all(&out.stderr);
    }
    if !out.status.success() {
        bail!("askpass failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(unix)]
fn prompt_terminal(prompt: &str) -> Result<String> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("open /dev/tty for credential prompt")?;
    tty.write_all(prompt.as_bytes())?;
    tty.flush()?;

    let mut reader = std::io::BufReader::new(tty.try_clone()?);
    let mut value = String::new();
    reader.read_line(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(unix))]
fn prompt_terminal(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

fn credential_prompt_origin(
    creds: &Credential,
    config: &grit_lib::config::ConfigSet,
) -> Result<String> {
    let protocol = creds
        .get("protocol")
        .ok_or_else(|| anyhow::anyhow!("missing protocol"))?;
    let host = creds
        .get("host")
        .ok_or_else(|| anyhow::anyhow!("missing host"))?;
    let mut out = format!(
        "{}://{}",
        sanitize_prompt_component(protocol, config),
        sanitize_prompt_component(host, config)
    );
    if let Some(path) = creds.get("path").filter(|p| !p.is_empty()) {
        out.push('/');
        out.push_str(&sanitize_prompt_component(path, config));
    }
    Ok(out)
}

fn sanitize_prompt_component(value: &str, config: &grit_lib::config::ConfigSet) -> String {
    let sanitize = credential_config_value(config, None, "sanitizePrompt")
        .as_deref()
        .map(|value| grit_lib::config::parse_bool(value).unwrap_or(true))
        .unwrap_or(true);
    if !sanitize {
        return value.to_string();
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        percent_encode_prompt_component(value)
    } else {
        value.to_string()
    }
}

fn percent_encode_prompt_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn ask_for_missing_fields(
    creds: &mut Credential,
    config: &grit_lib::config::ConfigSet,
) -> Result<()> {
    if !creds.has_key("username") {
        let prompt = format!(
            "Username for '{}': ",
            credential_prompt_origin(creds, config)?
        );
        let username = run_askpass(config, &prompt)?;
        creds.set("username", username);
    }
    if !creds.has_key("password") {
        let protocol = creds.get("protocol").unwrap_or_default();
        let host = creds.get("host").unwrap_or_default();
        let username = creds.get("username").unwrap_or_default();
        let encoded_user = percent_encode_prompt_component(username);
        let host = sanitize_prompt_component(host, config);
        let mut origin = format!("{protocol}://{encoded_user}@{host}");
        if let Some(path) = creds.get("path").filter(|p| !p.is_empty()) {
            origin.push('/');
            origin.push_str(&sanitize_prompt_component(path, config));
        }
        let prompt = format!("Password for '{origin}': ");
        let password = run_askpass(config, &prompt)?;
        creds.set("password", password);
    }
    Ok(())
}

fn credential_interactive_allowed(config: &grit_lib::config::ConfigSet) -> bool {
    config
        .get("credential.interactive")
        .as_deref()
        .map(|value| grit_lib::config::parse_bool(value).unwrap_or(true))
        .unwrap_or(true)
}

fn apply_config_defaults(
    creds: &mut Credential,
    config: &grit_lib::config::ConfigSet,
    target_url: Option<&str>,
) {
    if !credential_config_value(config, target_url, "useHttpPath")
        .as_deref()
        .map(|value| grit_lib::config::parse_bool(value).unwrap_or(false))
        .unwrap_or(false)
    {
        creds.remove_path_for_http();
    }
    if !creds.has_key("username") {
        if let Some(username) = credential_config_value(config, target_url, "username") {
            creds.set("username", username);
        }
    }
}

fn check_required_fields(creds: &Credential) -> Result<()> {
    if !creds.has_key("protocol") {
        bail!("fatal: refusing to work with credential missing protocol field");
    }
    if !creds.has_key("host") {
        bail!("fatal: refusing to work with credential missing host field");
    }
    Ok(())
}

/// Run `grit credential`.
pub fn run(args: Args) -> Result<()> {
    if matches!(args.action, CredentialAction::Capability) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        writeln!(out, "version 0")?;
        writeln!(out, "capability authtype")?;
        writeln!(out, "capability state")?;
        return Ok(());
    }

    let mut creds = read_credential_input()?;
    let git_dir = find_git_dir();
    warn_invalid_credential_config_env();
    let config = grit_lib::config::ConfigSet::load(git_dir.as_deref(), true).unwrap_or_default();
    normalize_url_field(&mut creds, &config)?;
    let target_url = creds.target_url();
    apply_config_defaults(&mut creds, &config, target_url.as_deref());
    check_protected_credential_values(&creds, &config)?;
    let now_utc = OffsetDateTime::now_utc().unix_timestamp();

    match args.action {
        CredentialAction::Fill => {
            check_required_fields(&creds)?;

            let mut filled = creds.clone();
            let mut advertised_capabilities = Vec::new();
            let mut unusable_preencoded_credential = false;
            if !filled.is_complete() {
                for helper in credential_helpers(&config, target_url.as_deref()) {
                    let response = invoke_helper(&helper, "get", &filled)?;
                    if response
                        .get("quit")
                        .is_some_and(|v| v == "1" || v == "true")
                    {
                        bail!("fatal: credential helper '{helper}' told us to quit");
                    }
                    for capability in response.values("capability[]") {
                        if creds.has_capability(capability)
                            && !advertised_capabilities.iter().any(|c| c == capability)
                        {
                            advertised_capabilities.push(capability.to_string());
                        }
                    }
                    if response.has_preencoded_credential() && !creds.has_capability("authtype") {
                        unusable_preencoded_credential = true;
                    }
                    filled.merge_helper_response(&response, &creds);
                    if filled.password_expired(now_utc) {
                        filled.remove_secret_fields();
                    }
                    if filled.is_complete() {
                        break;
                    }
                }
            }
            if !filled.is_complete() {
                if unusable_preencoded_credential {
                    filled.remove_all("authtype");
                    filled.remove_all("credential");
                    filled.remove_all("ephemeral");
                } else if !credential_interactive_allowed(&config) {
                    bail!("terminal prompts disabled");
                } else {
                    ask_for_missing_fields(&mut filled, &config)?;
                }
            }

            filled.set_output_capabilities(&advertised_capabilities);
            let stdout = io::stdout();
            filled.write_to(stdout.lock())?;
        }
        CredentialAction::Approve => {
            if creds.should_store(now_utc) {
                for helper in credential_helpers(&config, target_url.as_deref()) {
                    let _ = invoke_helper(&helper, "store", &creds)?;
                }
            }
        }
        CredentialAction::Reject => {
            for helper in credential_helpers(&config, target_url.as_deref()) {
                let _ = invoke_helper(&helper, "erase", &creds)?;
            }
        }
        CredentialAction::Capability => {}
    }

    Ok(())
}
