//! Git-compatible pathspec matching (magic tokens and global flags).
//!
//! Global flags are read from the same environment variables as Git:
//! `GIT_LITERAL_PATHSPECS`, `GIT_GLOB_PATHSPECS`, `GIT_NOGLOB_PATHSPECS`,
//! `GIT_ICASE_PATHSPECS`. The `grit` binary sets these from CLI flags such as
//! `--literal-pathspecs` before dispatching subcommands.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::crlf::path_gitattribute_value;
use crate::crlf::AttrRule;
use crate::error::{Error, Result as LibResult};
use crate::precompose_config::pathspec_precompose_enabled;
use crate::unicode_normalization::precompose_utf8_path;
use crate::wildmatch::{wildmatch, WM_CASEFOLD, WM_PATHNAME};

/// Returns the length of the leading literal segment before the first glob metacharacter,
/// matching Git's `simple_length()` (`*` `?` `[` `\`) on bytes.
#[must_use]
pub fn simple_length(match_str: &str) -> usize {
    let b = match_str.as_bytes();
    let mut len = 0usize;
    for &c in b {
        if matches!(c, b'*' | b'?' | b'[' | b'\\') {
            break;
        }
        len += 1;
    }
    len
}

/// Whether the pattern uses wildcards after Git pathspec escaping rules.
#[must_use]
pub fn has_glob_chars(s: &str) -> bool {
    simple_length(s) < s.len()
}

/// Read pathspec entries from raw file bytes (stdin or file), matching Git's
/// `--pathspec-from-file` / `--pathspec-file-nul` rules.
///
/// * **NUL mode:** entries are separated by `NUL`; each segment must not use
///   C-style quoted lines (Git rejects quoted pathspecs in this mode).
/// * **Line mode:** entries are separated by `LF`; optional `CR` before `LF`
///   is stripped; optional trailing line without a final newline is included;
///   double-quoted lines are C-unquoted (including octal escapes).
pub fn parse_pathspecs_from_source(data: &[u8], nul_terminated: bool) -> LibResult<Vec<String>> {
    if nul_terminated {
        let mut out = Vec::new();
        for chunk in data.split(|b| *b == 0) {
            if chunk.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(chunk);
            let t = s.trim();
            if t.starts_with('"') {
                return Err(Error::PathError(format!(
                    "pathspec-from-file: line is not NUL terminated: {t}"
                )));
            }
            out.push(t.to_string());
        }
        return Ok(out);
    }

    let text = String::from_utf8_lossy(data);
    let mut out = Vec::new();
    for raw in text.split_inclusive('\n') {
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line.starts_with('"') && line.ends_with('"') && line.len() >= 2 {
            out.push(unquote_c_style_pathspec_line(line)?);
        } else {
            out.push(line.to_string());
        }
    }
    Ok(out)
}

