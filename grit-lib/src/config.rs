//! Git-compatible configuration file parser and accessor.
//!
//! Supports the standard Git config file format:
//!
//! ```text
//! [section]
//!     key = value
//! [section "subsection"]
//!     key = value
//! ```
//!
//! # Multi-file layering
//!
//! Git reads configuration from several files in priority order:
//!
//! 1. System (`/etc/gitconfig`)
//! 2. Global (`~/.gitconfig` or `$XDG_CONFIG_HOME/git/config`)
//! 3. Local (`.git/config`)
//! 4. Worktree (`.git/config.worktree`)
//! 5. Command-line (`-c key=value` or `GIT_CONFIG_*`)
//!
//! [`ConfigSet`] merges all layers; last-wins for single-valued keys.
//!
//! # Include directives
//!
//! `[include] path = <path>` and `[includeIf "<condition>"] path = <path>`
//! are supported. Conditions: `gitdir:`, `gitdir/i:`, `onbranch:`,
//! and `hasconfig:remote.*.url:`.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::refs;
use crate::wildmatch::{wildmatch, WM_CASEFOLD, WM_PATHNAME};

/// The scope (origin) of a configuration value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigScope {
    /// System-wide configuration (`/etc/gitconfig`).
    System,
    /// Per-user global configuration (`~/.gitconfig` or XDG).
    Global,
    /// Repository-local configuration (`.git/config`).
    Local,
    /// Per-worktree configuration (`.git/config.worktree`).
    Worktree,
    /// Command-line overrides (`-c key=value`).
    Command,
}

impl fmt::Display for ConfigScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => write!(f, "system"),
            Self::Global => write!(f, "global"),
            Self::Local => write!(f, "local"),
            Self::Worktree => write!(f, "worktree"),
            Self::Command => write!(f, "command"),
        }
    }
}

/// A single configuration entry with its origin metadata.
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    /// Fully-qualified key in canonical form: `section.subsection.name`
    /// (section and name lowercased; subsection preserves case).
    pub key: String,
    /// The raw string value, or `None` for a boolean-true bare key.
    pub value: Option<String>,
    /// Which scope this entry came from.
    pub scope: ConfigScope,
    /// The file this entry was read from (if file-backed).
    pub file: Option<PathBuf>,
    /// One-based line number in the source file.
    pub line: usize,
}

/// Where a [`ConfigFile`] was loaded from for Git include semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigIncludeOrigin {
    /// Normal path on disk (`-f`, global/local config files, etc.).
    Disk,
    /// `--file -` (stdin).
    Stdin,
    /// Synthetic file built from `GIT_CONFIG_PARAMETERS` / `git -c`.
    CommandLine,
    /// `git config --blob=…`.
    Blob,
}

/// A parsed configuration file that preserves the raw text for round-trip
/// editing (set/unset/rename-section/remove-section).
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// The path to this config file on disk.
    pub path: PathBuf,
    /// The scope this file represents.
    pub scope: ConfigScope,
    /// Parsed entries (in file order).
    pub entries: Vec<ConfigEntry>,
    /// Raw lines of the file (for round-trip editing).
    raw_lines: Vec<String>,
    /// Source kind for `[include]` resolution (Git `CONFIG_ORIGIN_*`).
    pub include_origin: ConfigIncludeOrigin,
}

/// A merged view across all configuration scopes.
///
/// Entries are stored in file-order within each scope; scopes are layered
/// in priority order (system < global < local < worktree < command).
#[derive(Debug, Clone, Default)]
pub struct ConfigSet {
    /// All entries across all scopes, in load order.
    entries: Vec<ConfigEntry>,
}

/// Context for evaluating `[includeIf]` conditions (`gitdir:`, `onbranch:`, `hasconfig:`).
#[derive(Debug, Clone, Default)]
pub struct IncludeContext {
    /// Git directory path used for `gitdir:` matching (may contain unresolved symlinks).
    pub git_dir: Option<PathBuf>,
    /// When true, `git -c include.path=relative` fails instead of ignoring the include.
    pub command_line_relative_include_is_error: bool,
}

/// Options controlling how [`ConfigSet::load_with_options`] merges files and includes.
#[derive(Debug, Clone)]
pub struct LoadConfigOptions {
    /// Load `/etc/gitconfig` (unless `GIT_CONFIG_NOSYSTEM` is enabled).
    pub include_system: bool,
    /// Expand `[include]` / `[includeIf]` while reading file-backed layers.
    pub process_includes: bool,
    /// Expand includes for synthetic command-line config built from `GIT_CONFIG_PARAMETERS`.
    pub command_includes: bool,
    pub include_ctx: IncludeContext,
}

impl Default for LoadConfigOptions {
    fn default() -> Self {
        Self {
            include_system: true,
            process_includes: true,
            command_includes: true,
            include_ctx: IncludeContext::default(),
        }
    }
}

// ── Canonical key helpers ────────────────────────────────────────────

/// Normalise a config key to canonical form.
///
/// - Section name is lowercased.
/// - Variable name (last dot-separated component) is lowercased.
/// - Subsection (middle components) preserves original case.
///
/// Returns `Err` if the key has fewer than two dot-separated parts.
///
/// # Examples
///
/// - `core.bare` → `core.bare`
/// - `Section.SubSection.Key` → `section.SubSection.key`
/// - `CORE.BARE` → `core.bare`
pub fn canonical_key(raw: &str) -> Result<String> {
    // Reject keys containing newlines
    if raw.contains('\n') || raw.contains('\r') {
        return Err(Error::ConfigError(format!(
            "invalid key: '{}'",
            raw.replace('\n', "\\n")
        )));
    }

    let first_dot = raw
        .find('.')
        .ok_or_else(|| Error::ConfigError(format!("key does not contain a section: '{raw}'")))?;
    let last_dot = raw
        .rfind('.')
        .ok_or_else(|| Error::ConfigError(format!("key does not contain a section: '{raw}'")))?;

    if last_dot == raw.len() - 1 {
        return Err(Error::ConfigError(format!(
            "key does not contain variable name: '{raw}'"
        )));
    }

    let section = &raw[..first_dot];
    let name = &raw[last_dot + 1..];

    // Validate section name: must be alphanumeric or hyphen
    if section.is_empty() || !section.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(Error::ConfigError(format!(
            "invalid key (bad section): '{raw}'"
        )));
    }

    // Validate variable name: must start with alpha, rest alphanumeric or hyphen
    if !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        || !name.chars().all(|c| c.is_alphanumeric() || c == '-')
    {
        return Err(Error::ConfigError(format!(
            "invalid key (bad variable name): '{raw}'"
        )));
    }

    if first_dot == last_dot {
        // No subsection: section.name
        Ok(format!(
            "{}.{}",
            section.to_lowercase(),
            name.to_lowercase()
        ))
    } else {
        // section.subsection.name
        let subsection = &raw[first_dot + 1..last_dot];
        Ok(format!(
            "{}.{}.{}",
            section.to_lowercase(),
            subsection,
            name.to_lowercase()
        ))
    }
}

// ── Parser ──────────────────────────────────────────────────────────

/// Display path for config diagnostics (matches [`config_error_path_display`] for public callers).
#[must_use]
pub fn config_file_display_for_error(path: &Path) -> String {
    config_error_path_display(path)
}

fn config_error_path_display(path: &Path) -> String {
    if path == Path::new("-") {
        return "standard input".to_owned();
    }
    if path.file_name().and_then(|s| s.to_str()) == Some("config")
        && path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some(".git")
    {
        return ".git/config".to_owned();
    }
    path.display().to_string()
}

/// State tracked while parsing a config file line-by-line.
struct Parser {
    section: String,
    subsection: Option<String>,
}

impl Parser {
    fn new() -> Self {
        Self {
            section: String::new(),
            subsection: None,
        }
    }

    /// Build the canonical key for a variable name in the current section.
    fn make_key(&self, name: &str) -> String {
        let sec = self.section.to_lowercase();
        let var = name.to_lowercase();
        match &self.subsection {
            Some(sub) => format!("{sec}.{sub}.{var}"),
            None => format!("{sec}.{var}"),
        }
    }

    /// Parse a section header line like `[section]` or `[section "subsection"]`.
    ///
    /// Returns `true` if the line was a section header.
    /// If there is content after `]` (an inline key=value), it is returned
    /// via the `inline_remainder` parameter.
    fn try_parse_section_with_remainder<'a>(
        &mut self,
        line: &'a str,
        inline_remainder: &mut Option<&'a str>,
    ) -> bool {
        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            return false;
        }
        // Find the closing `]` — but for subsection headers like
        // [section "sub\"escaped"], we need to skip escaped chars
        // inside quotes.
        let end = {
            let bytes = trimmed.as_bytes();
            let mut i = 1; // skip opening '['
            let mut in_quotes = false;
            let mut found = None;
            while i < bytes.len() {
                if in_quotes {
                    if bytes[i] == b'\\' {
                        i += 2; // skip escaped char
                        continue;
                    }
                    if bytes[i] == b'"' {
                        in_quotes = false;
                    }
                } else {
                    if bytes[i] == b'"' {
                        in_quotes = true;
                    }
                    if bytes[i] == b']' {
                        found = Some(i);
                        break;
                    }
                }
                i += 1;
            }
            match found {
                Some(i) => i,
                None => return false,
            }
        };
        let inside = &trimmed[1..end];
        // Check for subsection: [section "subsection"]
        if let Some(quote_start) = inside.find('"') {
            self.section = inside[..quote_start].trim().to_owned();
            let rest = &inside[quote_start + 1..];
            // Find unescaped closing quote
            let mut sub = String::new();
            let mut chars = rest.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    if let Some(escaped) = chars.next() {
                        sub.push(escaped);
                    }
                } else if ch == '"' {
                    break;
                } else {
                    sub.push(ch);
                }
            }
            self.subsection = Some(sub);
        } else {
            self.section = inside.trim().to_owned();
            self.subsection = None;
        }
        // Check for inline content after the closing `]`
        let after = trimmed[end + 1..].trim();
        if !after.is_empty() && !after.starts_with('#') && !after.starts_with(';') {
            *inline_remainder = Some(after);
        } else {
            *inline_remainder = None;
        }
        true
    }

    /// Parse a section header line (without inline remainder tracking).
    fn try_parse_section(&mut self, line: &str) -> bool {
        let mut _remainder = None;
        self.try_parse_section_with_remainder(line, &mut _remainder)
    }

    /// Parse a `key = value` or bare `key` line.
    ///
    /// Returns `Some((canonical_key, value))` if this is a variable line.
    fn try_parse_entry(&self, line: &str) -> Option<(String, Option<String>)> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            return None;
        }
        if trimmed.starts_with('[') {
            return None;
        }
        if self.section.is_empty() {
            return None;
        }

        if let Some(eq_pos) = trimmed.find('=') {
            let raw_name = trimmed[..eq_pos].trim();
            let raw_value = trimmed[eq_pos + 1..].trim();
            // Strip inline comment (not inside quotes)
            let value = strip_inline_comment(raw_value);
            let value = unescape_value(&value);
            let key = self.make_key(raw_name);
            Some((key, Some(value)))
        } else {
            // Bare key (boolean true)
            let raw_name = strip_inline_comment(trimmed);
            if raw_name.split_whitespace().count() > 1 {
                return None;
            }
            let key = self.make_key(raw_name.trim());
            Some((key, None))
        }
    }
}

/// Check if a value line ends with a continuation backslash.
///
/// This checks the value portion (after `=`) for a trailing `\` that is
/// outside quotes and outside an inline comment. If the `\` is after
/// a `#` or `;` that starts a comment, it does NOT count as continuation.
/// True when the value portion (after the first `=`) ends inside an unclosed double-quoted span.
///
/// Mirrors Git config continuation rules: a line ending with an open `"` continues on the next
/// physical line. Outside quotes, `#` / `;` start comments and the line is complete.
fn entry_line_value_has_unclosed_quote(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(eq_pos) = trimmed.find('=') else {
        return false;
    };
    let raw_value = trimmed[eq_pos + 1..].trim_start();
    let mut in_quote = false;
    let mut last_was_backslash = false;
    for ch in raw_value.chars() {
        match ch {
            '"' if !last_was_backslash => {
                in_quote = !in_quote;
                last_was_backslash = false;
            }
            '\\' if in_quote && !last_was_backslash => {
                last_was_backslash = true;
                continue;
            }
            '#' | ';' if !in_quote && !last_was_backslash => return false,
            _ => {
                last_was_backslash = false;
            }
        }
    }
    in_quote
}

fn value_line_continues(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
        return false;
    }
    // Find the value portion (after '=')
    // If no '=', this is a bare key — no continuation
    let value_part = match trimmed.find('=') {
        Some(pos) => &trimmed[pos + 1..],
        None => return false,
    };
    // Walk the value portion tracking quotes and comments
    let mut in_quote = false;
    let mut last_was_backslash = false;
    let mut in_comment = false;
    for ch in value_part.chars() {
        if in_comment {
            // Inside comment, backslash doesn't matter
            last_was_backslash = false;
            continue;
        }
        match ch {
            '"' if !last_was_backslash => {
                in_quote = !in_quote;
                last_was_backslash = false;
            }
            '\\' if !last_was_backslash => {
                last_was_backslash = true;
                continue;
            }
            '#' | ';' if !in_quote && !last_was_backslash => {
                in_comment = true;
                last_was_backslash = false;
            }
            _ => {
                last_was_backslash = false;
            }
        }
    }
    // The line continues if it ends with an unescaped backslash outside comments
    last_was_backslash && !in_comment
}

/// Strip an inline comment (`#` or `;`) that is not inside quotes.
fn strip_inline_comment(s: &str) -> String {
    let mut in_quote = false;
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                result.push(ch);
            }
            '\\' if in_quote => {
                result.push(ch);
                if let Some(&next) = chars.peek() {
                    result.push(next);
                    chars.next();
                }
            }
            '#' | ';' if !in_quote => break,
            _ => result.push(ch),
        }
    }
    // Trim trailing whitespace that was before the comment
    let trimmed = result.trim_end();
    trimmed.to_owned()
}

/// Unescape a config value: handle `\"`, `\\`, `\n`, `\t`, and strip
/// surrounding quotes.
fn unescape_value(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => { /* strip quotes */ }
            '\\' => match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            },
            _ => result.push(ch),
        }
    }
    result
}