/// Unquote a single `--pathspec-from-file` line that is wrapped in double quotes.
fn unquote_c_style_pathspec_line(s: &str) -> LibResult<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') || bytes.len() < 2 {
        return Err(Error::PathError(format!("invalid C-style quoting: {s}")));
    }

    let inner = &bytes[1..bytes.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' {
            out.push(inner[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= inner.len() {
            return Err(Error::PathError(
                "invalid escape at end of string".to_string(),
            ));
        }
        match inner[i] {
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            c if c.is_ascii_digit() => {
                if i + 2 >= inner.len() {
                    return Err(Error::PathError("truncated octal escape".to_string()));
                }
                let oct = std::str::from_utf8(&inner[i..i + 3])
                    .map_err(|_| Error::PathError("invalid octal bytes".to_string()))?;
                out.push(
                    u8::from_str_radix(oct, 8)
                        .map_err(|_| Error::PathError("invalid octal escape value".to_string()))?,
                );
                i += 2;
            }
            other => {
                return Err(Error::PathError(format!(
                    "invalid escape sequence \\{}",
                    char::from(other)
                )));
            }
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| Error::PathError("invalid UTF-8 in quoted pathspec".into()))
}

#[derive(Debug, Clone, Default)]
struct PathspecMagic {
    literal: bool,
    glob: bool,
    icase: bool,
    exclude: bool,
    /// `:(top)` / short `:/` — paths are relative to repo root.
    top: bool,
    prefix: Option<String>,
    /// `:(attr:...)` requirements.
    attr_requirements: Vec<AttrRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttrRequirement {
    Set(String),
    Unset(String),
    Unspecified(String),
    Value(String, String),
}

impl AttrRequirement {
    fn name(&self) -> &str {
        match self {
            AttrRequirement::Set(name)
            | AttrRequirement::Unset(name)
            | AttrRequirement::Unspecified(name)
            | AttrRequirement::Value(name, _) => name,
        }
    }
}

fn parse_maybe_bool(v: &str) -> Option<bool> {
    let s = v.trim().to_ascii_lowercase();
    match s.as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn git_env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => parse_maybe_bool(&v).unwrap_or(default),
        Err(_) => default,
    }
}

fn literal_global() -> bool {
    git_env_bool("GIT_LITERAL_PATHSPECS", false)
}

/// Whether `GIT_LITERAL_PATHSPECS` is enabled (shell `*` and `?` are literal, not globs).
#[must_use]
pub fn literal_pathspecs_enabled() -> bool {
    literal_global()
}

fn glob_global() -> bool {
    git_env_bool("GIT_GLOB_PATHSPECS", false)
}

fn noglob_global() -> bool {
    git_env_bool("GIT_NOGLOB_PATHSPECS", false)
}

fn icase_global() -> bool {
    git_env_bool("GIT_ICASE_PATHSPECS", false)
}

/// Validates global pathspec environment flags the same way Git does.
///
/// Returns an error message suitable for `bail!` when flags are incompatible.
pub fn validate_global_pathspec_flags() -> Result<(), String> {
    let lit = literal_global();
    let glob = glob_global();
    let noglob = noglob_global();
    let icase = icase_global();

    if glob && noglob {
        return Err("global 'glob' and 'noglob' pathspec settings are incompatible".to_string());
    }
    if lit && (glob || noglob || icase) {
        return Err(
            "global 'literal' pathspec setting is incompatible with all other global pathspec settings"
                .to_string(),
        );
    }
    Ok(())
}

fn is_valid_attr_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn split_attr_expr(expr: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_value = false;
    let mut escaped = false;

    for ch in expr.chars() {
        if escaped {
            if ch.is_ascii_whitespace() {
                return Err(
                    "Escape character '\\' not allowed as last character in attr value".to_string(),
                );
            }
            if ch != ',' {
                return Err("Escape character '\\' not allowed for value matching".to_string());
            }
            cur.push(ch);
            escaped = false;
            continue;
        }
        if in_value && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '=' {
            in_value = true;
            cur.push(ch);
            continue;
        }
        if ch.is_ascii_whitespace() {
            if !cur.is_empty() {
                parts.push(cur);
                cur = String::new();
            }
            in_value = false;
            continue;
        }
        cur.push(ch);
    }

    if escaped {
        return Err(
            "Escape character '\\' not allowed as last character in attr value".to_string(),
        );
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    Ok(parts)
}

fn parse_attr_requirements(expr: &str) -> Result<Vec<AttrRequirement>, String> {
    if expr.trim().is_empty() {
        return Err("empty attr magic is invalid".to_string());
    }
    let mut out = Vec::new();
    for token in split_attr_expr(expr)? {
        if let Some(name) = token.strip_prefix('-') {
            if name.contains('=') {
                return Err("invalid attribute name".to_string());
            }
            if !is_valid_attr_name(name) {
                return Err(format!("{name} is not a valid attribute name"));
            }
            out.push(AttrRequirement::Unset(name.to_string()));
        } else if let Some(name) = token.strip_prefix('!') {
            if name.contains('=') {
                return Err("invalid attribute name".to_string());
            }
            if !is_valid_attr_name(name) {
                return Err(format!("{name} is not a valid attribute name"));
            }
            out.push(AttrRequirement::Unspecified(name.to_string()));
        } else if let Some((name, value)) = token.split_once('=') {
            if !is_valid_attr_name(name) {
                return Err(format!("{name} is not a valid attribute name"));
            }
            if value.is_empty() {
                return Err("empty attribute value is not allowed".to_string());
            }
            out.push(AttrRequirement::Value(name.to_string(), value.to_string()));
        } else {
            if !is_valid_attr_name(&token) {
                return Err(format!("{token} is not a valid attribute name"));
            }
            out.push(AttrRequirement::Set(token));
        }
    }
    if out.is_empty() {
        return Err("empty attr magic is invalid".to_string());
    }
    Ok(out)
}

/// Validate `:(attr:...)` pathspec magic in `specs`.
///
/// Returns `Ok(())` when all attribute magic is parseable. Returns a Git-style error string for
/// unsupported or malformed attribute magic.
pub fn validate_attr_pathspecs(specs: &[String]) -> Result<(), String> {
    for spec in specs {
        if literal_global() || !spec.starts_with(":(") {
            continue;
        }
        let Some(rest) = spec.strip_prefix(":(") else {
            continue;
        };
        let Some(close) = rest.find(')') else {
            continue;
        };
        let magic_part = &rest[..close];
        let mut attr_count = 0usize;
        for token in split_long_magic_tokens(magic_part) {
            let Some(expr) = token.trim().strip_prefix("attr:") else {
                continue;
            };
            attr_count += 1;
            if attr_count > 1 {
                return Err("Only one 'attr:' specification is allowed.".to_string());
            }
            parse_attr_requirements(expr)?;
        }
    }
    Ok(())
}

fn split_long_magic_tokens(magic_part: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for ch in magic_part.chars() {
        if escaped {
            cur.push('\\');
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == ',' {
            tokens.push(cur.trim().to_string());
            cur.clear();
            continue;
        }
        cur.push(ch);
    }
    if escaped {
        cur.push('\\');
    }
    tokens.push(cur.trim().to_string());
    tokens
}

fn parse_long_magic(rest_after_paren: &str) -> Option<(PathspecMagic, &str)> {
    let close = rest_after_paren.find(')')?;
    let magic_part = &rest_after_paren[..close];
    let tail = &rest_after_paren[close + 1..];
    let mut magic = PathspecMagic::default();
    for raw in split_long_magic_tokens(magic_part) {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(p) = token.strip_prefix("prefix:") {
            magic.prefix = Some(p.to_string());
            continue;
        }
        if let Some(expr) = token.strip_prefix("attr:") {
            if let Ok(reqs) = parse_attr_requirements(expr) {
                magic.attr_requirements = reqs;
            }
            continue;
        }
        if token.eq_ignore_ascii_case("literal") {
            magic.literal = true;
        } else if token.eq_ignore_ascii_case("glob") {
            magic.glob = true;
        } else if token.eq_ignore_ascii_case("icase") {
            magic.icase = true;
        } else if token.eq_ignore_ascii_case("exclude") {
            magic.exclude = true;
        } else if token.eq_ignore_ascii_case("top") {
            magic.top = true;
        }
    }
    Some((magic, tail))
}

/// `elem` is the full pathspec beginning with `:` (short magic form, not `:(...)`).
fn parse_short_magic(elem: &str) -> (PathspecMagic, &str) {
    let bytes = elem.as_bytes();
    let mut i = 1usize;
    let mut magic = PathspecMagic::default();
    while i < bytes.len() && bytes[i] != b':' {
        let ch = bytes[i];
        if ch == b'^' {
            magic.exclude = true;
            i += 1;
            continue;
        }
        let is_magic = match ch {
            b'!' => {
                magic.exclude = true;
                true
            }
            b'/' => {
                magic.top = true;
                true
            } // short `:/` = top
            _ => false,
        };
        if is_magic {
            i += 1;
            continue;
        }
        break;
    }
    if i < bytes.len() && bytes[i] == b':' {
        i += 1;
    }
    (magic, &elem[i..])
}

/// Strip `:(magic)` / `:magic` prefix when not in literal-global mode.
fn parse_element_magic(elem: &str) -> (PathspecMagic, &str) {
    if !elem.starts_with(':') || literal_global() {
        return (PathspecMagic::default(), elem);
    }
    if let Some(rest) = elem.strip_prefix(":(") {
        return parse_long_magic(rest).unwrap_or((PathspecMagic::default(), elem));
    }
    parse_short_magic(elem)
}

fn combine_magic(element: PathspecMagic) -> PathspecMagic {
    let mut m = element;
    if literal_global() {
        m.literal = true;
    }
    if glob_global() && !m.literal {
        m.glob = true;
    }
    if icase_global() {
        m.icase = true;
    }
    if noglob_global() && !m.glob {
        m.literal = true;
    }
    m
}

fn strip_top_magic(mut pattern: &str) -> &str {
    if let Some(r) = pattern.strip_prefix(":/") {
        pattern = r;
    }
    pattern
}

/// Path prefix used for Bloom-filter lookups (`revision.c` `convert_pathspec_to_bloom_keyvec`).
///
/// `cwd_from_repo_root` is the path from the repository work tree to the process cwd, using `/`
/// separators and no leading slash (empty string at repo root). Used for `:(top)` / `:/`.
#[must_use]
pub fn bloom_lookup_prefix_with_cwd(
    spec: &str,
    cwd_from_repo_root: Option<&str>,
) -> Option<String> {
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let magic = combine_magic(elem_magic);
    if magic.exclude || magic.icase {
        return None;
    }
    let pattern = strip_top_magic(raw_pattern);
    if pattern.is_empty() {
        return None;
    }
    let combined = if magic.top {
        let cwd = cwd_from_repo_root.unwrap_or("").trim_end_matches('/');
        if cwd.is_empty() {
            pattern.to_string()
        } else {
            format!("{cwd}/{pattern}")
        }
    } else {
        pattern.to_string()
    };
    let pattern = combined.as_str();
    let mut len = simple_length(pattern);
    if len != pattern.len() {
        while len > 0 && pattern.as_bytes()[len - 1] != b'/' {
            len -= 1;
        }
    }
    while len > 0 && pattern.as_bytes()[len - 1] == b'/' {
        len -= 1;
    }
    if len == 0 {
        return None;
    }
    Some(combined[..len].to_string())
}

#[must_use]
pub fn bloom_lookup_prefix(spec: &str) -> Option<String> {
    bloom_lookup_prefix_with_cwd(spec, None)
}

/// Whether every pathspec can participate in Bloom precomputation (Git `forbid_bloom_filters`).
#[must_use]
pub fn pathspecs_allow_bloom(specs: &[String]) -> bool {
    specs.iter().all(|s| {
        !s.is_empty() && !pathspec_is_exclude(s) && bloom_lookup_prefix_with_cwd(s, None).is_some()
    })
}

/// Whether `path` is included when Git applies a pathspec list with optional `:(exclude)` entries.
///
/// A path is rejected if any exclude pathspec matches it. When at least one non-exclude pathspec is
/// present, the path must also match one of those positives (`OR` semantics).
#[must_use]
pub fn path_allowed_by_pathspec_list(specs: &[String], path: &str) -> bool {
    let mut has_positive = false;
    let mut positive_match = false;
    for s in specs {
        let (elem, raw_pattern) = parse_element_magic(s);
        let magic = combine_magic(elem);
        if magic.exclude {
            if path_matches_pathspec_tail(raw_pattern, path, magic) {
                return false;
            }
            continue;
        }
        has_positive = true;
        if pathspec_matches(s, path) {
            positive_match = true;
        }
    }
    !has_positive || positive_match
}

/// True when `spec` matches `path` for pathspec bookkeeping (positive match or exclude hit).
#[must_use]
pub fn pathspec_contributes_match(spec: &str, path: &str) -> bool {
    pathspec_matches(spec, path) || pathspec_exclude_matches(spec, path)
}

fn path_matches_pathspec_tail(raw_pattern: &str, path: &str, magic: PathspecMagic) -> bool {
    if magic.literal && magic.glob {
        return false;
    }
    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };
    pathspec_matches_tail(pattern, path_for_match, magic)
}

/// True if `path` is matched by `spec` (Git pathspec syntax, including magic and globals).
///
/// Same as [`matches_pathspec`] (default file context; exclude specs never match positively here).
/// See [`matches_pathspec_list`].
#[must_use]
pub fn pathspec_matches(spec: &str, path: &str) -> bool {
    matches_pathspec(spec, path)
}

/// Returns whether `spec` uses Git's exclude magic (`:(exclude)`, `:!`, `:^`, etc.).
#[must_use]
pub fn pathspec_is_exclude(spec: &str) -> bool {
    let (elem_magic, _) = parse_element_magic(spec);
    combine_magic(elem_magic).exclude
}

/// Whether tree-walking should recurse into directory `full_name` for pathspec `spec` without
/// `-r` (Git `read_tree` / `show_recursive` “interesting” descent).
///
/// Exclude-only patterns never trigger descent alone.
#[must_use]
pub fn pathspec_wants_descent_into_tree(spec: &str, full_name: &str) -> bool {
    if pathspec_is_exclude(spec) {
        return false;
    }
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let magic = combine_magic(elem_magic);
    if magic.exclude {
        return false;
    }
    let pattern = strip_top_magic(raw_pattern);
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    if pattern.is_empty() || pattern == "." {
        return true;
    }
    let dir_prefix = format!("{full_name}/");
    if pattern.starts_with(&dir_prefix) {
        return true;
    }
    let probe = format!("{full_name}/.__grit_ls_tree_probe__");
    matches_ls_tree_pathspec(spec, &probe, 0o100644, &[])
}

/// Like [`matches_pathspec_set_for_object`], but uses [`matches_ls_tree_pathspec`] for each
/// element so `ls-files` / index filtering agrees with `ls-tree` on patterns such as `a[a]`.
#[must_use]
pub fn matches_pathspec_set_for_object_ls_tree(
    specs: &[String],
    path: &str,
    mode: u32,
    attr_rules: &[AttrRule],
) -> bool {
    if specs.is_empty() {
        return true;
    }
    let mut positives: Vec<&str> = Vec::new();
    let mut excludes: Vec<&str> = Vec::new();
    for s in specs {
        if pathspec_is_exclude(s) {
            excludes.push(s.as_str());
        } else {
            positives.push(s.as_str());
        }
    }
    let positive_ok = if positives.is_empty() {
        true
    } else {
        positives
            .iter()
            .any(|s| matches_ls_tree_pathspec(s, path, mode, attr_rules))
    };
    if !positive_ok {
        return false;
    }
    for ex in excludes {
        if matches_ls_tree_pathspec(ex, path, mode, attr_rules) {
            return false;
        }
    }
    true
}

/// True if `path` matches the combined pathspec list: any positive spec (or all paths when there
/// are only excludes, matching Git `parse_pathspec`), and not matched by any exclude spec.
#[must_use]
pub fn matches_pathspec_set_for_object(
    specs: &[String],
    path: &str,
    mode: u32,
    attr_rules: &[AttrRule],
) -> bool {
    if specs.is_empty() {
        return true;
    }
    let mut positives: Vec<&str> = Vec::new();
    let mut excludes: Vec<&str> = Vec::new();
    for s in specs {
        if pathspec_is_exclude(s) {
            excludes.push(s.as_str());
        } else {
            positives.push(s.as_str());
        }
    }
    let positive_ok = if positives.is_empty() {
        true
    } else {
        positives
            .iter()
            .any(|s| matches_pathspec_for_object(s, path, mode, attr_rules))
    };
    if !positive_ok {
        return false;
    }
    for ex in excludes {
        if matches_pathspec_for_object(ex, path, mode, attr_rules) {
            return false;
        }
    }
    true
}

/// True if `spec` uses `:(top)` or short `:/` (repo-root-relative) magic.
#[must_use]
pub fn pathspec_has_top(spec: &str) -> bool {
    let (elem_magic, _) = parse_element_magic(spec);
    combine_magic(elem_magic).top
}

fn pathspec_match_one_positive(path: &str, magic: PathspecMagic, raw_pattern: &str) -> bool {
    if magic.literal && magic.glob {
        return false;
    }
    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };
    pathspec_matches_tail(pattern, path_for_match, magic)
}

fn attr_requirements_match(
    requirements: &[AttrRequirement],
    attr_rules: &[AttrRule],
    path: &str,
    is_dir: bool,
    mode: u32,
) -> bool {
    requirements.iter().all(|req| {
        let value = if req.name() == "builtin_objectmode" {
            if mode == 0 {
                None
            } else {
                Some(format!("{mode:06o}"))
            }
        } else {
            path_gitattribute_value(attr_rules, path, is_dir, req.name())
        };
        match req {
            AttrRequirement::Set(_) => value.as_deref() == Some("set"),
            AttrRequirement::Unset(_) => value.as_deref() == Some("unset"),
            AttrRequirement::Unspecified(_) => value.is_none(),
            AttrRequirement::Value(_, expected) => value.as_deref() == Some(expected.as_str()),
        }
    })
}

fn matches_pathspec_element_with_context(
    spec: &str,
    path: &str,
    ctx: PathspecMatchContext,
) -> bool {
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let magic = combine_magic(elem_magic);
    if magic.exclude {
        return false;
    }
    if magic.literal && magic.glob {
        return false;
    }
    if !magic.attr_requirements.is_empty() {
        return false;
    }
    if magic.literal || magic.glob || magic.icase {
        return pathspec_matches(spec, path);
    }
    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };
    matches_pathspec_with_context(pattern, path_for_match, ctx)
}

fn pathspec_exclude_element_matches_with_context(
    spec: &str,
    path: &str,
    ctx: PathspecMatchContext,
) -> bool {
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let mut magic = combine_magic(elem_magic);
    if !magic.exclude {
        return false;
    }
    magic.exclude = false;
    if magic.literal && magic.glob {
        return false;
    }
    if !magic.attr_requirements.is_empty() {
        // Attribute pathspecs need `.gitattributes` context; use
        // [`matches_pathspec_list_for_object`] for those.
        return false;
    }
    if magic.literal || magic.glob || magic.icase {
        return pathspec_match_one_positive(path, magic, raw_pattern);
    }
    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };
    matches_pathspec_with_context(pattern, path_for_match, ctx)
}

/// True if `path` is matched by an exclude pathspec's pattern. Returns `false` if `spec` is not
/// an exclude pathspec.
#[must_use]
pub fn pathspec_exclude_matches(spec: &str, path: &str) -> bool {
    pathspec_exclude_element_matches_with_context(spec, path, PathspecMatchContext::default())
}

/// When every pathspec is an exclude and none use `:(top)` / `:/`, Git prepends an implicit
/// positive that matches only under the process cwd (relative to the work tree), not the whole
/// repository (`PATHSPEC_PREFER_CWD` in `pathspec.c`). `cwd_from_repo_root` is that prefix
/// without a trailing slash, or empty at the work tree root.
#[must_use]
pub fn extend_pathspec_list_implicit_cwd(
    specs: &[String],
    cwd_from_repo_root: Option<&str>,
) -> Vec<String> {
    if specs.is_empty() {
        return specs.to_vec();
    }
    if !specs.iter().all(|s| pathspec_is_exclude(s)) {
        return specs.to_vec();
    }
    let any_top = specs.iter().any(|s| pathspec_has_top(s));
    if any_top {
        return specs.to_vec();
    }
    let Some(cwd) = cwd_from_repo_root.map(str::trim).filter(|s| !s.is_empty()) else {
        return specs.to_vec();
    };
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return specs.to_vec();
    }
    let mut out = Vec::with_capacity(specs.len() + 1);
    out.push(format!("{cwd}/"));
    out.extend_from_slice(specs);
    out
}