/// Escape a config value for writing back to a file.
///
/// Wraps in double quotes if the value contains leading/trailing whitespace,
/// internal quotes, backslashes, or special characters.
/// Escape a subsection name for writing in a config section header.
/// In subsection names, `"` and `\` must be escaped.
fn escape_subsection(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

fn escape_value(s: &str) -> String {
    // Quote leading `-` so values are not mistaken for config options (Git does this for
    // submodule paths like `-sub` in `.gitmodules`), but leave signed numeric values bare.
    let leading_dash_needs_quoting = s.starts_with('-') && parse_i64(s).is_err();
    let needs_quoting = leading_dash_needs_quoting
        || s.starts_with(' ')
        || s.starts_with('\t')
        || s.ends_with(' ')
        || s.ends_with('\t')
        || s.contains('"')
        || s.contains('\\')
        || s.contains('\n')
        || s.contains('\r')
        || s.contains('#')
        || s.contains(';');

    if !needs_quoting {
        return s.to_owned();
    }

    let mut out = String::with_capacity(s.len() + 4);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Format a comment suffix for appending to a config value line.
///
/// Git's `--comment` flag normalises the comment:
/// - If the comment already starts with `#` (possibly preceded by whitespace/tab),
///   it is used as-is.
/// - Otherwise, ` # ` is prepended.
fn format_comment_suffix(comment: Option<&str>) -> String {
    match comment {
        None => String::new(),
        Some(c) => {
            if c.starts_with(' ') || c.starts_with('\t') {
                // Comment has its own leading whitespace separator
                c.to_owned()
            } else if c.starts_with('#') {
                // Comment starts with #, just prepend a space separator
                format!(" {c}")
            } else {
                // Plain text comment, prepend " # "
                format!(" # {c}")
            }
        }
    }
}

impl ConfigFile {
    /// Parse a config file from its raw text content.
    ///
    /// # Parameters
    ///
    /// - `path` — the file path (stored for diagnostics and round-trip writes).
    /// - `content` — the raw text of the file.
    /// - `scope` — the [`ConfigScope`] this file represents.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigError`] on malformed input.
    pub fn parse(path: &Path, content: &str, scope: ConfigScope) -> Result<Self> {
        let raw_lines: Vec<String> = content
            .lines()
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .map(String::from)
            .collect();
        let mut entries = Vec::new();
        let mut parser = Parser::new();

        let mut idx = 0;
        while idx < raw_lines.len() {
            let start_idx = idx;
            let line = &raw_lines[idx];
            idx += 1;

            // Pure comment lines don't continue even with trailing \
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }

            let mut inline_remainder = None;
            if parser.try_parse_section_with_remainder(line, &mut inline_remainder) {
                // Check if there's an inline key=value after the section header
                if let Some(remainder) = inline_remainder {
                    if let Some((key, value)) = parser.try_parse_entry(remainder) {
                        if key == "fetch.negotiationalgorithm" && value.is_none() {
                            let file_disp = config_error_path_display(path);
                            return Err(Error::Message(format!(
                                "error: missing value for 'fetch.negotiationalgorithm'\n\
fatal: bad config variable 'fetch.negotiationalgorithm' in file '{file_disp}' at line {}",
                                start_idx + 1
                            )));
                        }
                        entries.push(ConfigEntry {
                            key,
                            value,
                            scope,
                            file: Some(path.to_path_buf()),
                            line: start_idx + 1,
                        });
                    }
                }
                continue;
            }

            // For entry lines, we need to check continuation.
            // Build a logical line by joining continuations.
            let mut logical_line = line.clone();
            while value_line_continues(&logical_line) && idx < raw_lines.len() {
                // Remove the trailing backslash
                let t = logical_line.trim_end();
                logical_line = t[..t.len() - 1].to_string();
                // Append next line (trimmed of leading whitespace)
                let next = raw_lines[idx].trim_start();
                logical_line.push_str(next);
                idx += 1;
            }

            while entry_line_value_has_unclosed_quote(&logical_line) && idx < raw_lines.len() {
                let next = raw_lines[idx].trim_start();
                logical_line.push_str(next);
                idx += 1;
            }
            if entry_line_value_has_unclosed_quote(&logical_line) {
                let file_disp = config_error_path_display(path);
                return Err(Error::ConfigError(format!(
                    "bad config line {} in file '{file_disp}'",
                    start_idx + 1
                )));
            }

            if let Some((key, value)) = parser.try_parse_entry(&logical_line) {
                if key == "fetch.negotiationalgorithm" && value.is_none() {
                    let file_disp = config_error_path_display(path);
                    return Err(Error::Message(format!(
                        "error: missing value for 'fetch.negotiationalgorithm'\n\
fatal: bad config variable 'fetch.negotiationalgorithm' in file '{file_disp}' at line {}",
                        start_idx + 1
                    )));
                }
                entries.push(ConfigEntry {
                    key,
                    value,
                    scope,
                    file: Some(path.to_path_buf()),
                    line: start_idx + 1,
                });
            } else if logical_line.trim().is_empty() {
                continue;
            } else {
                let file_disp = config_error_path_display(path);
                let location = if path == Path::new("-") {
                    file_disp
                } else {
                    format!("file {file_disp}")
                };
                return Err(Error::Message(format!(
                    "fatal: bad config line {} in {location}",
                    start_idx + 1
                )));
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            scope,
            entries,
            raw_lines,
            include_origin: ConfigIncludeOrigin::Disk,
        })
    }

    /// Like [`Self::parse`] for `.gitmodules`, but on an unclosed-quote / bad line returns entries
    /// parsed **before** that line plus the one-based line number of the bad logical line.
    ///
    /// Git streams config and still applies entries from valid preceding lines; submodule-config
    /// tests rely on that when a later `.gitmodules` line is malformed.
    pub fn parse_gitmodules_best_effort(
        path: &Path,
        content: &str,
        scope: ConfigScope,
    ) -> (Vec<ConfigEntry>, Option<usize>) {
        let raw_lines: Vec<String> = content
            .lines()
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .map(String::from)
            .collect();
        let mut entries = Vec::new();
        let mut parser = Parser::new();

        let mut idx = 0;
        while idx < raw_lines.len() {
            let start_idx = idx;
            let line = &raw_lines[idx];
            idx += 1;

            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }

            let mut inline_remainder = None;
            if parser.try_parse_section_with_remainder(line, &mut inline_remainder) {
                if let Some(remainder) = inline_remainder {
                    if let Some((key, value)) = parser.try_parse_entry(remainder) {
                        entries.push(ConfigEntry {
                            key,
                            value,
                            scope,
                            file: Some(path.to_path_buf()),
                            line: start_idx + 1,
                        });
                    }
                }
                continue;
            }

            let mut logical_line = line.clone();
            while value_line_continues(&logical_line) && idx < raw_lines.len() {
                let t = logical_line.trim_end();
                logical_line = t[..t.len() - 1].to_string();
                let next = raw_lines[idx].trim_start();
                logical_line.push_str(next);
                idx += 1;
            }

            while entry_line_value_has_unclosed_quote(&logical_line) && idx < raw_lines.len() {
                let next = raw_lines[idx].trim_start();
                logical_line.push_str(next);
                idx += 1;
            }
            if entry_line_value_has_unclosed_quote(&logical_line) {
                return (entries, Some(start_idx + 1));
            }

            if let Some((key, value)) = parser.try_parse_entry(&logical_line) {
                entries.push(ConfigEntry {
                    key,
                    value,
                    scope,
                    file: Some(path.to_path_buf()),
                    line: start_idx + 1,
                });
            }
        }

        (entries, None)
    }

    /// Last value for `key` in this file only (canonical key, case-insensitive section/var like Git).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        let canon = canonical_key(key).ok()?;
        self.entries
            .iter()
            .rev()
            .find(|e| e.key == canon)
            .map(|e| e.value.clone().unwrap_or_else(|| "true".to_owned()))
    }

    /// Parse like [`Self::parse`] but record a non-disk include origin (blob, stdin, command line).
    pub fn parse_with_origin(
        path: &Path,
        content: &str,
        scope: ConfigScope,
        include_origin: ConfigIncludeOrigin,
    ) -> Result<Self> {
        let mut f = Self::parse(path, content, scope)?;
        f.include_origin = include_origin;
        Ok(f)
    }

    /// Build a synthetic [`ConfigFile`] from `GIT_CONFIG_PARAMETERS` / `git -c` payloads.
    ///
    /// Unlike [`Self::parse`], this accepts flat `key=value` assignments without `[section]`
    /// headers, matching how Git injects command-line configuration.
    pub fn from_git_config_parameters(path: &Path, raw: &str) -> Result<Self> {
        let mut entries = Vec::new();
        let pseudo_path = path.to_path_buf();
        for entry in parse_config_parameters_strict(raw)? {
            match entry {
                ConfigParameter::Pair { key, value } => {
                    let canon = canonical_key(key.trim())?;
                    entries.push(ConfigEntry {
                        key: canon,
                        value,
                        scope: ConfigScope::Command,
                        file: Some(pseudo_path.clone()),
                        line: 0,
                    });
                }
                ConfigParameter::OldStyle(entry) => {
                    if let Some((key, val)) = entry.split_once('=') {
                        let canon = canonical_key(key.trim())?;
                        entries.push(ConfigEntry {
                            key: canon,
                            value: Some(val.to_owned()),
                            scope: ConfigScope::Command,
                            file: Some(pseudo_path.clone()),
                            line: 0,
                        });
                    } else {
                        let canon = canonical_key(entry.trim())?;
                        entries.push(ConfigEntry {
                            key: canon,
                            value: None,
                            scope: ConfigScope::Command,
                            file: Some(pseudo_path.clone()),
                            line: 0,
                        });
                    }
                }
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            scope: ConfigScope::Command,
            entries,
            raw_lines: Vec::new(),
            include_origin: ConfigIncludeOrigin::CommandLine,
        })
    }

    /// Read and parse a config file from disk.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on read failure (other than not-found) or
    /// [`Error::ConfigError`] on parse failure.
    pub fn from_path(path: &Path, scope: ConfigScope) -> Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(content) => Ok(Some(Self::parse(path, &content, scope)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Set a value in this config file, creating the section if needed.
    ///
    /// If the key already exists, its last occurrence is updated in-place.
    /// Otherwise a new entry is appended (creating the section header if
    /// necessary).
    ///
    /// # Parameters
    ///
    /// - `key` — canonical key (e.g. `core.bare`).
    /// - `value` — the value to set.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        self.set_with_comment(key, value, None)
    }

    /// Set a value in this config file, optionally appending an inline comment.
    pub fn set_with_comment(
        &mut self,
        key: &str,
        value: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let canon = canonical_key(key)?;
        let raw_var = raw_variable_name(key);
        let comment_suffix = format_comment_suffix(comment);

        // Find the last entry with this key to replace in-place.
        let existing_idx = self.entries.iter().rposition(|e| e.key == canon);

        if let Some(idx) = existing_idx {
            let line_idx = self.entries[idx].line - 1;
            let raw_line = &self.raw_lines[line_idx];
            if is_section_header_with_inline_entry(raw_line) {
                // Entry is on the same line as a section header — split it
                let header_only = extract_section_header(raw_line);
                self.raw_lines[line_idx] = header_only;
                let new_line = format!("\t{} = {}{}", raw_var, escape_value(value), comment_suffix);
                self.raw_lines.insert(line_idx + 1, new_line);
                // Re-parse to fix up entries and line numbers
                let content = self.raw_lines.join("\n");
                let reparsed = Self::parse(&self.path, &content, self.scope)?;
                self.entries = reparsed.entries;
                self.raw_lines = reparsed.raw_lines;
            } else {
                self.raw_lines[line_idx] =
                    format!("\t{} = {}{}", raw_var, escape_value(value), comment_suffix);
                self.entries[idx].value = Some(value.to_owned());
            }
        } else {
            // Need to add: find or create the section
            let (section, subsection, _var) = split_key(&canon)?;
            let (raw_sec, raw_sub) = raw_section_parts(key);
            let section_line = self.find_or_create_section_preserving_case(
                &section,
                subsection.as_deref(),
                &raw_sec,
                raw_sub.as_deref(),
            );
            let new_line = format!("\t{} = {}{}", raw_var, escape_value(value), comment_suffix);

            // Insert after the section header (or last entry in section)
            let insert_at = self.last_line_in_section(section_line) + 1;
            self.raw_lines.insert(insert_at, new_line);

            // Re-parse to fix up line numbers
            let content = self.raw_lines.join("\n");
            let reparsed = Self::parse(&self.path, &content, self.scope)?;
            self.entries = reparsed.entries;
            self.raw_lines = reparsed.raw_lines;
        }

        Ok(())
    }

    /// Replace ALL occurrences of a key with a new value.
    ///
    /// Removes all but the last occurrence from the file, then updates
    /// the last occurrence with the new value (matching Git behaviour).
    pub fn replace_all(
        &mut self,
        key: &str,
        value: &str,
        value_pattern: Option<&str>,
    ) -> Result<()> {
        self.replace_all_with_comment(key, value, value_pattern, None)
    }

    /// Replace all occurrences, optionally appending an inline comment.
    ///
    /// Value patterns starting with `!` are treated as negated regex
    /// (matching values that do NOT match the pattern).
    pub fn replace_all_with_comment(
        &mut self,
        key: &str,
        value: &str,
        value_pattern: Option<&str>,
        comment: Option<&str>,
    ) -> Result<()> {
        let canon = canonical_key(key)?;
        let comment_suffix = format_comment_suffix(comment);

        // Parse optional regex pattern, handling `!` negation
        let (re, negated) = match value_pattern {
            Some(pat) => {
                let (neg, actual_pat) = if let Some(rest) = pat.strip_prefix('!') {
                    (true, rest)
                } else {
                    (false, pat)
                };
                let compiled = regex::Regex::new(actual_pat)
                    .map_err(|e| Error::ConfigError(format!("invalid value-pattern regex: {e}")))?;
                (Some(compiled), neg)
            }
            None => (None, false),
        };

        // Find all matching entries (by key, and optionally by value pattern)
        let matching_indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if e.key != canon {
                    return false;
                }
                if let Some(ref re) = re {
                    let v = e.value.as_deref().unwrap_or("");
                    let matched = re.is_match(v);
                    if negated {
                        !matched
                    } else {
                        matched
                    }
                } else {
                    true
                }
            })
            .map(|(i, _)| i)
            .collect();

        if matching_indices.is_empty() {
            // No matching entries — add a new one at the end of the section
            return self.add_value_with_comment(key, value, comment);
        }

        let raw_var = raw_variable_name(key);

        let target_idx = if value_pattern.is_some() {
            matching_indices[0]
        } else {
            *matching_indices
                .last()
                .ok_or_else(|| Error::ConfigError("missing config match".to_owned()))?
        };
        let target_line_idx = self.entries[target_idx].line - 1;
        let raw_line = &self.raw_lines[target_line_idx];
        if is_section_header_with_inline_entry(raw_line) {
            let header = extract_section_header(raw_line);
            self.raw_lines[target_line_idx] = header;
            let new_line = format!("\t{} = {}{}", raw_var, escape_value(value), comment_suffix);
            self.raw_lines.insert(target_line_idx + 1, new_line);
        } else {
            self.raw_lines[target_line_idx] =
                format!("\t{} = {}{}", raw_var, escape_value(value), comment_suffix);
        }

        for &idx in matching_indices.iter().rev() {
            if idx == target_idx {
                continue;
            }
            let line_idx = self.entries[idx].line - 1;
            self.remove_entry_line(line_idx);
        }

        // Re-parse
        let content = self.raw_lines.join("\n");
        let reparsed = Self::parse(&self.path, &content, self.scope)?;
        self.entries = reparsed.entries;
        self.raw_lines = reparsed.raw_lines;

        Ok(())
    }

    /// Count how many entries exist for a key.
    pub fn count(&self, key: &str) -> Result<usize> {
        let canon = canonical_key(key)?;
        Ok(self.entries.iter().filter(|e| e.key == canon).count())
    }

    /// Remove an entry at the given raw line index.
    ///
    /// If the line is a section header with an inline entry, only the inline
    /// portion is removed (the header is kept). Otherwise the entire line is
    /// removed. Also removes continuation lines following the entry.
    /// Remove an entry at the given raw line index.
    ///
    /// If the line is a section header with an inline entry, only the inline
    /// portion is removed (the header is kept). Otherwise the entire line
    /// (and any continuation lines) is removed.
    fn remove_entry_line(&mut self, line_idx: usize) {
        if is_section_header_with_inline_entry(&self.raw_lines[line_idx]) {
            // Keep the section header, strip the inline entry
            let header = extract_section_header(&self.raw_lines[line_idx]);
            self.raw_lines[line_idx] = header;
        } else {
            // Check if this line has continuation lines and remove them too
            let mut lines_to_remove = 1;
            let mut check_line = self.raw_lines[line_idx].clone();
            while value_line_continues(&check_line)
                && (line_idx + lines_to_remove) < self.raw_lines.len()
            {
                check_line = self.raw_lines[line_idx + lines_to_remove].clone();
                lines_to_remove += 1;
            }
            for _ in 0..lines_to_remove {
                self.raw_lines.remove(line_idx);
            }
        }
    }

    /// Unset (remove) only the last occurrence of a key.
    ///
    /// Returns the number of entries removed (0 or 1).
    pub fn unset_last(&mut self, key: &str) -> Result<usize> {
        let canon = canonical_key(key)?;
        let last_idx = self.entries.iter().rposition(|e| e.key == canon);

        if let Some(idx) = last_idx {
            let line_idx = self.entries[idx].line - 1;
            self.remove_entry_line(line_idx);
            let content = self.raw_lines.join("\n");
            let reparsed = Self::parse(&self.path, &content, self.scope)?;
            self.entries = reparsed.entries;
            self.raw_lines = reparsed.raw_lines;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    /// Unset (remove) all occurrences of a key.
    ///
    /// # Parameters
    ///
    /// - `key` — canonical key (e.g. `core.bare`).
    ///
    /// # Returns
    ///
    /// The number of entries removed.
    pub fn unset(&mut self, key: &str) -> Result<usize> {
        let canon = canonical_key(key)?;
        let line_indices: Vec<usize> = self
            .entries
            .iter()
            .filter(|e| e.key == canon)
            .map(|e| e.line - 1)
            .collect();

        let count = line_indices.len();
        // Remove from bottom to top to keep indices valid
        for &idx in line_indices.iter().rev() {
            self.remove_entry_line(idx);
        }

        if count > 0 {
            let content = self.raw_lines.join("\n");
            let reparsed = Self::parse(&self.path, &content, self.scope)?;
            self.entries = reparsed.entries;
            self.raw_lines = reparsed.raw_lines;
        }

        Ok(count)
    }

    /// Unset entries matching a key and optional value-pattern regex.
    ///
    /// If `value_pattern` is `None`, removes all entries with the given key.
    /// If `value_pattern` is `Some(pat)`, only removes entries whose value matches the regex.
    ///
    /// When `preserve_empty_section_header` is `true`, a section header is kept even if the
    /// section has no remaining keys (Git's `config unset --all`). When `false`, empty sections
    /// are stripped (`config --unset`, `config --unset-all`, and value-pattern unsets).
    pub fn unset_matching(
        &mut self,
        key: &str,
        value_pattern: Option<&str>,
        preserve_empty_section_header: bool,
    ) -> Result<usize> {
        let canon = canonical_key(key)?;
        let re = match value_pattern {
            Some(pat) => Some(
                regex::Regex::new(pat)
                    .map_err(|e| Error::ConfigError(format!("invalid value-pattern regex: {e}")))?,
            ),
            None => None,
        };

        let line_indices: Vec<usize> = self
            .entries
            .iter()
            .filter(|e| {
                if e.key != canon {
                    return false;
                }
                if let Some(ref re) = re {
                    let v = e.value.as_deref().unwrap_or("");
                    re.is_match(v)
                } else {
                    true
                }
            })
            .map(|e| e.line - 1)
            .collect();

        let count = line_indices.len();
        for &idx in line_indices.iter().rev() {
            self.remove_entry_line(idx);
        }

        if count > 0 {
            if !preserve_empty_section_header {
                let (section, subsection, _) = split_key(&canon)?;
                self.remove_empty_section_headers_matching(&section, subsection.as_deref());
            }

            let content = self.raw_lines.join("\n");
            let reparsed = Self::parse(&self.path, &content, self.scope)?;
            self.entries = reparsed.entries;
            self.raw_lines = reparsed.raw_lines;
        }

        Ok(count)
    }

    /// Remove an entire section (and all its entries).
    ///
    /// # Parameters
    ///
    /// - `section` — section name (e.g. `"core"`, `"remote.origin"`).
    pub fn remove_section(&mut self, section: &str) -> Result<bool> {
        let (sec_name, sub_name) = parse_section_name(section);
        let sec_lower = sec_name.to_lowercase();

        let mut remove = vec![false; self.raw_lines.len()];
        let mut removing = false;
        let mut found = false;
        let mut parser = Parser::new();

        for (idx, line) in self.raw_lines.iter().enumerate() {
            if parser.try_parse_section(line) {
                removing = section_matches(&parser, &sec_lower, sub_name);
                found |= removing;
            }
            if removing {
                remove[idx] = true;
            }
        }

        if found {
            self.raw_lines = self
                .raw_lines
                .iter()
                .enumerate()
                .filter_map(|(idx, line)| (!remove[idx]).then_some(line.clone()))
                .collect();
            let content = self.raw_lines.join("\n");
            let reparsed = Self::parse(&self.path, &content, self.scope)?;
            self.entries = reparsed.entries;
            self.raw_lines = reparsed.raw_lines;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Rename a section.
    ///
    /// # Parameters
    ///
    /// - `old_name` — current section name (e.g. `"branch.main"`).
    /// - `new_name` — new section name (e.g. `"branch.develop"`).
    pub fn rename_section(&mut self, old_name: &str, new_name: &str) -> Result<bool> {
        let (old_sec, old_sub) = parse_section_name(old_name);
        let (new_sec, new_sub) = parse_section_name(new_name);
        validate_section_name(new_sec, new_sub)?;
        let old_lower = old_sec.to_lowercase();

        let mut found = false;
        let mut parser = Parser::new();

        let mut idx = 0usize;
        while idx < self.raw_lines.len() {
            let line = self.raw_lines[idx].clone();
            let mut inline_remainder = None;
            if parser.try_parse_section_with_remainder(&line, &mut inline_remainder)
                && section_matches(&parser, &old_lower, old_sub)
            {
                // Rewrite the section header
                let header = match new_sub {
                    Some(sub) => format!("[{} \"{}\"]", new_sec, sub),
                    None => format!("[{}]", new_sec),
                };
                self.raw_lines[idx] = header;
                if let Some(remainder) = inline_remainder {
                    self.raw_lines
                        .insert(idx + 1, format!("\t{}", remainder.trim()));
                    idx += 1;
                }
                found = true;
            }
            idx += 1;
        }

        if found {
            let content = self.raw_lines.join("\n");
            let reparsed = Self::parse(&self.path, &content, self.scope)?;
            self.entries = reparsed.entries;
            self.raw_lines = reparsed.raw_lines;
        }

        Ok(found)
    }

    /// Append a new value for a key without removing existing entries.
    ///
    /// This is the behaviour of `git config --add section.key value`.
    /// If the section doesn't exist, it is created.
    pub fn add_value(&mut self, key: &str, value: &str) -> Result<()> {
        self.add_value_with_comment(key, value, None)
    }

    /// Append a new value with an optional inline comment.
    pub fn add_value_with_comment(
        &mut self,
        key: &str,
        value: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let canon = canonical_key(key)?;
        let raw_var = raw_variable_name(key);
        let comment_suffix = format_comment_suffix(comment);
        let (section, subsection, _var) = split_key(&canon)?;
        let (raw_sec, raw_sub) = raw_section_parts(key);

        let section_line = self.find_or_create_section_preserving_case(
            &section,
            subsection.as_deref(),
            &raw_sec,
            raw_sub.as_deref(),
        );
        let new_line = format!("\t{} = {}{}", raw_var, escape_value(value), comment_suffix);
        let insert_at = self.last_line_in_section(section_line) + 1;
        self.raw_lines.insert(insert_at, new_line);

        // Re-parse to fix up entries and line numbers
        let content = self.raw_lines.join("\n");
        let reparsed = Self::parse(&self.path, &content, self.scope)?;
        self.entries = reparsed.entries;
        self.raw_lines = reparsed.raw_lines;

        Ok(())
    }

    /// Write the (possibly modified) config back to disk.
    /// Remove section headers that have no remaining entries or comments.
    fn remove_empty_section_headers_matching(&mut self, section: &str, subsection: Option<&str>) {
        let (Ok(section_re), Ok(comment_re)) = (
            regex::Regex::new(r"^\s*\["),
            regex::Regex::new(r"^\s*(#|;)"),
        ) else {
            // Static patterns: compilation cannot fail in practice; bail out safely.
            return;
        };

        let mut to_remove: Vec<usize> = Vec::new();
        let len = self.raw_lines.len();
        let section_lower = section.to_lowercase();
        let mut parser = Parser::new();

        for i in 0..len {
            let line = &self.raw_lines[i];
            if !section_re.is_match(line) {
                continue;
            }
            if !parser.try_parse_section(line)
                || !section_matches(&parser, &section_lower, subsection)
            {
                continue;
            }
            // Don't remove section headers that have inline key=value entries
            if is_section_header_with_inline_entry(line) {
                continue;
            }
            let has_attached_leading_comment = self.raw_lines[..i]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, line)| !line.trim().is_empty())
                .is_some_and(|(idx, line)| {
                    comment_re.is_match(line)
                        && idx
                            .checked_sub(1)
                            .is_none_or(|prev| !value_line_continues(&self.raw_lines[prev]))
                });
            if has_attached_leading_comment {
                continue;
            }
            // Check if this section header is followed only by blank lines,
            // comments, or another section header (or end of file).
            let mut has_entries = false;
            for j in (i + 1)..len {
                let next = self.raw_lines[j].trim();
                if next.is_empty() {
                    continue;
                }
                if section_re.is_match(&self.raw_lines[j]) {
                    break;
                }
                if comment_re.is_match(&self.raw_lines[j]) {
                    // Has comments — keep the section
                    has_entries = true;
                    break;
                }
                // Has a key-value entry
                has_entries = true;
                break;
            }
            if !has_entries {
                to_remove.push(i);
            }
        }

        // Remove in reverse to preserve indices
        for &idx in to_remove.iter().rev() {
            self.raw_lines.remove(idx);
        }

        // Also remove trailing blank lines
        while self.raw_lines.last().is_some_and(|l| l.trim().is_empty()) {
            self.raw_lines.pop();
        }
    }

    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on write failure.
    pub fn write(&self) -> Result<()> {
        let content = self.raw_lines.join("\n");
        let trimmed = content.trim();
        if trimmed.is_empty() {
            // Write empty file if no content
            fs::write(&self.path, "")?;
        } else {
            // Ensure trailing newline
            let content = if content.ends_with('\n') {
                content
            } else {
                format!("{content}\n")
            };
            fs::write(&self.path, content)?;
        }
        evict_config_cache_for_path(&self.path);
        Ok(())
    }

    /// Find the line index of a section header, or create one.
    #[allow(dead_code)]
    fn find_or_create_section(&mut self, section: &str, subsection: Option<&str>) -> usize {
        let sec_lower = section.to_lowercase();
        let mut parser = Parser::new();

        for (idx, line) in self.raw_lines.iter().enumerate() {
            if parser.try_parse_section(line) && section_matches(&parser, &sec_lower, subsection) {
                return idx;
            }
        }

        // Create new section at end of file
        let header = match subsection {
            Some(sub) => {
                let escaped = escape_subsection(sub);
                format!("[{} \"{}\"]", section, escaped)
            }
            None => format!("[{}]", section),
        };
        self.raw_lines.push(header);
        self.raw_lines.len() - 1
    }

    /// Find the line index of a section header (case-insensitive match),
    /// or create one using the original-case names from user input.
    fn find_or_create_section_preserving_case(
        &mut self,
        section: &str,
        subsection: Option<&str>,
        raw_section: &str,
        raw_subsection: Option<&str>,
    ) -> usize {
        let sec_lower = section.to_lowercase();
        let mut parser = Parser::new();

        for (idx, line) in self.raw_lines.iter().enumerate() {
            if parser.try_parse_section(line) && section_matches(&parser, &sec_lower, subsection) {
                return idx;
            }
        }

        // Create new section at end of file, using original case
        let header = match raw_subsection {
            Some(sub) => {
                let escaped = escape_subsection(sub);
                format!("[{} \"{}\"]", raw_section, escaped)
            }
            None => format!("[{}]", raw_section),
        };
        self.raw_lines.push(header);
        self.raw_lines.len() - 1
    }

    /// Find the last line that belongs to the section starting at `section_line`.
    fn last_line_in_section(&self, section_line: usize) -> usize {
        let mut last = section_line;
        for idx in (section_line + 1)..self.raw_lines.len() {
            let trimmed = self.raw_lines[idx].trim();
            if trimmed.starts_with('[') {
                break;
            }
            last = idx;
        }
        last
    }
}

// ── ConfigSet ───────────────────────────────────────────────────────

impl ConfigSet {
    /// Create an empty config set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// All merged entries in load order (for listing keys such as `alias.*`).
    #[must_use]
    pub fn entries(&self) -> &[ConfigEntry] {
        &self.entries
    }

    /// Merge entries from a [`ConfigFile`] into this set.
    ///
    /// Entries are appended; later values override earlier ones for
    /// single-value lookups.
    pub fn merge(&mut self, file: &ConfigFile) {
        self.entries.extend(file.entries.iter().cloned());
    }

    /// Merge another [`ConfigSet`] into this set (entries appended in order).
    pub fn merge_set(&mut self, other: &ConfigSet) {
        self.entries.extend(other.entries.iter().cloned());
    }

    /// Add a command-line override (`-c key=value`).
    pub fn add_command_override(&mut self, key: &str, value: &str) -> Result<()> {
        let canon = canonical_key(key)?;
        self.entries.push(ConfigEntry {
            key: canon,
            value: Some(value.to_owned()),
            scope: ConfigScope::Command,
            file: None,
            line: 0,
        });
        Ok(())
    }

    /// Get the last (highest-priority) value for a key.
    ///
    /// # Parameters
    ///
    /// - `key` — the key to look up (will be canonicalized).
    ///
    /// # Returns
    ///
    /// `Some(value)` for the last matching entry, or `None` if not found.
    /// Bare boolean keys return `Some("true")`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        let canon = canonical_key(key).ok()?;
        self.entries
            .iter()
            .rev()
            .find(|e| e.key == canon)
            .map(|e| e.value.clone().unwrap_or_else(|| "true".to_owned()))
    }

    /// Last (highest-priority) [`ConfigEntry`] for a key, including origin metadata.
    ///
    /// Bare boolean keys are returned with [`ConfigEntry::value`] set to `None` (same as `get`,
    /// which maps them to `"true"` for string lookups).
    #[must_use]
    pub fn get_last_entry(&self, key: &str) -> Option<ConfigEntry> {
        let canon = canonical_key(key).ok()?;
        self.entries.iter().rev().find(|e| e.key == canon).cloned()
    }

    /// Get all values for a key (multi-valued; in load order).
    #[must_use]
    pub fn get_all(&self, key: &str) -> Vec<String> {
        let canon = match canonical_key(key) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        self.entries
            .iter()
            .filter(|e| e.key == canon)
            .map(|e| e.value.clone().unwrap_or_default())
            .collect()
    }

    /// All raw values for a key in load order, preserving `None` for bare boolean keys.
    ///
    /// Matches Git's multi-value list where `NULL` means a value-less / boolean-true key.
    #[must_use]
    pub fn get_all_raw(&self, key: &str) -> Vec<Option<String>> {
        let canon = match canonical_key(key) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        self.entries
            .iter()
            .filter(|e| e.key == canon)
            .map(|e| e.value.clone())
            .collect()
    }

    /// True if any config entry uses `key` (after canonicalization), including bare boolean keys.
    ///
    /// Unlike [`Self::get`], this does not treat a missing value as `"true"` — it reports whether
    /// the key appears in the merged config at all (Git `repo_config_get` / `git_configset_get`).
    #[must_use]
    pub fn has_key(&self, key: &str) -> bool {
        let Ok(canon) = canonical_key(key) else {
            return false;
        };
        self.entries.iter().any(|e| e.key == canon)
    }

    /// Get a boolean value, interpreting `true`/`yes`/`on`/`1` as true and
    /// `false`/`no`/`off`/`0` as false.
    ///
    /// `pack.allowPackReuse` may be `single` or `multi` (Git enum, not a bool). Those values are
    /// treated as unset for boolean lookup so `get_bool` does not error during broad config scans.
    pub fn get_bool(&self, key: &str) -> Option<std::result::Result<bool, String>> {
        let v = self.get(key)?;
        if canonical_key(key).ok().as_deref() == Some("pack.allowpackreuse") {
            let lower = v.trim().to_ascii_lowercase();
            if lower == "single" || lower == "multi" {
                return None;
            }
        }
        Some(parse_bool(&v))
    }

    /// Whether pathnames in human-readable output should fully C-quote non-ASCII bytes as octal.
    ///
    /// Maps to Git's `quote_path_fully` (`core.quotepath`, default true). When false, UTF-8 and
    /// other high bytes are emitted literally; only ASCII specials are escaped. Also honors
    /// `core.quotePath` as an alternate spelling.
    #[must_use]
    pub fn quote_path_fully(&self) -> bool {
        let from_key = |key: &str| self.get_bool(key).and_then(|r| r.ok());
        from_key("core.quotepath")
            .or_else(|| from_key("core.quotePath"))
            .unwrap_or(true)
    }

    /// Default for `pack.writeReverseIndex` / `pack.writereverseindex` (Git default: true).
    ///
    /// Tests set `GIT_TEST_NO_WRITE_REV_INDEX` to force no `.rev` output.
    #[must_use]
    pub fn pack_write_reverse_index_default(&self) -> bool {
        if std::env::var("GIT_TEST_NO_WRITE_REV_INDEX")
            .ok()
            .as_deref()
            .is_some_and(|v| {
                let s = v.trim().to_ascii_lowercase();
                matches!(s.as_str(), "1" | "true" | "yes" | "on")
            })
        {
            return false;
        }
        if self
            .get("pack.writereverseindex")
            .or_else(|| self.get("pack.writeReverseIndex"))
            .is_some_and(|v| v.trim().is_empty())
        {
            return false;
        }
        self.get_bool("pack.writereverseindex")
            .or_else(|| self.get_bool("pack.writeReverseIndex"))
            .and_then(|r| r.ok())
            .unwrap_or(true)
    }

    /// Default for `pack.readReverseIndex` / `pack.readreverseindex` (Git default: true).
    #[must_use]
    pub fn pack_read_reverse_index_default(&self) -> bool {
        self.get_bool("pack.readreverseindex")
            .or_else(|| self.get_bool("pack.readReverseIndex"))
            .and_then(|r| r.ok())
            .unwrap_or(true)
    }

    /// Resolved `core.logAllRefUpdates` using this merged set (includes `git -c` / env), then Git's
    /// bare-repo default when the key is unset everywhere.
    #[must_use]
    pub fn effective_log_refs_config(&self, git_dir: &Path) -> refs::LogRefsConfig {
        if let Some(v) = self.get("core.logAllRefUpdates") {
            let lower = v.trim().to_ascii_lowercase();
            let parsed = match lower.as_str() {
                "always" => Some(refs::LogRefsConfig::Always),
                "1" | "true" | "yes" | "on" => Some(refs::LogRefsConfig::Normal),
                "0" | "false" | "no" | "off" | "never" => Some(refs::LogRefsConfig::None),
                _ => None,
            };
            if let Some(c) = parsed {
                return c;
            }
        }
        refs::effective_log_refs_config(git_dir)
    }

    /// Get an integer value, supporting Git's `k`/`m`/`g` suffixes.
    pub fn get_i64(&self, key: &str) -> Option<std::result::Result<i64, String>> {
        self.get(key).map(|v| parse_i64(&v))
    }

    /// Zlib deflate level for `git pack-objects` (Git's `pack_compression_level`).
    ///
    /// Entries are applied in [`Self::entries`] order. `core.compression` sets the pack level
    /// until a `pack.compression` appears (Git `pack_compression_seen`). `core.loosecompression`
    /// is ignored here — it only affects loose-object zlib, not packs.
    ///
    /// `-1` means zlib default (level 6). Valid values are `-1` or `0..=9`.
    pub fn pack_objects_zlib_level(&self) -> Result<i32> {
        const Z_DEFAULT_COMPRESSION: i32 = 6;
        const Z_BEST_COMPRESSION: i32 = 9;

        let parse_compression = |raw: &str| -> Result<i32> {
            let v = parse_git_config_int_strict(raw.trim()).map_err(|_| {
                Error::ConfigError(format!("bad numeric config value '{raw}' for compression"))
            })?;
            if v == -1 {
                return Ok(Z_DEFAULT_COMPRESSION);
            }
            if v < 0 || v > i64::from(Z_BEST_COMPRESSION) {
                return Err(Error::ConfigError(format!(
                    "bad zlib compression level {v}"
                )));
            }
            Ok(v as i32)
        };

        // `core.loosecompression` affects loose objects only (Git `zlib_compression_level`), not pack.
        let mut pack_level = Z_DEFAULT_COMPRESSION;
        let mut pack_compression_seen = false;

        for e in self.entries() {
            match e.key.as_str() {
                "core.compression" => {
                    let Some(val) = e.value.as_deref() else {
                        continue;
                    };
                    let level = parse_compression(val)?;
                    if !pack_compression_seen {
                        pack_level = level;
                    }
                }
                "pack.compression" => {
                    let Some(val) = e.value.as_deref() else {
                        continue;
                    };
                    pack_level = parse_compression(val)?;
                    pack_compression_seen = true;
                }
                _ => {}
            }
        }

        Ok(pack_level)
    }

    /// Zlib deflate level for loose objects (Git's `zlib_compression_level`).
    ///
    /// `core.looseCompression` wins; otherwise `core.compression` applies; when neither is set
    /// Git defaults loose objects to `Z_BEST_SPEED` (1), trading disk for write throughput.
    /// `-1` means zlib default (level 6). Valid values are `-1` or `0..=9`; out-of-range or
    /// non-numeric values fall back to the default rather than erroring (matching how the
    /// object writer must not fail on odd config).
    #[must_use]
    pub fn loose_objects_zlib_level(&self) -> u32 {
        const Z_BEST_SPEED: u32 = 1;
        const Z_DEFAULT_COMPRESSION: u32 = 6;
        let parse_level = |raw: &str| -> Option<u32> {
            let v = parse_git_config_int_strict(raw.trim()).ok()?;
            match v {
                -1 => Some(Z_DEFAULT_COMPRESSION),
                0..=9 => Some(v as u32),
                _ => None,
            }
        };
        if let Some(level) = self
            .get("core.looseCompression")
            .as_deref()
            .and_then(parse_level)
        {
            return level;
        }
        if let Some(level) = self
            .get("core.compression")
            .as_deref()
            .and_then(parse_level)
        {
            return level;
        }
        Z_BEST_SPEED
    }

    /// Get all entries matching a key pattern (regex).
    ///
    /// Used by `git config --get-regexp`. Returns an error if the pattern
    /// is not a valid regex.
    pub fn get_regexp(&self, pattern: &str) -> std::result::Result<Vec<&ConfigEntry>, String> {
        let re = regex::Regex::new(pattern).map_err(|e| format!("invalid key pattern: {e}"))?;
        Ok(self
            .entries
            .iter()
            .filter(|e| re.is_match(&e.key))
            .collect())
    }

    /// Load the standard Git configuration file cascade for a repository.
    ///
    /// # Parameters
    ///
    /// - `git_dir` — path to the `.git` directory (for local/worktree config).
    /// - `include_system` — whether to load system config.
    ///
    /// # Errors
    ///
    /// Returns errors from file I/O or parsing.
    pub fn load(git_dir: Option<&Path>, include_system: bool) -> Result<Self> {
        let mut opts = LoadConfigOptions::default();
        opts.include_system = include_system;
        opts.include_ctx.git_dir = git_dir.map(PathBuf::from);
        Self::load_with_options(git_dir, &opts)
    }

    /// Load the standard configuration cascade with explicit include and scope control.
    ///
    /// See [`LoadConfigOptions`] for `GIT_CONFIG_PARAMETERS` / `-c` include behaviour.
    ///
    /// Results are memoized for the process lifetime and revalidated against
    /// the cascade files' stat stamps on every call (see the cache notes near
    /// [`ConfigCacheKey`]).
    pub fn load_with_options(git_dir: Option<&Path>, opts: &LoadConfigOptions) -> Result<Self> {
        let Some(env_fp) = config_env_fingerprint() else {
            return Self::load_with_options_uncached(git_dir, opts, &mut Vec::new());
        };
        let key = ConfigCacheKey::new(git_dir, opts);
        let base_stamps = config_file_stamps(git_dir, opts);
        if let Some(cached) = config_cache_lookup(&key, &env_fp, &base_stamps) {
            return Ok(cached);
        }
        let mut included_files = Vec::new();
        let set = Self::load_with_options_uncached(git_dir, opts, &mut included_files)?;
        included_files.sort_unstable();
        included_files.dedup();
        let extra_stamps = stamp_paths(included_files);
        let mut cache = config_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            key,
            ConfigCacheEntry {
                env_fingerprint: env_fp,
                base_stamps,
                extra_stamps,
                set: Arc::new(set.clone()),
            },
        );
        Ok(set)
    }

    fn load_with_options_uncached(
        git_dir: Option<&Path>,
        opts: &LoadConfigOptions,
        included_files: &mut Vec<PathBuf>,
    ) -> Result<Self> {
        let mut set = Self::new();
        let proc = opts.process_includes;
        let ctx = opts.include_ctx.clone();

        // System config
        if opts.include_system && !git_config_nosystem_enabled() {
            let system_path = std::env::var("GIT_CONFIG_SYSTEM")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/etc/gitconfig"));
            match ConfigFile::from_path(&system_path, ConfigScope::System) {
                Ok(Some(f)) => {
                    Self::merge_with_includes_collect(&mut set, &f, proc, 0, &ctx, included_files)?
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }

        // Global config (Git merges every existing file: XDG then ~/.gitconfig).
        for path in global_config_paths() {
            match ConfigFile::from_path(&path, ConfigScope::Global) {
                Ok(Some(f)) => {
                    Self::merge_with_includes_collect(&mut set, &f, proc, 0, &ctx, included_files)?
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }

        // Local config — linked worktrees read `commondir/config`, not the admin `config`.
        if let Some(gd) = git_dir {
            let common_dir = crate::repo::common_git_dir_for_config(gd);
            let local_path = common_dir.join("config");
            match ConfigFile::from_path(&local_path, ConfigScope::Local) {
                Ok(Some(f)) => {
                    Self::merge_with_includes_collect(&mut set, &f, proc, 0, &ctx, included_files)?
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }

            // Worktree config — Git only reads `config.worktree` when
            // `extensions.worktreeConfig` is enabled in the common repository `config`.
            let wt_path = gd.join("config.worktree");
            if crate::repo::worktree_config_enabled(&common_dir) {
                match ConfigFile::from_path(&wt_path, ConfigScope::Worktree) {
                    Ok(Some(f)) => Self::merge_with_includes_collect(
                        &mut set,
                        &f,
                        proc,
                        0,
                        &ctx,
                        included_files,
                    )?,
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
            }
        }

        // Environment overrides: optional file
        if let Ok(path) = std::env::var("GIT_CONFIG") {
            match ConfigFile::from_path(Path::new(&path), ConfigScope::Command) {
                Ok(Some(f)) => {
                    if proc {
                        Self::merge_with_includes_collect(
                            &mut set,
                            &f,
                            proc,
                            0,
                            &ctx,
                            included_files,
                        )?;
                    } else {
                        set.merge(&f);
                    }
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }

        add_environment_config_pairs(&mut set)?;

        // GIT_CONFIG_PARAMETERS — used by `git -c key=value`.
        if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
            if proc && opts.command_includes && !params.trim().is_empty() {
                let pseudo = Path::new(":GIT_CONFIG_PARAMETERS");
                let cmd_file = ConfigFile::from_git_config_parameters(pseudo, &params)?;
                Self::merge_with_includes_collect(
                    &mut set,
                    &cmd_file,
                    proc,
                    0,
                    &ctx,
                    included_files,
                )?;
            } else if !params.trim().is_empty() {
                for entry in parse_config_parameters(&params) {
                    if let Some((key, val)) =
                        entry.split_once('\u{1}').or_else(|| entry.split_once('='))
                    {
                        let _ = set.add_command_override(key.trim(), val);
                    } else {
                        let _ = set.add_command_override(entry.trim(), "true");
                    }
                }
            }
        }

        Ok(set)
    }

    /// Read configuration the way Git's `read_early_config` / `do_git_config_sequence` does:
    /// system (unless disabled), global files in Git order, optional repository `config` /
    /// `config.worktree`, then `GIT_CONFIG_PARAMETERS`.
    ///
    /// When `git_dir` is `None` (no discovered repository, e.g. `GIT_CEILING_DIRECTORIES`), only
    /// non-repo layers are read — matching Git when discovery returns no gitdir (t1309 ceiling #2).
    ///
    /// Returns all values for `key` in load order (Git's `read_early_config` callback runs once per
    /// occurrence).
    ///
    /// This matches upstream ordering for `test-tool config read_early_config` (t1309, t1305).
    pub fn read_early_config(git_dir: Option<&Path>, key: &str) -> Result<Vec<String>> {
        let mut set = Self::new();
        let ctx = IncludeContext {
            git_dir: git_dir.map(PathBuf::from),
            command_line_relative_include_is_error: false,
        };

        // System
        if !git_config_nosystem_enabled() {
            let system_path = std::env::var("GIT_CONFIG_SYSTEM")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/etc/gitconfig"));
            if let Ok(Some(f)) = ConfigFile::from_path(&system_path, ConfigScope::System) {
                Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
            }
        }

        // Global: all existing candidates (Git merges every readable file).
        for path in global_config_paths() {
            if let Ok(Some(f)) = ConfigFile::from_path(&path, ConfigScope::Global) {
                Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
            }
        }

        if let Some(gd) = git_dir {
            let common_dir = crate::repo::common_git_dir_for_config(gd);
            // Local (commondir) — skip when format is newer than supported (t1309).
            let local_path = common_dir.join("config");
            if let Some(msg) = crate::repo::early_config_ignore_repo_reason(&common_dir) {
                eprintln!("warning: ignoring git dir '{}': {}", gd.display(), msg);
            } else if let Ok(Some(f)) = ConfigFile::from_path(&local_path, ConfigScope::Local) {
                set.merge_file_with_includes(&f, true, &ctx)?;
            }

            // Worktree-specific config (when enabled for this repo).
            let wt_path = gd.join("config.worktree");
            if crate::repo::worktree_config_enabled(&common_dir) {
                if let Ok(Some(f)) = ConfigFile::from_path(&wt_path, ConfigScope::Worktree) {
                    Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
                }
            }
        }

        // GIT_CONFIG_PARAMETERS — same as full load (`load_with_options` default).
        if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
            if !params.trim().is_empty() {
                let pseudo = Path::new(":GIT_CONFIG_PARAMETERS");
                let cmd_file = ConfigFile::from_git_config_parameters(pseudo, &params)?;
                Self::merge_with_includes(&mut set, &cmd_file, true, 0, &ctx)?;
            }
        }

        Ok(set.get_all(key))
    }

    /// Merge a single config file, optionally expanding `[include]` / `[includeIf]`.
    ///
    /// Used by `grit config -f` and scoped reads; [`ConfigSet::load_with_options`] uses the same
    /// internal routine for the standard cascade.
    pub fn merge_file_with_includes(
        &mut self,
        file: &ConfigFile,
        process_includes: bool,
        ctx: &IncludeContext,
    ) -> Result<()> {
        Self::merge_with_includes(self, file, process_includes, 0, ctx)
    }

    /// Load only the repository's own `config` file (plus any `[include]` targets).
    ///
    /// Unlike [`Self::load`], this ignores system/global config and environment
    /// overrides. Used for receive-side options (e.g. `transfer.fsckObjects`) so a
    /// pusher's global configuration cannot weaken the remote repository's policy.
    pub fn load_repo_local_only(git_dir: &Path) -> Result<Self> {
        let mut set = Self::new();
        let local_path = git_dir.join("config");
        let ctx = IncludeContext {
            git_dir: Some(git_dir.to_path_buf()),
            command_line_relative_include_is_error: false,
        };
        if let Ok(Some(f)) = ConfigFile::from_path(&local_path, ConfigScope::Local) {
            Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
        }
        Ok(set)
    }

    /// Load configuration the way Git loads **protected** config (e.g. `uploadpack.packObjectsHook`).
    ///
    /// This matches Git's `read_protected_config`: system (optional), global files only (no
    /// repository or worktree `config`), then command-line overrides from `GIT_CONFIG_COUNT` /
    /// `GIT_CONFIG_PARAMETERS`. It does **not** read `$GIT_CONFIG` (Git omits that for protected
    /// config).
    ///
    /// Global file order matches Git: XDG `git/config` first (when present), then `~/.gitconfig`,
    /// unless `GIT_CONFIG_GLOBAL` is set (single file). When both global files exist, both are
    /// merged so later entries win for duplicate keys.
    pub fn load_protected(include_system: bool) -> Result<Self> {
        let mut set = Self::new();
        let ctx = IncludeContext {
            git_dir: None,
            command_line_relative_include_is_error: false,
        };

        if include_system && !git_config_nosystem_enabled() {
            let system_path = std::env::var("GIT_CONFIG_SYSTEM")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/etc/gitconfig"));
            if let Ok(Some(f)) = ConfigFile::from_path(&system_path, ConfigScope::System) {
                Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
            }
        }

        if let Ok(p) = std::env::var("GIT_CONFIG_GLOBAL") {
            let path = PathBuf::from(p);
            if let Ok(Some(f)) = ConfigFile::from_path(&path, ConfigScope::Global) {
                Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
            }
        } else {
            let mut global_paths = Vec::new();
            if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
                global_paths.push(PathBuf::from(xdg).join("git/config"));
            } else if let Some(home) = home_dir() {
                global_paths.push(home.join(".config/git/config"));
            }
            if let Some(home) = home_dir() {
                global_paths.push(home.join(".gitconfig"));
            }
            for path in global_paths {
                if let Ok(Some(f)) = ConfigFile::from_path(&path, ConfigScope::Global) {
                    Self::merge_with_includes(&mut set, &f, true, 0, &ctx)?;
                }
            }
        }

        add_environment_config_pairs(&mut set)?;

        if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
            for entry in parse_config_parameters(&params) {
                if let Some((key, val)) =
                    entry.split_once('\u{1}').or_else(|| entry.split_once('='))
                {
                    let _ = set.add_command_override(key.trim(), val);
                } else {
                    let _ = set.add_command_override(entry.trim(), "true");
                }
            }
        }

        Ok(set)
    }

    /// Merge a file, processing `[include]` and `[includeIf]` directives.
    fn merge_with_includes(
        set: &mut Self,
        file: &ConfigFile,
        process_includes: bool,
        depth: usize,
        ctx: &IncludeContext,
    ) -> Result<()> {
        let mut included_files = Vec::new();
        Self::merge_with_includes_collect(
            set,
            file,
            process_includes,
            depth,
            ctx,
            &mut included_files,
        )
    }

    /// [`Self::merge_with_includes`], additionally recording every resolved
    /// include target path in `included_files` (whether or not the target
    /// exists) so the cascade cache can stamp them.
    fn merge_with_includes_collect(
        set: &mut Self,
        file: &ConfigFile,
        process_includes: bool,
        depth: usize,
        ctx: &IncludeContext,
        included_files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        // Mirror Git behavior and stop runaway include recursion.
        // t0017 expects the diagnostic to contain this exact phrase.
        const MAX_INCLUDE_DEPTH: usize = 10;
        if depth > MAX_INCLUDE_DEPTH {
            return Err(Error::ConfigError(
                "exceeded maximum include depth".to_owned(),
            ));
        }
        if !process_includes {
            set.merge(file);
            return Ok(());
        }

        for entry in &file.entries {
            set.entries.push(entry.clone());

            let Some((inc_path, condition)) = include_directive_for_entry(entry) else {
                continue;
            };
            let included_by_hasconfig = condition.as_deref().is_some_and(is_hasconfig_remote_url);
            if condition.is_some() && !included_by_hasconfig {
                let cond = condition.as_deref().unwrap_or_default();
                if !evaluate_include_condition(cond, set, file, ctx) {
                    continue;
                }
            }

            let resolved = match resolve_include_file_path(&inc_path, file, ctx) {
                Ok(p) => p,
                Err(Error::ConfigError(msg)) if msg.is_empty() => continue,
                Err(e) => return Err(e),
            };
            included_files.push(resolved.clone());
            // Git's `git_config_from_file` surfaces parse errors in an included file as a
            // fatal error (t0001 #102 `re-init reads matching includeIf.onbranch`). A missing
            // include target is silently skipped (`from_path` -> `Ok(None)`).
            let Some(inc_file) = ConfigFile::from_path(&resolved, file.scope)? else {
                continue;
            };

            if included_by_hasconfig {
                validate_hasconfig_remote_url_include(&inc_file, process_includes, depth + 1, ctx)?;
                let cond = condition.as_deref().unwrap_or_default();
                if !evaluate_include_condition(cond, set, file, ctx) {
                    continue;
                }
            }

            Self::merge_with_includes_collect(
                set,
                &inc_file,
                process_includes,
                depth + 1,
                ctx,
                included_files,
            )?;
        }

        Ok(())
    }
}

fn include_directive_for_entry(entry: &ConfigEntry) -> Option<(String, Option<String>)> {
    let val = entry.value.as_ref()?;
    if entry.key == "include.path" {
        return Some((val.clone(), None));
    }
    if entry.key.starts_with("includeif.") && entry.key.ends_with(".path") {
        let mid = &entry.key["includeif.".len()..entry.key.len() - ".path".len()];
        return Some((val.clone(), Some(mid.to_owned())));
    }
    None
}

// ── Process-lifetime config cascade cache ───────────────────────────
//
// `ConfigSet::load` / `load_with_options` are called per file in hot loops
// (grep/diff/add re-load the cascade for every path), so the parsed cascade
// is memoized for the lifetime of the process. A cached entry is served only
// when every input that fed it is provably unchanged:
//
// - the stat stamps (mtime + size, or "absent") of every base cascade file,
//   recorded *before* the parse, still match. `HEAD` is stamped too so that
//   `includeIf.onbranch:` condition flips invalidate;
// - every include target the parse resolved (even ones that were missing)
//   still carries the stamp it had at parse time;
// - the config-relevant environment is byte-identical.
//
// The remaining `includeIf` condition inputs are covered transitively:
// `gitdir:` depends only on the cache key and environment, and
// `hasconfig:remote.*.url:` depends only on the (stamped) file contents.
//
// In-process writers ([`ConfigFile::write`]) additionally evict every entry
// whose cascade contains the written path, closing the window where a
// rewrite lands within one mtime tick at an unchanged size. External writers
// in that same window are not detectable by stat; C git parses the cascade
// once per process with no revalidation at all, so serving a stamped copy is
// strictly more conservative than upstream.

/// Stat snapshot of one cascade file (`None` = file absent).
type ConfigFileStamp = (PathBuf, Option<(SystemTime, u64)>);

#[derive(PartialEq, Eq, Hash)]
struct ConfigCacheKey {
    git_dir: Option<PathBuf>,
    include_system: bool,
    process_includes: bool,
    command_includes: bool,
    ctx_git_dir: Option<PathBuf>,
    ctx_relative_include_is_error: bool,
}

impl ConfigCacheKey {
    fn new(git_dir: Option<&Path>, opts: &LoadConfigOptions) -> Self {
        Self {
            git_dir: git_dir.map(Path::to_path_buf),
            include_system: opts.include_system,
            process_includes: opts.process_includes,
            command_includes: opts.command_includes,
            ctx_git_dir: opts.include_ctx.git_dir.clone(),
            ctx_relative_include_is_error: opts.include_ctx.command_line_relative_include_is_error,
        }
    }
}

struct ConfigCacheEntry {
    env_fingerprint: Vec<(String, Option<String>)>,
    /// Stamps of the fixed cascade files (recomputable from key + env).
    base_stamps: Vec<ConfigFileStamp>,
    /// Stamps of the include targets this parse resolved (revalidated by
    /// re-statting each stored path).
    extra_stamps: Vec<ConfigFileStamp>,
    set: Arc<ConfigSet>,
}

fn config_cache() -> &'static Mutex<HashMap<ConfigCacheKey, ConfigCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<ConfigCacheKey, ConfigCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Environment that feeds the cascade (file paths and synthetic entries).
/// `None` means "do not cache this load".
fn config_env_fingerprint() -> Option<Vec<(String, Option<String>)>> {
    const VARS: [&str; 8] = [
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_GLOBAL",
        "XDG_CONFIG_HOME",
        "HOME",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ];
    let mut fp: Vec<(String, Option<String>)> = VARS
        .iter()
        .map(|name| ((*name).to_owned(), std::env::var(name).ok()))
        .collect();
    if let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT") {
        // Mirror `add_environment_config_pairs`; absurd counts are not worth caching.
        const MAX_TRACKED: usize = 256;
        match count_str.parse::<usize>() {
            Ok(n) if n <= MAX_TRACKED => {
                for i in 0..n {
                    for var in [
                        format!("GIT_CONFIG_KEY_{i}"),
                        format!("GIT_CONFIG_VALUE_{i}"),
                    ] {
                        let val = std::env::var(&var).ok();
                        fp.push((var, val));
                    }
                }
            }
            Ok(_) => return None,
            Err(_) => {}
        }
    }
    Some(fp)
}

/// The on-disk files [`ConfigSet::load_with_options_uncached`] would consult,
/// in cascade order.
fn config_cascade_file_paths(git_dir: Option<&Path>, opts: &LoadConfigOptions) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if opts.include_system && !git_config_nosystem_enabled() {
        paths.push(
            std::env::var("GIT_CONFIG_SYSTEM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/etc/gitconfig")),
        );
    }
    paths.extend(global_config_paths());
    if let Some(gd) = git_dir {
        let common_dir = crate::repo::common_git_dir_for_config(gd);
        paths.push(common_dir.join("config"));
        // Stamped unconditionally: whether it is merged depends on
        // `extensions.worktreeConfig` in the (stamped) common config.
        paths.push(gd.join("config.worktree"));
        // `includeIf.onbranch:` conditions read HEAD; stamping it means a
        // branch switch invalidates cascades that could depend on it.
        paths.push(gd.join("HEAD"));
    }
    if let Some(cgd) = &opts.include_ctx.git_dir {
        if Some(cgd.as_path()) != git_dir {
            paths.push(cgd.join("HEAD"));
        }
    }
    if let Ok(p) = std::env::var("GIT_CONFIG") {
        paths.push(PathBuf::from(p));
    }
    paths
}