/// Git `match_pathspec` semantics over a pathspec list: OR of positive specs minus OR of exclude
/// specs. If every element is exclude-only, Git implicitly prepends `.` (match all); this
/// function does the same.
#[must_use]
pub fn matches_pathspec_list(path: &str, specs: &[String]) -> bool {
    matches_pathspec_list_with_context(path, specs, PathspecMatchContext::default())
}

/// Like [`matches_pathspec_list`], but uses `ctx` for non-magic pathspec elements (trailing `/`).
#[must_use]
pub fn matches_pathspec_list_with_context(
    path: &str,
    specs: &[String],
    ctx: PathspecMatchContext,
) -> bool {
    if specs.is_empty() {
        return true;
    }
    let has_exclude = specs.iter().any(|s| pathspec_is_exclude(s));
    let positive_specs: Vec<&String> = specs.iter().filter(|s| !pathspec_is_exclude(s)).collect();
    let positive = if positive_specs.is_empty() {
        true
    } else {
        positive_specs
            .iter()
            .any(|s| matches_pathspec_element_with_context(s, path, ctx))
    };
    if !positive {
        return false;
    }
    if !has_exclude {
        return true;
    }
    let excluded = specs.iter().any(|s| {
        pathspec_is_exclude(s) && pathspec_exclude_element_matches_with_context(s, path, ctx)
    });
    !excluded
}

/// `matches_pathspec_list` for tree/index objects with mode and `.gitattributes` rules.
#[must_use]
pub fn matches_pathspec_list_for_object(
    path: &str,
    mode: u32,
    attr_rules: &[AttrRule],
    specs: &[String],
) -> bool {
    if specs.is_empty() {
        return true;
    }
    let has_exclude = specs.iter().any(|s| pathspec_is_exclude(s));
    let positive_specs: Vec<&String> = specs.iter().filter(|s| !pathspec_is_exclude(s)).collect();
    let positive = if positive_specs.is_empty() {
        true
    } else {
        positive_specs
            .iter()
            .any(|s| matches_pathspec_for_object(s, path, mode, attr_rules))
    };
    if !positive {
        return false;
    }
    if !has_exclude {
        return true;
    }
    let excluded = specs.iter().any(|s| {
        pathspec_is_exclude(s) && matches_pathspec_exclude_for_object(s, path, mode, attr_rules)
    });
    !excluded
}

fn matches_pathspec_exclude_for_object(
    spec: &str,
    path: &str,
    mode: u32,
    attr_rules: &[AttrRule],
) -> bool {
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let mut magic = combine_magic(elem_magic);
    if !magic.exclude {
        return false;
    }
    magic.exclude = false;
    if magic.literal && magic.glob {
        return false;
    }
    let ctx = context_from_mode_bits(mode);
    let is_dir_for_attr = path.ends_with('/') || ctx.is_directory || ctx.is_git_submodule;
    if !magic.attr_requirements.is_empty()
        && !attr_requirements_match(
            &magic.attr_requirements,
            attr_rules,
            path,
            is_dir_for_attr,
            mode,
        )
    {
        return false;
    }
    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };
    if magic.literal || magic.glob || magic.icase {
        pathspec_matches_tail(pattern, path_for_match, magic)
    } else {
        matches_pathspec_with_context(pattern, path_for_match, ctx)
    }
}

fn pathspec_matches_tail(pattern: &str, path: &str, magic: PathspecMagic) -> bool {
    if pattern.is_empty() {
        return true;
    }

    let flags = if magic.icase { WM_CASEFOLD } else { 0 };

    if magic.literal {
        return literal_prefix_match(pattern, path);
    }

    let wm_flags = if magic.glob {
        flags | WM_PATHNAME
    } else {
        flags
    };

    let pattern_bytes = pattern.as_bytes();
    let path_bytes = path.as_bytes();
    let simple = simple_length(pattern);

    // Git `match_pathspec_item`: exact / directory prefix before `git_fnmatch`.
    // Only when the pattern has no glob metacharacters (`simple_length` spans the whole pattern);
    // otherwise a pattern like `a[a]` must not match children via `a[a]/` prefix (t6130 vs ls-tree).
    if ps_str_eq(pattern, path, magic.icase) {
        return true;
    }
    if simple == pattern.len() {
        if let Some(prefix) = pattern.strip_suffix('/') {
            if ps_str_eq(prefix, path, magic.icase) {
                return true;
            }
            let prefix_slash = format!("{prefix}/");
            if path_starts_with(path, &prefix_slash, magic.icase) {
                return true;
            }
        } else {
            let prefix_slash = format!("{pattern}/");
            if path_starts_with(path, &prefix_slash, magic.icase) {
                return true;
            }
        }
    }

    // `:(glob)**/*.txt` at repo root: Git matches `untracked.txt` (leading `**/` is optional).
    if magic.glob && !path.contains('/') && pattern.starts_with("**/") {
        if wildmatch(pattern_bytes, path_bytes, wm_flags) {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix("**/") {
            if wildmatch(suffix.as_bytes(), path_bytes, wm_flags) {
                return true;
            }
        }
    }

    // Wildcard: require literal bytes up to `simple_length`, then wildmatch the tail only.
    if simple < pattern.len() {
        if path_bytes.len() < simple {
            return false;
        }
        let path_lit = &path_bytes[..simple];
        let pat_lit = &pattern_bytes[..simple];
        let same = if magic.icase {
            path_lit.eq_ignore_ascii_case(pat_lit)
        } else {
            path_lit == pat_lit
        };
        if !same {
            return false;
        }
        let pat_rest = &pattern[simple..];
        let path_rest = &path[simple..];
        return wildmatch(pat_rest.as_bytes(), path_rest.as_bytes(), wm_flags);
    }

    ps_str_eq(pattern, path, magic.icase)
        || path_starts_with(path, &format!("{pattern}/"), magic.icase)
}

fn ps_str_eq(a: &str, b: &str, icase: bool) -> bool {
    if icase {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn path_starts_with(path: &str, prefix: &str, icase: bool) -> bool {
    if icase {
        path.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    } else {
        path.starts_with(prefix)
    }
}

fn literal_prefix_match(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('/') {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    path == pattern || path.starts_with(&format!("{pattern}/"))
}

/// Literal pathspec match for `ls-tree` when the pattern has no `*`/`?` (brackets stay literal).
fn ls_tree_literal_match(pattern: &str, path: &str, ctx: PathspecMatchContext) -> bool {
    if let Some(prefix) = pattern.strip_suffix('/') {
        if path.starts_with(&format!("{prefix}/")) {
            return true;
        }
        if path == prefix {
            return ctx.is_directory || ctx.is_git_submodule;
        }
        return false;
    }
    path == pattern || path.starts_with(&format!("{pattern}/"))
}

/// Optional path metadata for literal pathspecs with a trailing `/` (tree-walk / diff-tree).
///
/// Git treats `dir/` as “directory or git submodule only”: a regular file `dir`
/// does not match, but a tree entry `dir` or gitlink `dir` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathspecMatchContext {
    /// The index/tree entry is a directory (mode `040000`).
    pub is_directory: bool,
    /// The entry is a git submodule / gitlink (`160000`).
    pub is_git_submodule: bool,
}

/// Returns whether `path` matches the pathspec `spec` with default (file) context.
///
/// For pathspecs ending in `/`, a path equal to the prefix matches only when
/// [`PathspecMatchContext`] indicates a directory or submodule; see
/// [`matches_pathspec_with_context`].
#[must_use]
pub fn matches_pathspec(spec: &str, path: &str) -> bool {
    matches_pathspec_with_context(spec, path, PathspecMatchContext::default())
}

/// Like [`matches_pathspec`], but uses `ctx` for trailing-`/` literal pathspecs and for
/// wildcard pathspecs where the pattern continues after a directory boundary (Git
/// `matches_pathspec` + directory semantics).
#[must_use]
pub fn matches_pathspec_with_context(spec: &str, path: &str, ctx: PathspecMatchContext) -> bool {
    let spec_nfc: Cow<'_, str> = if pathspec_precompose_enabled() {
        precompose_utf8_path(spec)
    } else {
        Cow::Borrowed(spec)
    };
    let path_nfc: Cow<'_, str> = if pathspec_precompose_enabled() {
        precompose_utf8_path(path)
    } else {
        Cow::Borrowed(path)
    };
    let spec = spec_nfc.as_ref();
    let path = path_nfc.as_ref();

    let trimmed = spec.strip_prefix("./").unwrap_or(spec);
    if trimmed == "." || trimmed.is_empty() {
        return true;
    }

    let (elem_magic, raw_pattern) = parse_element_magic(trimmed);
    let magic = combine_magic(elem_magic);

    if magic.literal && magic.glob {
        return false;
    }
    if magic.exclude {
        return false;
    }

    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };

    if magic.literal {
        if let Some(prefix) = pattern.strip_suffix('/') {
            if path_for_match.starts_with(&format!("{prefix}/")) {
                return true;
            }
            if path_for_match == prefix {
                return ctx.is_directory || ctx.is_git_submodule;
            }
            return false;
        }
        return path_for_match == pattern || path_for_match.starts_with(&format!("{pattern}/"));
    }

    // No wildcards and trailing `/`: directory-only semantics (Git `matches_pathspec`).
    if let Some(prefix) = pattern.strip_suffix('/') {
        if simple_length(pattern) == pattern.len() {
            if path_for_match.starts_with(&format!("{prefix}/")) {
                return true;
            }
            if path_for_match == prefix {
                return ctx.is_directory || ctx.is_git_submodule;
            }
            return false;
        }
    }

    if pathspec_matches_tail(pattern, path_for_match, magic) {
        return true;
    }

    if (ctx.is_directory || ctx.is_git_submodule)
        && !path_for_match.is_empty()
        && pattern.len() > path_for_match.len()
        && pattern.as_bytes().get(path_for_match.len()) == Some(&b'/')
        && pattern.starts_with(path_for_match)
        && simple_length(pattern) < pattern.len()
    {
        return true;
    }

    false
}