fn stamp_for_path(path: &Path) -> Option<(SystemTime, u64)> {
    fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

fn stamp_paths(paths: Vec<PathBuf>) -> Vec<ConfigFileStamp> {
    paths
        .into_iter()
        .map(|path| {
            let stamp = stamp_for_path(&path);
            (path, stamp)
        })
        .collect()
}

fn config_file_stamps(git_dir: Option<&Path>, opts: &LoadConfigOptions) -> Vec<ConfigFileStamp> {
    stamp_paths(config_cascade_file_paths(git_dir, opts))
}

fn config_cache_lookup(
    key: &ConfigCacheKey,
    env_fp: &[(String, Option<String>)],
    base_stamps: &[ConfigFileStamp],
) -> Option<ConfigSet> {
    let cache = config_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = cache.get(key)?;
    if entry.env_fingerprint.as_slice() != env_fp || entry.base_stamps.as_slice() != base_stamps {
        return None;
    }
    for (path, stamp) in &entry.extra_stamps {
        if stamp_for_path(path) != *stamp {
            return None;
        }
    }
    Some((*entry.set).clone())
}

/// Drop every cached cascade that read `path` (called on in-process writes).
fn evict_config_cache_for_path(path: &Path) {
    let mut cache = config_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|_, entry| {
        !entry
            .base_stamps
            .iter()
            .chain(&entry.extra_stamps)
            .any(|(p, _)| p == path)
    });
}