/// Parse a Git mode string (e.g. `100644`, `040000`) into a [`PathspecMatchContext`].
#[must_use]
pub fn context_from_mode_octal(mode: &str) -> PathspecMatchContext {
    let Ok(bits) = u32::from_str_radix(mode, 8) else {
        return PathspecMatchContext::default();
    };
    context_from_mode_bits(bits)
}

/// Classify a raw Git mode (e.g. from an index or tree entry) for pathspec matching.
#[must_use]
pub fn context_from_mode_bits(mode: u32) -> PathspecMatchContext {
    let ty = mode & 0o170000;
    PathspecMatchContext {
        is_directory: ty == 0o040000,
        is_git_submodule: ty == 0o160000,
    }
}

/// Pathspec matching for `ls-tree` after Git forces `pathspec.has_wildcard = 0` (`ls-tree.c`).
///
/// Metacharacters `*` / `?` still participate in [`wildmatch`]; `[` and `\\` are **not** glob
/// starters unless a `*` or `?` appears — so `a[a]` matches the literal directory `a[a]` (t3102),
/// while `a*` matches `a/one`, `aa/two`, `a[a]/three`, …
#[must_use]
pub fn matches_ls_tree_pathspec(
    spec: &str,
    path: &str,
    mode: u32,
    attr_rules: &[AttrRule],
) -> bool {
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let mut magic = combine_magic(elem_magic);
    magic.exclude = false;

    if magic.literal && magic.glob {
        return false;
    }

    let ctx = context_from_mode_bits(mode);
    let is_dir_for_attr = path.ends_with('/') || ctx.is_directory || ctx.is_git_submodule;

    if !magic.attr_requirements.is_empty()
        && !attr_requirements_match(
            &magic.attr_requirements,
            attr_rules,
            path,
            is_dir_for_attr,
            mode,
        )
    {
        return false;
    }

    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };

    if magic.literal || magic.glob || magic.icase {
        return pathspec_matches_tail(pattern, path_for_match, magic);
    }

    let spec_nfc: Cow<'_, str> = if pathspec_precompose_enabled() {
        precompose_utf8_path(pattern)
    } else {
        Cow::Borrowed(pattern)
    };
    let path_nfc: Cow<'_, str> = if pathspec_precompose_enabled() {
        precompose_utf8_path(path_for_match)
    } else {
        Cow::Borrowed(path_for_match)
    };
    let pattern = spec_nfc.as_ref();
    let path = path_nfc.as_ref();

    let trimmed = pattern.strip_prefix("./").unwrap_or(pattern);
    if trimmed == "." || trimmed.is_empty() {
        return true;
    }

    let uses_star_or_question = trimmed.contains('*') || trimmed.contains('?');
    if !uses_star_or_question {
        return ls_tree_literal_match(trimmed, path, ctx);
    }

    let nwl = simple_length(trimmed);
    let flags = 0u32;
    if nwl == trimmed.len() {
        return wildmatch(trimmed.as_bytes(), path.as_bytes(), flags);
    }
    let lit = trimmed.as_bytes().get(..nwl).unwrap_or_default();
    let path_b = path.as_bytes();
    if path_b.len() < nwl {
        return false;
    }
    if &path_b[..nwl] != lit {
        return false;
    }
    let pat_rest = &trimmed[nwl..];
    let path_rest = &path[nwl..];
    wildmatch(pat_rest.as_bytes(), path_rest.as_bytes(), flags)
}

/// Match a pathspec against a tree path, using `.gitattributes` for `:(attr:...)`.
///
/// Used by `git archive` style tree walks: `mode` supplies directory/gitlink context for
/// literal pathspecs ending in `/`.
#[must_use]
pub fn matches_pathspec_for_object(
    spec: &str,
    path: &str,
    mode: u32,
    attr_rules: &[AttrRule],
) -> bool {
    let (elem_magic, raw_pattern) = parse_element_magic(spec);
    let mut magic = combine_magic(elem_magic);
    magic.exclude = false;

    if magic.literal && magic.glob {
        return false;
    }

    let ctx = context_from_mode_bits(mode);
    let is_dir_for_attr = path.ends_with('/') || ctx.is_directory || ctx.is_git_submodule;

    if !magic.attr_requirements.is_empty()
        && !attr_requirements_match(
            &magic.attr_requirements,
            attr_rules,
            path,
            is_dir_for_attr,
            mode,
        )
    {
        return false;
    }

    let pattern = strip_top_magic(raw_pattern);
    let path_for_match = if let Some(prefix) = magic.prefix.as_deref() {
        if !path.starts_with(prefix) {
            return false;
        }
        &path[prefix.len()..]
    } else {
        path
    };
    if magic.literal || magic.glob || magic.icase {
        pathspec_matches_tail(pattern, path_for_match, magic)
    } else {
        matches_pathspec_with_context(pattern, path_for_match, ctx)
    }
}

/// Returns wildmatch flags for `:(icase)` / `:(glob)`-style patterns when those
/// appear as explicit magic (not used by default CLI pathspecs).
#[must_use]
pub fn wildmatch_flags_icase_glob(icase: bool, glob: bool) -> u32 {
    let mut f = if glob { WM_PATHNAME } else { 0 };
    if icase {
        f |= WM_CASEFOLD;
    }
    f
}

/// Resolved path lies outside the repository work tree (Git `prefix_path_gently` failure).
#[derive(Debug, Clone)]
pub struct PathOutsideRepository {
    /// User-facing pathspec token (argv element).
    pub elt: String,
    /// Resolved absolute path outside the work tree.
    pub path: String,
    /// Canonical work tree root.
    pub work_tree: PathBuf,
}

impl std::fmt::Display for PathOutsideRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fatal: {}: '{}' is outside repository at '{}'",
            self.elt,
            self.path,
            self.work_tree.display()
        )
    }
}