fn git_config_nosystem_enabled() -> bool {
    std::env::var("GIT_CONFIG_NOSYSTEM")
        .ok()
        .map(|value| parse_bool(&value).unwrap_or(true))
        .unwrap_or(false)
}

fn add_environment_config_pairs(set: &mut ConfigSet) -> Result<()> {
    let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT") else {
        return Ok(());
    };
    if count_str.is_empty() {
        return Ok(());
    }

    let count = count_str
        .parse::<usize>()
        .map_err(|_| Error::ConfigError("bogus count in GIT_CONFIG_COUNT".to_owned()))?;
    if count > i32::MAX as usize {
        return Err(Error::ConfigError(
            "too many entries in GIT_CONFIG_COUNT".to_owned(),
        ));
    }

    for i in 0..count {
        let key_var = format!("GIT_CONFIG_KEY_{i}");
        let value_var = format!("GIT_CONFIG_VALUE_{i}");
        let key = std::env::var(&key_var)
            .map_err(|_| Error::ConfigError(format!("missing config key {key_var}")))?;
        let value = std::env::var(&value_var)
            .map_err(|_| Error::ConfigError(format!("missing config value {value_var}")))?;
        set.add_command_override(&key, &value)?;
    }

    Ok(())
}

// ── Type coercion helpers ───────────────────────────────────────────