/// Resolve a magic pathspec relative to a current-directory prefix.
///
/// This keeps the `cwd` prefix case-sensitive (via an internal `prefix:` magic
/// token) while still honoring magic options like `icase` for the tail.
/// Returns `None` when `spec` is not a parseable magic pathspec.
pub fn resolve_magic_pathspec(spec: &str, cwd_prefix: &str) -> Option<String> {
    if !spec.starts_with(":(") {
        return None;
    }
    let close_idx = spec.find(')')?;
    let magic_prefix = &spec[..=close_idx];
    let tail = &spec[close_idx + 1..];
    Some(resolve_magic_pathspec_parts(magic_prefix, tail, cwd_prefix))
}

fn resolve_magic_pathspec_parts(magic_prefix: &str, tail: &str, cwd_prefix: &str) -> String {
    if has_magic_prefix_token(magic_prefix) {
        return format!("{magic_prefix}{tail}");
    }

    if let Some(rooted_tail) = tail.strip_prefix('/') {
        return format!("{magic_prefix}{}", normalize_relative_path_str(rooted_tail));
    }

    let combined = if cwd_prefix.is_empty() {
        normalize_relative_path_str(tail)
    } else {
        normalize_relative_path_str(&format!("{cwd_prefix}{tail}"))
    };

    let cwd_base = normalize_relative_path_str(cwd_prefix.trim_end_matches('/'));
    if !cwd_base.is_empty()
        && (combined == cwd_base || combined.starts_with(&format!("{cwd_base}/")))
    {
        let after_base = combined
            .strip_prefix(&cwd_base)
            .unwrap_or(combined.as_str());
        let remainder = after_base.strip_prefix('/').unwrap_or(after_base);
        let magic_with_prefix = inject_magic_prefix_token(magic_prefix, &format!("{cwd_base}/"));
        return format!("{magic_with_prefix}{remainder}");
    }

    format!("{magic_prefix}{combined}")
}

fn has_magic_prefix_token(magic_prefix: &str) -> bool {
    let Some(inner) = magic_prefix
        .strip_prefix(":(")
        .and_then(|s| s.strip_suffix(')'))
    else {
        return false;
    };
    inner
        .split(',')
        .map(str::trim)
        .any(|token| token.starts_with("prefix:"))
}

fn inject_magic_prefix_token(magic_prefix: &str, prefix: &str) -> String {
    let Some(inner) = magic_prefix
        .strip_prefix(":(")
        .and_then(|s| s.strip_suffix(')'))
    else {
        return magic_prefix.to_string();
    };
    if inner.trim().is_empty() {
        format!(":(prefix:{prefix})")
    } else {
        format!(":({inner},prefix:{prefix})")
    }
}