/// Parse a Git boolean value.
///
/// Accepts: `true`, `yes`, `on`, `1` as true.
/// Accepts: `false`, `no`, `off`, `0` as false.
///
/// Note: bare config keys are represented as `None` in [`ConfigEntry`] and
/// are normalized to `"true"` by higher-level readers (`ConfigSet::get`).
/// An explicit empty assignment (`key =` with no value) is stored as `""` and
/// is treated as false for `--bool` / [`parse_bool`]. Bare keys are represented
/// as `None` and normalized to `"true"` by callers before reaching this parser.
pub fn parse_bool(s: &str) -> std::result::Result<bool, String> {
    match s.to_lowercase().as_str() {
        "true" | "yes" | "on" => Ok(true),
        "" => Ok(false),
        "false" | "no" | "off" => Ok(false),
        _ => {
            // Try parsing as Git's config integer syntax: 0 -> false, non-zero -> true.
            if let Ok(n) = parse_i64(s) {
                return Ok(n != 0);
            }
            Err(format!("bad boolean config value '{s}'"))
        }
    }
}

/// Parse a Git integer value with optional `k`/`m`/`g` suffix.
pub fn parse_i64(s: &str) -> std::result::Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty integer value".to_owned());
    }

    let (num_str, multiplier) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024_i64),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1_i64),
    };

    let base: i64 = num_str
        .parse()
        .map_err(|_| format!("invalid integer: '{s}'"))?;
    base.checked_mul(multiplier)
        .ok_or_else(|| format!("integer overflow: '{s}'"))
}

/// Why [`parse_git_config_int_strict`] failed (mirrors Git `errno` after `git_parse_signed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitConfigIntStrictError {
    /// `EINVAL` — trailing junk, unknown unit suffix, or not a number.
    InvalidUnit,
    /// `ERANGE` — value does not fit in `i64` after scaling.
    OutOfRange,
}

/// Parse a signed decimal integer with optional `k`/`m`/`g` multiplier suffix, requiring the
/// entire input (trimmed) to be consumed — same constraints as Git's `git_parse_signed` used by
/// `git_config_int` (so `no` and `1foo` are rejected, unlike [`parse_i64`]).
pub fn parse_git_config_int_strict(raw: &str) -> std::result::Result<i64, GitConfigIntStrictError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(GitConfigIntStrictError::InvalidUnit);
    }

    let bytes = s.as_bytes();
    let mut idx = 0usize;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        idx = 1;
    }
    if idx >= bytes.len() {
        return Err(GitConfigIntStrictError::InvalidUnit);
    }
    let digit_start = idx;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == digit_start {
        return Err(GitConfigIntStrictError::InvalidUnit);
    }

    let num_part =
        std::str::from_utf8(&bytes[..idx]).map_err(|_| GitConfigIntStrictError::InvalidUnit)?;
    let suffix =
        std::str::from_utf8(&bytes[idx..]).map_err(|_| GitConfigIntStrictError::InvalidUnit)?;
    let mult: i64 = match suffix {
        "" => 1,
        "k" | "K" => 1024,
        "m" | "M" => 1024 * 1024,
        "g" | "G" => 1024_i64
            .checked_mul(1024)
            .and_then(|x| x.checked_mul(1024))
            .ok_or(GitConfigIntStrictError::OutOfRange)?,
        _ => return Err(GitConfigIntStrictError::InvalidUnit),
    };

    let val: i64 = num_part
        .parse()
        .map_err(|_| GitConfigIntStrictError::InvalidUnit)?;
    val.checked_mul(mult)
        .ok_or(GitConfigIntStrictError::OutOfRange)
}

const DIFF_CONTEXT_KEY: &str = "diff.context";

fn format_bad_numeric_diff_context(
    value: &str,
    err: GitConfigIntStrictError,
    entry: &ConfigEntry,
) -> String {
    let detail = match err {
        GitConfigIntStrictError::InvalidUnit => "invalid unit",
        GitConfigIntStrictError::OutOfRange => "out of range",
    };
    if entry.scope == ConfigScope::Command || entry.file.is_none() {
        return format!(
            "fatal: bad numeric config value '{value}' for '{DIFF_CONTEXT_KEY}': {detail}"
        );
    }
    let path = entry
        .file
        .as_deref()
        .map(config_error_path_display)
        .unwrap_or_default();
    format!("fatal: bad numeric config value '{value}' for '{DIFF_CONTEXT_KEY}' in file {path}: {detail}")
}

fn format_bad_diff_context_variable(entry: &ConfigEntry) -> String {
    if entry.scope == ConfigScope::Command || entry.file.is_none() {
        return format!("fatal: unable to parse '{DIFF_CONTEXT_KEY}' from command-line config");
    }
    let path = entry
        .file
        .as_deref()
        .map(config_error_path_display)
        .unwrap_or_default();
    format!(
        "fatal: bad config variable '{DIFF_CONTEXT_KEY}' in file '{path}' at line {}",
        entry.line
    )
}

/// Read `diff.context` from a loaded [`ConfigSet`] with Git-compatible validation.
///
/// Returns `Ok(None)` when the key is unset. When set, the value must be a non-negative integer
/// acceptable to Git's diff machinery (same rules as `git diff` / `git log -p`).
pub fn resolve_diff_context_lines(cfg: &ConfigSet) -> std::result::Result<Option<usize>, String> {
    let Some(entry) = cfg.get_last_entry(DIFF_CONTEXT_KEY) else {
        return Ok(None);
    };
    let value_src = entry.value.as_deref().unwrap_or("").trim();
    match parse_git_config_int_strict(value_src) {
        Ok(n) if n < 0 => Err(format_bad_diff_context_variable(&entry)),
        Ok(n) => Ok(Some(usize::try_from(n).map_err(|_| {
            format_bad_numeric_diff_context(value_src, GitConfigIntStrictError::OutOfRange, &entry)
        })?)),
        Err(e) => Err(format_bad_numeric_diff_context(value_src, e, &entry)),
    }
}

/// Parse a Git color value and return the ANSI escape sequence.
///
/// Matches Git's `color_parse_mem` (`git/color.c`): whitespace-separated words,
/// optional leading `reset`, up to two color tokens (foreground then background),
/// then graphic rendition attributes. Attribute codes are accumulated as a
/// bitmask keyed by SGR number (so `bold` sets bit 1, `nobold` sets bit 22).
pub fn parse_color(s: &str) -> std::result::Result<String, String> {
    const COLOR_BACKGROUND_OFFSET: i32 = 10;
    const COLOR_FOREGROUND_ANSI: i32 = 30;
    const COLOR_FOREGROUND_RGB: i32 = 38;
    const COLOR_FOREGROUND_256: i32 = 38;
    const COLOR_FOREGROUND_BRIGHT_ANSI: i32 = 90;

    #[derive(Clone, Copy, Default)]
    struct Color {
        kind: u8,
        value: u8,
        red: u8,
        green: u8,
        blue: u8,
    }

    const COLOR_UNSPECIFIED: u8 = 0;
    const COLOR_NORMAL: u8 = 1;
    const COLOR_ANSI: u8 = 2;
    const COLOR_256: u8 = 3;
    const COLOR_RGB: u8 = 4;

    fn color_empty(c: &Color) -> bool {
        c.kind == COLOR_UNSPECIFIED || c.kind == COLOR_NORMAL
    }

    fn parse_ansi_color(name: &str) -> Option<Color> {
        let color_names = [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ];
        let color_offset = COLOR_FOREGROUND_ANSI;

        if name.eq_ignore_ascii_case("default") {
            return Some(Color {
                kind: COLOR_ANSI,
                value: (9 + color_offset) as u8,
                ..Default::default()
            });
        }

        let (name, color_offset) = if name.len() >= 6 && name[..6].eq_ignore_ascii_case("bright") {
            (&name[6..], COLOR_FOREGROUND_BRIGHT_ANSI)
        } else {
            (name, COLOR_FOREGROUND_ANSI)
        };

        for (i, cn) in color_names.iter().enumerate() {
            if name.eq_ignore_ascii_case(cn) {
                return Some(Color {
                    kind: COLOR_ANSI,
                    value: (i as i32 + color_offset) as u8,
                    ..Default::default()
                });
            }
        }
        None
    }

    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    fn get_hex_color(chars: &[u8], width: usize) -> Option<(u8, usize)> {
        assert!(width == 1 || width == 2);
        if chars.len() < width {
            return None;
        }
        let v = if width == 2 {
            let hi = hex_val(chars[0])?;
            let lo = hex_val(chars[1])?;
            (hi << 4) | lo
        } else {
            let n = hex_val(chars[0])?;
            (n << 4) | n
        };
        Some((v, width))
    }

    fn parse_single_color(word: &str) -> Option<Color> {
        if word.eq_ignore_ascii_case("normal") {
            return Some(Color {
                kind: COLOR_NORMAL,
                ..Default::default()
            });
        }

        let bytes = word.as_bytes();
        if (bytes.len() == 7 || bytes.len() == 4) && bytes.first() == Some(&b'#') {
            let width = if bytes.len() == 7 { 2 } else { 1 };
            let mut idx = 1;
            let (r, n1) = get_hex_color(&bytes[idx..], width)?;
            idx += n1;
            let (g, n2) = get_hex_color(&bytes[idx..], width)?;
            idx += n2;
            let (b, n3) = get_hex_color(&bytes[idx..], width)?;
            idx += n3;
            if idx != bytes.len() {
                return None;
            }
            return Some(Color {
                kind: COLOR_RGB,
                red: r,
                green: g,
                blue: b,
                ..Default::default()
            });
        }

        if let Some(c) = parse_ansi_color(word) {
            return Some(c);
        }

        let Ok(val) = word.parse::<i64>() else {
            return None;
        };
        if val < -1 {
            return None;
        }
        if val < 0 {
            return Some(Color {
                kind: COLOR_NORMAL,
                ..Default::default()
            });
        }
        if val < 8 {
            return Some(Color {
                kind: COLOR_ANSI,
                value: (val as i32 + COLOR_FOREGROUND_ANSI) as u8,
                ..Default::default()
            });
        }
        if val < 16 {
            return Some(Color {
                kind: COLOR_ANSI,
                value: (val as i32 - 8 + COLOR_FOREGROUND_BRIGHT_ANSI) as u8,
                ..Default::default()
            });
        }
        if val < 256 {
            return Some(Color {
                kind: COLOR_256,
                value: val as u8,
                ..Default::default()
            });
        }
        None
    }

    fn parse_attr(word: &str) -> Option<u8> {
        const ATTRS: [(&str, u8, u8); 8] = [
            ("bold", 1, 22),
            ("dim", 2, 22),
            ("italic", 3, 23),
            ("ul", 4, 24),
            ("underline", 4, 24),
            ("blink", 5, 25),
            ("reverse", 7, 27),
            ("strike", 9, 29),
        ];

        let mut negate = false;
        let mut rest = word;
        if let Some(stripped) = rest.strip_prefix("no") {
            negate = true;
            rest = stripped;
            if let Some(s) = rest.strip_prefix('-') {
                rest = s;
            }
        }

        for (name, val, neg) in ATTRS {
            if rest == name {
                return Some(if negate { neg } else { val });
            }
        }
        None
    }

    fn append_color_output(out: &mut String, c: &Color, background: bool) {
        let offset = if background {
            COLOR_BACKGROUND_OFFSET
        } else {
            0
        };
        match c.kind {
            COLOR_UNSPECIFIED | COLOR_NORMAL => {}
            COLOR_ANSI => {
                use std::fmt::Write;
                let _ = write!(out, "{}", i32::from(c.value) + offset);
            }
            COLOR_256 => {
                use std::fmt::Write;
                let _ = write!(out, "{};5;{}", COLOR_FOREGROUND_256 + offset, c.value);
            }
            COLOR_RGB => {
                use std::fmt::Write;
                let _ = write!(
                    out,
                    "{};2;{};{};{}",
                    COLOR_FOREGROUND_RGB + offset,
                    c.red,
                    c.green,
                    c.blue
                );
            }
            _ => {}
        }
    }

    let s = s.trim();
    if s.is_empty() {
        return Ok(String::new());
    }

    let mut has_reset = false;
    let mut attr: u64 = 0;
    let mut fg = Color::default();
    let mut bg = Color::default();
    fg.kind = COLOR_UNSPECIFIED;
    bg.kind = COLOR_UNSPECIFIED;

    for word in s.split_whitespace() {
        if word.eq_ignore_ascii_case("reset") {
            has_reset = true;
            continue;
        }

        if let Some(c) = parse_single_color(word) {
            if fg.kind == COLOR_UNSPECIFIED {
                fg = c;
                continue;
            }
            if bg.kind == COLOR_UNSPECIFIED {
                bg = c;
                continue;
            }
            return Err(format!("bad color value '{s}'"));
        }

        if let Some(code) = parse_attr(word) {
            attr |= 1u64 << u64::from(code);
            continue;
        }

        return Err(format!("bad color value '{s}'"));
    }

    if !has_reset && attr == 0 && color_empty(&fg) && color_empty(&bg) {
        return Err(format!("bad color value '{s}'"));
    }

    let mut out = String::from("\x1b[");
    let mut sep = if has_reset { 1u32 } else { 0u32 };

    let mut attr_bits = attr;
    let mut i = 0u32;
    while attr_bits != 0 {
        let bit = 1u64 << i;
        if attr_bits & bit == 0 {
            i += 1;
            continue;
        }
        attr_bits &= !bit;
        if sep > 0 {
            out.push(';');
        }
        sep += 1;
        use std::fmt::Write;
        let _ = write!(out, "{i}");
        i += 1;
    }

    if !color_empty(&fg) {
        if sep > 0 {
            out.push(';');
        }
        sep += 1;
        append_color_output(&mut out, &fg, false);
    }
    if !color_empty(&bg) {
        if sep > 0 {
            out.push(';');
        }
        append_color_output(&mut out, &bg, true);
    }
    out.push('m');
    Ok(out)
}

#[derive(Debug, Clone)]
struct UrlParts {
    scheme: String,
    user: Option<String>,
    host: String,
    port: Option<String>,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct UrlMatchScore {
    host_len: usize,
    path_len: usize,
    user_matched: bool,
}

fn parse_config_url(url: &str) -> Option<UrlParts> {
    let (scheme, rest) = url.split_once("://")?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user.to_owned()), host),
        None => (None, authority),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.contains(']') => (host, Some(port.to_owned())),
        _ => (host_port, None),
    };
    Some(UrlParts {
        scheme: scheme.to_lowercase(),
        user,
        host: host.to_lowercase(),
        port,
        path: if path.is_empty() {
            "/".to_owned()
        } else {
            path.trim_end_matches('/').to_owned()
        },
    })
}

fn host_matches(pattern: &str, target: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('.').collect();
    let target_parts: Vec<&str> = target.split('.').collect();
    pattern_parts.len() == target_parts.len()
        && pattern_parts
            .iter()
            .zip(target_parts)
            .all(|(pattern, target)| *pattern == "*" || *pattern == target)
}

fn path_match_len(pattern: &str, target: &str) -> Option<usize> {
    let pattern = if pattern.is_empty() { "/" } else { pattern };
    let target = if target.is_empty() { "/" } else { target };
    if pattern == "/" {
        return Some(1);
    }
    let pattern = pattern.trim_end_matches('/');
    if target == pattern
        || target
            .strip_prefix(pattern)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        Some(pattern.len() + 1)
    } else {
        None
    }
}

fn url_match_score(pattern_url: &str, target_url: &str) -> Option<UrlMatchScore> {
    let pattern = parse_config_url(pattern_url)?;
    let target = parse_config_url(target_url)?;
    if pattern.scheme != target.scheme {
        return None;
    }
    let user_matched = match pattern.user.as_deref() {
        Some(user) if target.user.as_deref() == Some(user) => true,
        Some(_) => return None,
        None => false,
    };
    if !host_matches(&pattern.host, &target.host) || pattern.port != target.port {
        return None;
    }
    let path_len = path_match_len(&pattern.path, &target.path)?;
    Some(UrlMatchScore {
        host_len: pattern.host.len(),
        path_len,
        user_matched,
    })
}

/// Match a URL against a URL pattern from config.
pub fn url_matches(pattern_url: &str, target_url: &str) -> bool {
    url_match_score(pattern_url, target_url).is_some()
}

/// Get the best URL match for a specific key.
pub fn get_urlmatch_entries<'a>(
    entries: &'a [ConfigEntry],
    section: &str,
    variable: &str,
    url: &str,
) -> Vec<&'a ConfigEntry> {
    let section_lower = section.to_lowercase();
    let variable_lower = variable.to_lowercase();
    let mut matches: Vec<(UrlMatchScore, &'a ConfigEntry)> = Vec::new();

    for entry in entries {
        let key = &entry.key;
        let first_dot = match key.find('.') {
            Some(i) => i,
            None => continue,
        };
        let last_dot = match key.rfind('.') {
            Some(i) => i,
            None => continue,
        };
        let entry_section = &key[..first_dot];
        let entry_variable = &key[last_dot + 1..];
        if entry_section.to_lowercase() != section_lower
            || entry_variable.to_lowercase() != variable_lower
        {
            continue;
        }
        if first_dot == last_dot {
            matches.push((
                UrlMatchScore {
                    host_len: 0,
                    path_len: 0,
                    user_matched: false,
                },
                entry,
            ));
        } else {
            let subsection = &key[first_dot + 1..last_dot];
            if let Some(score) = url_match_score(subsection, url) {
                matches.push((score, entry));
            }
        }
    }
    matches.sort_by_key(|a| a.0);
    matches.into_iter().map(|(_, e)| e).collect()
}