fn normalize_relative_path_str(path: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::Normal(seg) => {
                parts.push(seg.to_string_lossy().to_string());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

/// Current directory relative to `work_tree`, or `None` if cwd is the work tree root.
#[must_use]
pub fn pathdiff(cwd: &Path, work_tree: &Path) -> Option<String> {
    let cwd_canon = cwd.canonicalize().ok()?;
    let wt_canon = work_tree.canonicalize().ok()?;

    if cwd_canon == wt_canon {
        return None;
    }

    cwd_canon
        .strip_prefix(&wt_canon)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// For exclude (and other cwd-relative) pathspec magic from a subdirectory, Git resolves the
/// pattern against the current directory (`:!sub/` from `repo/sub` → exclude `sub/sub/`).
fn prepend_cwd_to_short_exclude_pathspec(spec: &str, cwd: &str) -> Option<String> {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return None;
    }
    let bytes = spec.as_bytes();
    if bytes.first().copied() != Some(b':') {
        return None;
    }
    // `:/path` is `:(top)` short form — exclude is relative to repo root, not cwd (t6132).
    if bytes.get(1).copied() == Some(b'/') {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i] != b':' {
        let ch = bytes[i];
        if ch == b'^' {
            i += 1;
            continue;
        }
        let is_magic = matches!(ch, b'!' | b'/');
        if is_magic {
            i += 1;
            continue;
        }
        break;
    }
    if i < bytes.len() && bytes[i] == b':' {
        i += 1;
    }
    let pattern = spec.get(i..)?;
    if pattern.is_empty() || pattern.starts_with('/') {
        return None;
    }
    Some(format!("{}{}/{pattern}", &spec[..i], cwd))
}

/// Resolve a pathspec string to a path relative to the repository work tree.
///
/// `prefix` is the current directory relative to the work tree (no trailing slash),
/// or `None` when cwd is the work tree root.
#[must_use]
pub fn resolve_pathspec(pathspec: &str, work_tree: &Path, prefix: Option<&str>) -> String {
    // Git: `.` at repo root means "match the whole tree" (not an empty pathspec).
    // An empty resolved pathspec would match nothing and breaks `grep -- . t` max-depth.
    if pathspec == "." {
        return match prefix {
            Some(p) if !p.is_empty() => p.to_owned(),
            _ => ".".to_owned(),
        };
    }
    if pathspec.contains("../") || pathspec.starts_with("../") {
        let cwd = std::env::current_dir().unwrap_or_default();
        let abs = cwd.join(pathspec);
        let wt_canon = work_tree
            .canonicalize()
            .unwrap_or_else(|_| work_tree.to_path_buf());
        let mut parts: Vec<std::ffi::OsString> = Vec::new();
        for component in abs.components() {
            use std::path::Component;
            match component {
                Component::ParentDir => {
                    parts.pop();
                }
                Component::CurDir => {}
                other => parts.push(other.as_os_str().to_os_string()),
            }
        }
        let abs_norm: PathBuf = parts.iter().collect();
        if let Ok(rel) = abs_norm.strip_prefix(&wt_canon) {
            return rel.to_string_lossy().to_string();
        }
    }
    if Path::new(pathspec).is_absolute() {
        let abs = Path::new(pathspec);
        let wt_canon = work_tree
            .canonicalize()
            .unwrap_or_else(|_| work_tree.to_path_buf());
        let abs_canon = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
        if let Ok(rel) = abs_canon.strip_prefix(&wt_canon) {
            return rel.to_string_lossy().to_string();
        }
        return pathspec.to_owned();
    }

    if pathspec.starts_with(':') {
        if let Some(p) = prefix {
            if !p.is_empty() && !literal_pathspecs_enabled() {
                let cwd_ps = format!("{}/", p.trim_end_matches('/'));
                if pathspec.starts_with(":(") {
                    if let Some(resolved) = resolve_magic_pathspec(pathspec, &cwd_ps) {
                        return resolved;
                    }
                    return pathspec.to_owned();
                }
                if pathspec_is_exclude(pathspec) {
                    if let Some(fixed) = prepend_cwd_to_short_exclude_pathspec(pathspec, p) {
                        return fixed;
                    }
                }
            }
        }
        if let Some(rest) = pathspec.strip_prefix(":/") {
            // `:/!foo` / `:/^bar` — `:/` is `:(top)`; the tail is still short magic, not a literal path.
            if rest.starts_with('!') || rest.starts_with('^') {
                return pathspec.to_owned();
            }
            return rest.to_owned();
        }
        // Long magic `:(...)` must stay intact — `:(exclude)path` is not the same as `path`
        // (t6132-pathspec-exclude, grep --untracked with exclude pathspecs).
        if pathspec.starts_with(":(") {
            return pathspec.to_owned();
        }
        return pathspec.to_owned();
    }

    match prefix {
        Some(p) if !p.is_empty() => {
            normalize_relative_path_str(&PathBuf::from(p).join(pathspec).to_string_lossy())
        }
        _ => {
            // Git's `prefix_path` normalizes away leading `./` segments, so index and tree
            // paths never contain a `.` component (`git add ./-` stages `-`, not `./-`,
            // which would otherwise write an invalid `.` tree entry).
            let mut spec = pathspec;
            while let Some(rest) = spec.strip_prefix("./") {
                spec = rest.trim_start_matches('/');
            }
            if spec.is_empty() {
                ".".to_owned()
            } else {
                spec.to_owned()
            }
        }
    }
}

/// Resolve a pathspec and ensure it lies inside `work_tree` (used by `git add`, etc.).
///
/// Returns [`PathOutsideRepository`] when resolution stays absolute, matching Git's
/// `'%s' is outside repository at '%s'` fatal (t7010).
pub fn resolve_pathspec_in_worktree(
    elt: &str,
    pathspec: &str,
    work_tree: &Path,
    prefix: Option<&str>,
) -> Result<String, PathOutsideRepository> {
    let resolved = resolve_pathspec(pathspec, work_tree, prefix);
    if Path::new(&resolved).is_absolute() {
        let wt = work_tree
            .canonicalize()
            .unwrap_or_else(|_| work_tree.to_path_buf());
        return Err(PathOutsideRepository {
            elt: elt.to_string(),
            path: resolved,
            work_tree: wt,
        });
    }
    Ok(resolved)
}

/// Normalize a worktree file path for porcelain commands (`blame`, `log`, …).
///
/// Accepts repo-relative or absolute paths under `work_tree`.
#[must_use]
pub fn normalize_worktree_file_path(
    file_path: &str,
    work_tree: &Path,
    prefix: Option<&str>,
) -> String {
    let resolved = resolve_pathspec(file_path, work_tree, prefix);
    if Path::new(&resolved).is_absolute() {
        file_path.to_string()
    } else {
        resolved
    }
}

#[cfg(test)]
mod tree_entry_pathspec_tests {
    use super::*;

    #[test]
    fn t6130_bracket_filename_matches_pathspec() {
        assert!(matches_pathspec("f[o][o]", "f[o][o]"));
        assert!(matches_pathspec(":(glob)f[o][o]", "f[o][o]"));
    }

    #[test]
    fn literal_prefix_and_exact() {
        assert!(matches_pathspec("path1", "path1/file1"));
        assert!(matches_pathspec_with_context(
            "path1/",
            "path1/file1",
            PathspecMatchContext::default()
        ));
        assert!(matches_pathspec("file0", "file0"));
        assert!(!matches_pathspec("path", "path1/file1"));
    }

    #[test]
    fn ls_tree_bracket_in_name_is_literal_prefix() {
        assert!(matches_ls_tree_pathspec(
            "a[a]",
            "a[a]/three",
            0o100644,
            &[]
        ));
        assert!(!matches_pathspec_with_context(
            "a[a]",
            "a[a]/three",
            PathspecMatchContext::default()
        ));
    }

    #[test]
    fn wildcards_cross_slash_by_default() {
        assert!(matches_pathspec("f*", "file0"));
        assert!(matches_pathspec("*file1", "path1/file1"));
        assert!(matches_pathspec_with_context(
            "path1/f*",
            "path1",
            PathspecMatchContext {
                is_directory: true,
                ..Default::default()
            }
        ));
        assert!(matches_pathspec("path1/*file1", "path1/file1"));
    }

    #[test]
    fn glob_double_star_txt_at_repo_root() {
        assert!(pathspec_matches(":(glob)**/*.txt", "untracked.txt"));
        assert!(pathspec_matches(":(glob)**/*.txt", "d/untracked.txt"));
    }

    #[test]
    fn trailing_slash_directory_only() {
        assert!(!matches_pathspec_with_context(
            "file0/",
            "file0",
            PathspecMatchContext::default()
        ));
        assert!(matches_pathspec_with_context(
            "file0/",
            "file0",
            PathspecMatchContext {
                is_directory: true,
                ..Default::default()
            }
        ));
        assert!(matches_pathspec_with_context(
            "submod/",
            "submod",
            PathspecMatchContext {
                is_git_submodule: true,
                ..Default::default()
            }
        ));
    }

    #[test]
    fn exclude_top_short_magic_subtracts_from_positive() {
        let specs = vec!["*".to_string(), ":/!sub2".to_string()];
        assert!(matches_pathspec_list("sub/file", &specs));
        assert!(!matches_pathspec_list("sub2/file", &specs));
        assert!(pathspec_exclude_matches(":/!sub2", "sub2/file"));
    }
}

#[cfg(test)]
mod pathspec_list_tests {
    use super::*;
    use crate::crlf::parse_gitattributes_content;

    #[test]
    fn exclude_removes_paths_matching_icase_positive() {
        let specs = vec![
            ":(icase)*.txt".to_string(),
            ":(exclude)submodule/subsub/*".to_string(),
        ];
        assert!(path_allowed_by_pathspec_list(&specs, "submodule/g.txt"));
        assert!(!path_allowed_by_pathspec_list(
            &specs,
            "submodule/subsub/e.txt"
        ));
    }

    #[test]
    fn prefixed_attr_exclude_removes_matching_child_path() {
        let specs = vec![
            "sub".to_string(),
            ":(exclude,attr:labelB,prefix:sub/)".to_string(),
        ];
        let exclude_only = vec![":(exclude,attr:labelB,prefix:sub/)".to_string()];
        let attrs = parse_gitattributes_content("fileB labelB\n");
        assert!(!matches_pathspec_list_for_object(
            "sub/fileB",
            0o100644,
            &attrs,
            &specs,
        ));
        assert!(!matches_pathspec_list_for_object(
            "sub/fileB",
            0o100644,
            &attrs,
            &exclude_only,
        ));
    }
}