/// Get all matching variables in a section for a given URL.
pub fn get_urlmatch_all_in_section(
    entries: &[ConfigEntry],
    section: &str,
    url: &str,
) -> Vec<(String, String, ConfigScope)> {
    let section_lower = section.to_lowercase();
    let mut matches: Vec<(String, UrlMatchScore, String, String, ConfigScope)> = Vec::new();

    for entry in entries {
        let key = &entry.key;
        let first_dot = match key.find('.') {
            Some(i) => i,
            None => continue,
        };
        let last_dot = match key.rfind('.') {
            Some(i) => i,
            None => continue,
        };
        let entry_section = &key[..first_dot];
        if entry_section.to_lowercase() != section_lower {
            continue;
        }
        let entry_variable = &key[last_dot + 1..];
        let val = entry.value.as_deref().unwrap_or("");
        if first_dot == last_dot {
            let canonical = format!("{}.{}", section_lower, entry_variable);
            matches.push((
                entry_variable.to_lowercase(),
                UrlMatchScore {
                    host_len: 0,
                    path_len: 0,
                    user_matched: false,
                },
                val.to_owned(),
                canonical,
                entry.scope,
            ));
        } else {
            let subsection = &key[first_dot + 1..last_dot];
            if let Some(score) = url_match_score(subsection, url) {
                let canonical = format!("{}.{}", section_lower, entry_variable);
                matches.push((
                    entry_variable.to_lowercase(),
                    score,
                    val.to_owned(),
                    canonical,
                    entry.scope,
                ));
            }
        }
    }

    let mut best: std::collections::BTreeMap<String, (UrlMatchScore, String, String, ConfigScope)> =
        std::collections::BTreeMap::new();
    for (var, specificity, val, canonical, scope) in matches {
        let entry = best.entry(var).or_insert((
            UrlMatchScore {
                host_len: 0,
                path_len: 0,
                user_matched: false,
            },
            String::new(),
            String::new(),
            scope,
        ));
        if specificity >= entry.0 {
            *entry = (specificity, val, canonical, scope);
        }
    }
    best.into_values()
        .map(|(_, val, canonical, scope)| (canonical, val, scope))
        .collect()
}

/// Parse a Git path value (expand `~/` to home directory).
/// Parse a path value. Returns the resolved path string.
/// Does NOT handle :(optional) prefix — use `parse_path_optional` for that.
pub fn parse_path(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    s.to_owned()
}

/// Parse a path value that may have an `:(optional)` prefix.
///
/// Returns `Some(path)` if the path should be used, `None` if the path
/// is optional and does not exist (meaning the entry should be skipped).
pub fn parse_path_optional(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix(":(optional)") {
        let resolved = parse_path(rest);
        if std::path::Path::new(&resolved).exists() {
            Some(resolved)
        } else {
            None // optional and missing → skip
        }
    } else {
        Some(parse_path(s))
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Parse `GIT_CONFIG_PARAMETERS` payloads.
///
/// We support the common formats seen in tests and wrappers:
/// - single-quoted entries: `'key=value'`
/// - double-quoted entries: `"key=value"`
/// - unquoted `key=value` tokens separated by whitespace
///
/// Backslash escapes are interpreted minimally inside double quotes.
///
/// Return the last `key=value` assignment for `key` in a `GIT_CONFIG_PARAMETERS` payload.
///
/// Matches Git's command-line config layering: later tokens win. Keys are canonicalized the same
/// way as file-backed config (`fetch.output` and `FETCH.Output` both match `fetch.output`).
#[must_use]
pub fn git_config_parameters_last_value(raw: &str, key: &str) -> Option<String> {
    let Ok(canon) = canonical_key(key) else {
        return None;
    };
    let mut last: Option<String> = None;
    for entry in parse_config_parameters_strict(raw).ok()? {
        match entry {
            ConfigParameter::Pair { key, value } => {
                if canonical_key(key.trim()).ok().as_ref() == Some(&canon) {
                    last = Some(value.unwrap_or_else(|| "true".to_owned()));
                }
            }
            ConfigParameter::OldStyle(entry) => {
                if let Some((k, v)) = entry.split_once('=') {
                    if canonical_key(k.trim()).ok().as_ref() == Some(&canon) {
                        last = Some(v.to_owned());
                    }
                } else if canonical_key(entry.trim()).ok().as_ref() == Some(&canon) {
                    last = Some("true".to_owned());
                }
            }
        }
    }
    last
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigParameter {
    OldStyle(String),
    Pair { key: String, value: Option<String> },
}

pub fn parse_config_parameters(raw: &str) -> Vec<String> {
    parse_config_parameters_strict(raw)
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| match entry {
                    ConfigParameter::OldStyle(entry) => entry,
                    ConfigParameter::Pair {
                        key,
                        value: Some(value),
                    } => format!("{key}\u{1}{value}"),
                    ConfigParameter::Pair { key, value: None } => format!("{key}\u{1}"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_config_parameters_strict(raw: &str) -> Result<Vec<ConfigParameter>> {
    let mut out: Vec<ConfigParameter> = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut idx = skip_config_parameter_spaces(&chars, 0);

    while idx < chars.len() {
        let (key, next) = sq_dequote_step_chars(&chars, idx)?;
        let Some(next_idx) = next else {
            out.push(ConfigParameter::OldStyle(key));
            break;
        };

        if chars[next_idx].is_whitespace() {
            out.push(ConfigParameter::OldStyle(key));
            idx = skip_config_parameter_spaces(&chars, next_idx);
            continue;
        }

        if chars[next_idx] != '=' {
            return Err(Error::ConfigError(
                "bogus format in GIT_CONFIG_PARAMETERS".to_owned(),
            ));
        }

        let value_start = next_idx + 1;
        if value_start >= chars.len() || chars[value_start].is_whitespace() {
            out.push(ConfigParameter::Pair { key, value: None });
            idx = skip_config_parameter_spaces(&chars, value_start);
            continue;
        }

        if chars[value_start] != '\'' {
            return Err(Error::ConfigError(
                "bogus format in GIT_CONFIG_PARAMETERS".to_owned(),
            ));
        }
        let (value, value_next) = sq_dequote_step_chars(&chars, value_start)?;
        if let Some(value_next) = value_next {
            if !chars[value_next].is_whitespace() {
                return Err(Error::ConfigError(
                    "bogus format in GIT_CONFIG_PARAMETERS".to_owned(),
                ));
            }
            idx = skip_config_parameter_spaces(&chars, value_next);
        } else {
            idx = chars.len();
        }
        out.push(ConfigParameter::Pair {
            key,
            value: Some(value),
        });
    }

    Ok(out)
}

fn skip_config_parameter_spaces(chars: &[char], mut idx: usize) -> usize {
    while idx < chars.len() && chars[idx].is_whitespace() {
        idx += 1;
    }
    idx
}

fn sq_dequote_step_chars(chars: &[char], start: usize) -> Result<(String, Option<usize>)> {
    if chars.get(start) != Some(&'\'') {
        return Err(Error::ConfigError(
            "bogus format in GIT_CONFIG_PARAMETERS".to_owned(),
        ));
    }

    let mut out = String::new();
    let mut idx = start + 1;
    loop {
        let Some(&ch) = chars.get(idx) else {
            return Err(Error::ConfigError(
                "bogus format in GIT_CONFIG_PARAMETERS".to_owned(),
            ));
        };
        if ch != '\'' {
            out.push(ch);
            idx += 1;
            continue;
        }

        idx += 1;
        match chars.get(idx).copied() {
            None => return Ok((out, None)),
            Some('\\')
                if chars
                    .get(idx + 1)
                    .copied()
                    .is_some_and(needs_sq_backslash_quote)
                    && chars.get(idx + 2) == Some(&'\'') =>
            {
                if let Some(escaped) = chars.get(idx + 1) {
                    out.push(*escaped);
                }
                idx += 3;
            }
            _ => return Ok((out, Some(idx))),
        }
    }
}

fn needs_sq_backslash_quote(ch: char) -> bool {
    ch == '\'' || ch == '!'
}

/// Return candidate paths for the global config file, in priority order.
/// Public accessor for the ordered list of global config file paths.
pub fn global_config_paths_pub() -> Vec<PathBuf> {
    global_config_paths()
}

fn global_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // $GIT_CONFIG_GLOBAL overrides
    if let Ok(p) = std::env::var("GIT_CONFIG_GLOBAL") {
        paths.push(PathBuf::from(p));
        return paths;
    }

    // Git order: XDG `git/config` first, then `~/.gitconfig` (see `git_global_config_paths`).
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(xdg).join("git/config"));
    } else if let Some(home) = home_dir() {
        paths.push(home.join(".config/git/config"));
    }
    if let Some(home) = home_dir() {
        paths.push(home.join(".gitconfig"));
    }

    paths
}

/// Return the user's home directory.
///
/// `$HOME` is honored on every platform. On Windows, where `$HOME` is usually
/// unset outside a Git-for-Windows/MSYS shell, we fall back to the same sources
/// Git does: `%HOMEDRIVE%%HOMEPATH%`, then `%USERPROFILE%`. Without this, global
/// config (`~/.gitconfig`) would be invisible on a stock Windows shell.
fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    {
        if let (Some(drive), Some(path)) = (
            std::env::var_os("HOMEDRIVE").filter(|d| !d.is_empty()),
            std::env::var_os("HOMEPATH").filter(|p| !p.is_empty()),
        ) {
            let mut combined = drive;
            combined.push(path);
            return Some(PathBuf::from(combined));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(profile));
        }
    }
    None
}

/// True when Git would treat the config source as `CONFIG_ORIGIN_FILE` for includes.
fn include_source_is_disk_file(file: &ConfigFile) -> bool {
    file.include_origin == ConfigIncludeOrigin::Disk
}

/// Resolve an include file path (Git `handle_path_include` semantics).
///
/// Relative paths are only allowed when the including config came from a real on-disk file.
fn resolve_include_file_path(
    path: &str,
    file: &ConfigFile,
    ctx: &IncludeContext,
) -> Result<PathBuf> {
    let expanded = parse_path(path);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if !include_source_is_disk_file(file) {
        if file.include_origin == ConfigIncludeOrigin::CommandLine {
            if ctx.command_line_relative_include_is_error {
                return Err(Error::ConfigError(
                    "relative config includes must come from files".to_owned(),
                ));
            }
            return Err(Error::ConfigError(String::new()));
        }
        return Err(Error::ConfigError(
            "relative config includes must come from files".to_owned(),
        ));
    }
    let base = match file.path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        Some(_) | None => Path::new("."),
    };
    Ok(base.join(p))
}

fn is_dir_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

fn add_trailing_starstar_for_dir(pat: &mut String) {
    let bytes = pat.as_bytes();
    if bytes.last().is_some_and(|&b| is_dir_sep(b)) {
        pat.push_str("**");
    }
}

/// Prepare a `gitdir:` / `gitdir/i:` pattern (Git `prepare_include_condition_pattern`).
fn prepare_gitdir_pattern(condition: &str, file: &ConfigFile) -> Result<(String, usize)> {
    // Git `interpolate_path`: expand `~/` in the condition before pattern rules.
    let mut pat = parse_path(condition);
    if pat.starts_with("./") || pat.starts_with(".\\") {
        if !include_source_is_disk_file(file) {
            return Err(Error::ConfigError(
                "relative config include conditionals must come from files".to_owned(),
            ));
        }
        let parent = file.path.parent().ok_or_else(|| {
            Error::ConfigError(
                "relative config include conditionals must come from files".to_owned(),
            )
        })?;
        let real = parent.canonicalize().map_err(Error::Io)?;
        let mut dir = real.to_string_lossy().into_owned();
        if !dir.ends_with('/') && !dir.ends_with('\\') {
            dir.push('/');
        }
        let rest = &pat[2..];
        pat = format!("{dir}{rest}");
        let prefix_len = dir.len();
        add_trailing_starstar_for_dir(&mut pat);
        return Ok((pat, prefix_len));
    }
    let p = Path::new(&pat);
    if !p.is_absolute() {
        pat.insert_str(0, "**/");
    }
    add_trailing_starstar_for_dir(&mut pat);
    Ok((pat, 0))
}

/// Git `include_by_gitdir` tries `strbuf_realpath` first, then `strbuf_add_absolute_path` if no match.
///
/// `text_abs` uses `$PWD` (which preserves symlinks) when available, matching Git's
/// `strbuf_add_absolute_path` behaviour. This lets `gitdir:bar/` match when `bar` is a symlink.
fn git_dir_match_texts(git_dir: &Path) -> (String, String) {
    let real = git_dir
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| git_dir.to_string_lossy().into_owned());
    // Build the non-canonical absolute path using $PWD (symlink-preserving) when available.
    // Git C uses `strbuf_add_absolute_path` which prefers $PWD over getcwd() to preserve symlinks.
    let abs = if git_dir.is_absolute() {
        // If git_dir is already canonical, try to reconstruct the symlink-preserving variant
        // by replacing the canonical cwd prefix with $PWD.
        let pwd_abs = std::env::var("PWD").ok().and_then(|pwd| {
            let pwd_path = std::path::Path::new(&pwd);
            if !pwd_path.is_absolute() {
                return None;
            }
            let pwd_canon = pwd_path.canonicalize().ok()?;
            let git_dir_str = git_dir.to_string_lossy();
            let pwd_canon_str = pwd_canon.to_string_lossy();
            // If git_dir starts with the canonical cwd, replace that prefix with $PWD
            let suffix = git_dir_str.strip_prefix(pwd_canon_str.as_ref())?;
            Some(format!("{pwd}{suffix}"))
        });
        pwd_abs.unwrap_or_else(|| git_dir.to_string_lossy().into_owned())
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(git_dir).to_string_lossy().into_owned()
    } else {
        git_dir.to_string_lossy().into_owned()
    };
    (real, abs)
}

fn include_by_gitdir(
    condition: &str,
    file: &ConfigFile,
    ctx: &IncludeContext,
    icase: bool,
) -> bool {
    let Some(git_dir) = ctx.git_dir.as_ref() else {
        return false;
    };
    let (pattern, prefix) = match prepare_gitdir_pattern(condition, file) {
        Ok(x) => x,
        Err(_) => return false,
    };
    let flags = WM_PATHNAME | if icase { WM_CASEFOLD } else { 0 };
    let (text_real, text_abs) = git_dir_match_texts(git_dir);
    let try_match = |text: &str| -> bool {
        let t = text.as_bytes();
        let p = pattern.as_bytes();
        if prefix > 0 {
            if t.len() < prefix {
                return false;
            }
            let pre = &p[..prefix];
            let te = &t[..prefix];
            let ok = if icase {
                pre.eq_ignore_ascii_case(te)
            } else {
                pre == te
            };
            if !ok {
                return false;
            }
            return wildmatch(&p[prefix..], &t[prefix..], flags);
        }
        wildmatch(p, t, flags)
    };
    if try_match(&text_real) {
        return true;
    }
    text_real != text_abs && try_match(&text_abs)
}

fn current_branch_short_name(git_dir: Option<&Path>) -> Option<String> {
    let gd = git_dir?;
    let target = refs::read_symbolic_ref(gd, "HEAD").ok()??;
    let rest = target.strip_prefix("refs/heads/")?;
    Some(rest.to_owned())
}

fn include_by_onbranch(condition: &str, ctx: &IncludeContext) -> bool {
    let Some(short) = current_branch_short_name(ctx.git_dir.as_deref()) else {
        return false;
    };
    let mut pattern = condition.to_owned();
    add_trailing_starstar_for_dir(&mut pattern);
    wildmatch(pattern.as_bytes(), short.as_bytes(), WM_PATHNAME)
}

fn is_remote_url_entry(entry: &ConfigEntry) -> bool {
    let Ok((section, subsection, variable)) = split_key(&entry.key) else {
        return false;
    };
    section == "remote" && subsection.is_some() && variable == "url"
}

fn is_hasconfig_remote_url(condition: &str) -> bool {
    condition
        .strip_prefix("hasconfig:")
        .is_some_and(|rest| rest.starts_with("remote.*.url:"))
}

fn include_by_hasconfig_remote_url(condition: &str, set: &ConfigSet, file: &ConfigFile) -> bool {
    let Some(pattern) = condition.strip_prefix("remote.*.url:") else {
        return false;
    };
    set.entries
        .iter()
        .chain(file.entries.iter())
        .filter(|entry| is_remote_url_entry(entry))
        .filter_map(|entry| entry.value.as_deref())
        .any(|value| wildmatch(pattern.as_bytes(), value.as_bytes(), WM_PATHNAME))
}

fn validate_hasconfig_remote_url_include(
    file: &ConfigFile,
    process_includes: bool,
    depth: usize,
    ctx: &IncludeContext,
) -> Result<()> {
    const MAX_INCLUDE_DEPTH: usize = 10;
    if depth > MAX_INCLUDE_DEPTH {
        return Err(Error::ConfigError(
            "exceeded maximum include depth".to_owned(),
        ));
    }
    if file.entries.iter().any(is_remote_url_entry) {
        return Err(Error::Message(
            "fatal: remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
                .to_owned(),
        ));
    }
    if !process_includes {
        return Ok(());
    }
    for entry in &file.entries {
        let Some((inc_path, condition)) = include_directive_for_entry(entry) else {
            continue;
        };
        if let Some(ref cond) = condition {
            if !evaluate_include_condition(cond, &ConfigSet::new(), file, ctx) {
                continue;
            }
        }
        let resolved = match resolve_include_file_path(&inc_path, file, ctx) {
            Ok(p) => p,
            Err(Error::ConfigError(msg)) if msg.is_empty() => continue,
            Err(e) => return Err(e),
        };
        if let Some(inc_file) = ConfigFile::from_path(&resolved, file.scope)? {
            validate_hasconfig_remote_url_include(&inc_file, process_includes, depth + 1, ctx)?;
        }
    }
    Ok(())
}

/// Evaluate an `[includeIf]` condition.
///
/// Supports `gitdir:`, `gitdir/i:`, `onbranch:`, and `hasconfig:remote.*.url:` like Git.
/// Unknown prefixes are false.
fn evaluate_include_condition(
    condition: &str,
    set: &ConfigSet,
    file: &ConfigFile,
    ctx: &IncludeContext,
) -> bool {
    if let Some(rest) = condition.strip_prefix("gitdir/i:") {
        return include_by_gitdir(rest, file, ctx, true);
    }
    if let Some(rest) = condition.strip_prefix("gitdir:") {
        return include_by_gitdir(rest, file, ctx, false);
    }
    if let Some(rest) = condition.strip_prefix("onbranch:") {
        return include_by_onbranch(rest, ctx);
    }
    if let Some(rest) = condition.strip_prefix("hasconfig:") {
        return include_by_hasconfig_remote_url(rest, set, file);
    }
    false
}

/// Split a canonical key into (section, subsection, variable).
fn split_key(key: &str) -> Result<(String, Option<String>, String)> {
    let first_dot = key
        .find('.')
        .ok_or_else(|| Error::ConfigError(format!("invalid key: '{key}'")))?;
    let last_dot = key
        .rfind('.')
        .ok_or_else(|| Error::ConfigError(format!("invalid key: '{key}'")))?;

    let section = key[..first_dot].to_owned();
    let variable = key[last_dot + 1..].to_owned();

    let subsection = if first_dot == last_dot {
        None
    } else {
        Some(key[first_dot + 1..last_dot].to_owned())
    };

    Ok((section, subsection, variable))
}

/// Extract the variable name from a canonical key.
#[allow(dead_code)]
fn variable_name_from_key(key: &str) -> &str {
    match key.rfind('.') {
        Some(i) => &key[i + 1..],
        None => key,
    }
}

/// Parse a section name that may contain a subsection (e.g. `"remote.origin"`).
///
/// Returns (section, subsection).
fn parse_section_name(name: &str) -> (&str, Option<&str>) {
    match name.find('.') {
        Some(i) => (&name[..i], Some(&name[i + 1..])),
        None => (name, None),
    }
}

fn section_matches(parser: &Parser, section_lower: &str, subsection: Option<&str>) -> bool {
    if parser.section.to_lowercase() == section_lower && parser.subsection.as_deref() == subsection
    {
        return true;
    }
    let Some(subsection) = subsection else {
        return false;
    };
    parser.subsection.is_none()
        && parser.section.to_lowercase() == format!("{section_lower}.{}", subsection.to_lowercase())
}

fn validate_section_name(section: &str, subsection: Option<&str>) -> Result<()> {
    if section.is_empty()
        || !section
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        || subsection.is_some_and(str::is_empty)
    {
        return Err(Error::ConfigError(format!(
            "invalid section name: {section}"
        )));
    }
    Ok(())
}

/// Extract the original-case variable name from a raw (user-typed) key.
///
/// E.g. `"Section.Movie"` → `"Movie"`, `"a.b.CamelCase"` → `"CamelCase"`.
fn raw_variable_name(raw_key: &str) -> &str {
    match raw_key.rfind('.') {
        Some(i) => &raw_key[i + 1..],
        None => raw_key,
    }
}

/// Extract the original-case section and subsection from a raw (user-typed) key.
///
/// E.g. `"Section.key"` → `("Section", None)`,
///      `"Remote.origin.url"` → `("Remote", Some("origin"))`.
fn raw_section_parts(raw_key: &str) -> (String, Option<String>) {
    let first_dot = match raw_key.find('.') {
        Some(i) => i,
        None => return (raw_key.to_owned(), None),
    };
    // rfind always succeeds here since we already found at least one dot above.
    let last_dot = match raw_key.rfind('.') {
        Some(i) => i,
        None => return (raw_key[..first_dot].to_owned(), None),
    };
    let section = raw_key[..first_dot].to_owned();
    if first_dot == last_dot {
        (section, None)
    } else {
        let subsection = raw_key[first_dot + 1..last_dot].to_owned();
        (section, Some(subsection))
    }
}

/// Check if a raw line is a section header that also contains an inline key=value.
fn is_section_header_with_inline_entry(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') {
        return false;
    }
    let end = match trimmed.find(']') {
        Some(i) => i,
        None => return false,
    };
    let after = trimmed[end + 1..].trim();
    // Has non-comment content after the ]
    !after.is_empty() && !after.starts_with('#') && !after.starts_with(';')
}

/// Extract just the section header portion (up to and including `]` and any
/// comment after it, but not any inline key=value) from a raw line.
fn extract_section_header(line: &str) -> String {
    let trimmed = line.trim();
    let end = match trimmed.find(']') {
        Some(i) => i,
        None => return line.to_owned(),
    };
    // Preserve any comment on the section header itself (between ] and key),
    // but git doesn't really do this. Just return up to ].
    trimmed[..=end].to_owned()
}

#[cfg(test)]
mod get_regexp_tests {
    use super::{ConfigFile, ConfigScope, ConfigSet};
    use std::path::Path;

    fn set_from_snippet(text: &str) -> ConfigSet {
        let path = Path::new(".git/config");
        let file = ConfigFile::parse(path, text, ConfigScope::Local).expect("parse config snippet");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn get_regexp_matches_section_prefix_like_git_config() {
        let text = r#"
[user]
    email = alice@example.com
    name = Alice
[core]
    bare = false
"#;
        let set = set_from_snippet(text);
        let keys: Vec<_> = set
            .get_regexp("user")
            .expect("valid pattern")
            .into_iter()
            .map(|e| e.key.as_str())
            .collect();
        assert!(keys.contains(&"user.email"));
        assert!(keys.contains(&"user.name"));
        assert!(!keys.iter().any(|k| k.starts_with("core.")));
    }

    #[test]
    fn get_regexp_returns_all_multi_value_entries_in_order() {
        let text = r#"
[remote "origin"]
    url = https://example.com/repo.git
    fetch = +refs/heads/*:refs/remotes/origin/*
    push = +refs/heads/main:refs/heads/main
    push = +refs/heads/develop:refs/heads/develop
"#;
        let set = set_from_snippet(text);
        let matches = set.get_regexp("remote.origin").expect("valid pattern");
        let push_vals: Vec<_> = matches
            .iter()
            .filter(|e| e.key == "remote.origin.push")
            .map(|e| e.value.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(push_vals.len(), 2);
        assert_eq!(push_vals[0], "+refs/heads/main:refs/heads/main");
        assert_eq!(push_vals[1], "+refs/heads/develop:refs/heads/develop");
    }

    #[test]
    fn get_regexp_dot_matches_any_key() {
        let text = r#"
[a]
    x = 1
[b]
    y = 2
"#;
        let set = set_from_snippet(text);
        let m = set.get_regexp(".").expect("valid pattern");
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn get_regexp_no_match_returns_empty_vec() {
        let set = set_from_snippet("[user]\n\tname = x\n");
        let m = set.get_regexp("zzz").expect("valid pattern");
        assert!(m.is_empty());
    }

    #[test]
    fn get_regexp_invalid_pattern_is_error() {
        let set = set_from_snippet("[user]\n\tname = x\n");
        let err = set.get_regexp("(").expect_err("unclosed group");
        assert!(err.contains("invalid key pattern"), "got: {err}");
    }
}

#[cfg(test)]
mod pack_compression_tests {
    use super::{ConfigFile, ConfigScope, ConfigSet};
    use std::path::Path;

    fn set_from_snippet(text: &str) -> ConfigSet {
        let path = Path::new(".git/config");
        let file = ConfigFile::parse(path, text, ConfigScope::Local).expect("parse config snippet");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn pack_objects_zlib_level_defaults_to_six() {
        let set = ConfigSet::new();
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 6);
    }

    #[test]
    fn pack_objects_zlib_level_core_compression() {
        let set = set_from_snippet("[core]\n\tcompression = 0\n");
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 0);
        let set = set_from_snippet("[core]\n\tcompression = 9\n");
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 9);
    }

    #[test]
    fn pack_objects_zlib_level_pack_overrides_core() {
        let set = set_from_snippet("[core]\n\tcompression = 9\n[pack]\n\tcompression = 0\n");
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 0);
        let set = set_from_snippet("[core]\n\tcompression = 0\n[pack]\n\tcompression = 9\n");
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 9);
    }

    #[test]
    fn pack_objects_zlib_level_later_core_does_not_override_earlier_pack() {
        let mut set = ConfigSet::new();
        set.merge(
            &ConfigFile::parse(
                Path::new("a"),
                "[pack]\n\tcompression = 9\n",
                ConfigScope::Local,
            )
            .unwrap(),
        );
        set.merge(
            &ConfigFile::parse(
                Path::new("b"),
                "[core]\n\tcompression = 0\n",
                ConfigScope::Local,
            )
            .unwrap(),
        );
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 9);
    }

    #[test]
    fn pack_objects_zlib_level_loosecompression_does_not_block_core_pack_level() {
        let set = set_from_snippet("[core]\n\tloosecompression = 1\n\tcompression = 0\n");
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 0);
    }

    #[test]
    fn pack_objects_zlib_level_pack_wins_after_loose_and_core() {
        let set = set_from_snippet(
            "[core]\n\tloosecompression = 1\n\tcompression = 0\n[pack]\n\tcompression = 9\n",
        );
        assert_eq!(set.pack_objects_zlib_level().unwrap(), 9);
    }
}

#[cfg(test)]
mod config_cache_tests {
    use super::*;
    use filetime::FileTime;

    fn local_opts(git_dir: &Path) -> LoadConfigOptions {
        let mut opts = LoadConfigOptions::default();
        opts.include_system = false;
        opts.include_ctx.git_dir = Some(git_dir.to_path_buf());
        opts
    }

    fn load_value(git_dir: &Path) -> Option<String> {
        let opts = local_opts(git_dir);
        let set = ConfigSet::load_with_options(Some(git_dir), &opts).expect("load cascade");
        set.get("gritcachetest.value")
    }

    fn mtime_of(path: &Path) -> FileTime {
        FileTime::from_last_modification_time(&fs::metadata(path).expect("stat config"))
    }

    fn restore_mtime(path: &Path, stamp: FileTime) {
        filetime::set_file_mtime(path, stamp).expect("restore mtime");
    }

    #[test]
    fn cache_serves_same_stamp_and_config_write_evicts() {
        let td = tempfile::tempdir().expect("tempdir");
        let gd = td.path();
        let cfg = gd.join("config");
        fs::write(&cfg, "[gritcachetest]\n\tvalue = aaa\n").expect("write v1");
        let t0 = mtime_of(&cfg);
        assert_eq!(load_value(gd).as_deref(), Some("aaa"));

        // Same size + restored mtime: indistinguishable by stat, so the cache
        // serves the old parse. (This is the window `ConfigFile::write`
        // eviction closes for in-process writers; C git would not re-read at
        // all.) This assertion is what proves the cache is actually used.
        fs::write(&cfg, "[gritcachetest]\n\tvalue = bbb\n").expect("write v2");
        restore_mtime(&cfg, t0);
        assert_eq!(load_value(gd).as_deref(), Some("aaa"));

        // An in-process `ConfigFile::write` evicts even with identical stamps.
        let mut file = ConfigFile::from_path(&cfg, ConfigScope::Local)
            .expect("read config")
            .expect("config exists");
        file.set("gritcachetest.value", "ccc").expect("set value");
        file.write().expect("persist");
        restore_mtime(&cfg, t0);
        assert_eq!(load_value(gd).as_deref(), Some("ccc"));
    }

    #[test]
    fn cache_invalidates_on_size_or_existence_change() {
        let td = tempfile::tempdir().expect("tempdir");
        let gd = td.path();
        let cfg = gd.join("config");
        // Absent local config is cached as "absent"...
        assert_eq!(load_value(gd), None);
        // ...and creating the file is a stamp change (None -> Some).
        fs::write(&cfg, "[gritcachetest]\n\tvalue = first\n").expect("create");
        assert_eq!(load_value(gd).as_deref(), Some("first"));
        // A size change invalidates even with a restored mtime.
        let t0 = mtime_of(&cfg);
        fs::write(&cfg, "[gritcachetest]\n\tvalue = second-longer\n").expect("rewrite");
        restore_mtime(&cfg, t0);
        assert_eq!(load_value(gd).as_deref(), Some("second-longer"));
    }

    #[test]
    fn include_targets_are_stamped_and_invalidate() {
        let td = tempfile::tempdir().expect("tempdir");
        let gd = td.path();
        fs::write(gd.join("config"), "[include]\n\tpath = extra.conf\n").expect("write parent");
        let inc = gd.join("extra.conf");
        fs::write(&inc, "[gritcachetest]\n\tvalue = one\n").expect("write include v1");
        assert_eq!(load_value(gd).as_deref(), Some("one"));

        // Same-size + restored-mtime rewrite of only the included file: the
        // parse result is served from cache (proving the include target is
        // stamped rather than re-read)...
        let t0 = mtime_of(&inc);
        fs::write(&inc, "[gritcachetest]\n\tvalue = two\n").expect("write include v2");
        restore_mtime(&inc, t0);
        assert_eq!(load_value(gd).as_deref(), Some("one"));

        // ...while a stat-visible change to the included file invalidates.
        fs::write(&inc, "[gritcachetest]\n\tvalue = two-longer\n").expect("write include v3");
        assert_eq!(load_value(gd).as_deref(), Some("two-longer"));
    }

    #[test]
    fn missing_include_target_is_watched() {
        let td = tempfile::tempdir().expect("tempdir");
        let gd = td.path();
        fs::write(gd.join("config"), "[include]\n\tpath = extra.conf\n").expect("write parent");
        assert_eq!(load_value(gd), None);

        // The resolved-but-missing target was stamped as absent; creating it
        // must invalidate the cached parse.
        fs::write(gd.join("extra.conf"), "[gritcachetest]\n\tvalue = born\n").expect("create");
        assert_eq!(load_value(gd).as_deref(), Some("born"));
    }

    #[test]
    fn onbranch_condition_follows_head() {
        let td = tempfile::tempdir().expect("tempdir");
        let gd = td.path();
        fs::write(gd.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        fs::write(
            gd.join("config"),
            "[includeIf \"onbranch:main\"]\n\tpath = branch.conf\n",
        )
        .expect("write config");
        fs::write(
            gd.join("branch.conf"),
            "[gritcachetest]\n\tvalue = onmain\n",
        )
        .expect("write branch config");
        assert_eq!(load_value(gd).as_deref(), Some("onmain"));

        // Switching branches rewrites HEAD; the stamped HEAD must invalidate
        // the cached cascade so the condition is re-evaluated.
        fs::write(gd.join("HEAD"), "ref: refs/heads/dev\n").expect("rewrite HEAD");
        assert_eq!(load_value(gd), None);
    }
}
