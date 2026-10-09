//! Gitattributes parsing and pattern matching for `check-attr` and validation.
//!
//! Implements Git-consistent rule ordering, macro expansion (`[attr]`), `binary`
//! expansion, `**` globbing via [`crate::wildmatch`], and optional case folding
//! for `core.ignorecase`.

use crate::config::parse_path;
use crate::config::ConfigSet;
#[cfg(unix)]
use crate::index::normalize_mode;
use crate::index::Index;
use crate::index::MODE_EXECUTABLE;
use crate::index::MODE_GITLINK;
use crate::index::MODE_REGULAR;
use crate::index::MODE_SYMLINK;
use crate::index::MODE_TREE;
use crate::objects::parse_tree;
use crate::objects::ObjectId;
use crate::objects::ObjectKind;
use crate::odb::Odb;
use crate::repo::Repository;
use crate::rev_parse::resolve_revision;
use crate::wildmatch::{wildmatch, WM_CASEFOLD, WM_PATHNAME};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Maximum length of a single `.gitattributes` line (bytes), matching Git (`ATTR_MAX_LINE_LENGTH`).
/// Lines of this length or longer are ignored with a warning.
pub const MAX_ATTR_LINE_BYTES: usize = 2048;

/// Maximum `.gitattributes` file size (bytes) before Git ignores the file.
pub const MAX_ATTR_FILE_BYTES: usize = 100 * 1024 * 1024;

/// Parsed attribute value for display (`check-attr` output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    Set,
    /// Explicit `-attr` in a rule — `check-attr` prints `unset`.
    Unset,
    /// Macro body `!attr` — clears the attribute to *unspecified* (not `unset`).
    Clear,
    Value(String),
}

impl AttrValue {
    /// Text form as printed by `git check-attr`.
    #[must_use]
    pub fn display(&self) -> &str {
        match self {
            AttrValue::Set => "set",
            AttrValue::Unset => "unset",
            AttrValue::Clear => "unspecified",
            AttrValue::Value(v) => v.as_str(),
        }
    }
}

/// Pattern flags after Git `parse_path_pattern` (`dir.c`).
const PAT_NODIR: u32 = 1;
const PAT_MUSTBEDIR: u32 = 2;
const PAT_ENDSWITH: u32 = 4;

#[inline]
fn is_glob_special_attr(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

/// Length of initial literal segment before the first glob special (Git `simple_length`).
fn simple_length_pat(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if is_glob_special_attr(b[i]) {
            return i;
        }
        i += 1;
    }
    i
}

/// Parse pattern text like Git `parse_path_pattern` (after `!` and unquoting are handled).
fn parse_attr_pattern_fields(pat: &str) -> (String, u32, usize) {
    let mut flags = 0u32;
    let mut len = pat.len();
    if len > 0 && pat.as_bytes()[len - 1] == b'/' {
        len -= 1;
        flags |= PAT_MUSTBEDIR;
    }
    let p = &pat[..len];
    let has_slash = p.as_bytes().contains(&b'/');
    if !has_slash {
        flags |= PAT_NODIR;
    }
    if let Some(rest) = p.strip_prefix('*') {
        if !rest.is_empty() && simple_length_pat(rest) == rest.len() {
            flags |= PAT_ENDSWITH;
        }
    }
    let mut nowild = simple_length_pat(p);
    if nowild > len {
        nowild = len;
    }
    (p.to_string(), flags, nowild)
}

/// One line in a gitattributes file.
#[derive(Debug, Clone)]
pub struct AttrRule {
    /// Directory of the `.gitattributes` file that defined this rule (repo-relative, `/`,
    /// no trailing slash). Empty for the repository root file.
    pub attr_base: String,
    /// Pattern body (no leading `!`; trailing `/` stripped; same as Git after `parse_path_pattern` prep).
    pub pattern: String,
    /// From `parse_path_pattern`: basename-only match vs full path under `attr_base`.
    pub pattern_flags: u32,
    /// Length of leading literal segment before first wildcard (Git `nowildcardlen`).
    pub nowildcardlen: usize,
    /// If true, this rule was discarded (negative pattern) after emitting a warning.
    pub skip: bool,
    /// 1-based line number in the source file.
    pub line: usize,
    /// Attribute assignments in source order (last wins for duplicates on this line).
    pub attrs: Vec<(String, AttrValue)>,
}

/// Macro definitions from `[attr]name ...` lines.
#[derive(Debug, Clone, Default)]
pub struct MacroTable {
    /// Maps macro name → list of assignments (e.g. `!test` → unset test).
    pub defs: HashMap<String, Vec<(String, AttrValue)>>,
}

/// Result of parsing a gitattributes file.
#[derive(Debug, Clone, Default)]
pub struct ParsedGitAttributes {
    pub rules: Vec<AttrRule>,
    pub macros: MacroTable,
    pub warnings: Vec<String>,
}

/// Returns true if `name` is reserved (`builtin_*` except the real builtin names Git allows).
#[must_use]
pub fn is_reserved_builtin_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("builtin_") else {
        return false;
    };
    matches!(rest, "objectmode")
}

/// Validate user-defined attribute names in parsed rules (for `git add`).
///
/// Returns an error string matching Git when a rule uses an invalid `builtin_*` name.
pub fn validate_rules_for_add(
    rules: &[AttrRule],
    display_path: &str,
) -> std::result::Result<(), String> {
    for rule in rules {
        if rule.skip {
            continue;
        }
        for (name, _) in &rule.attrs {
            if name.starts_with("builtin_") && !is_reserved_builtin_name(name) {
                return Err(format!(
                    "{name} is not a valid attribute name: {display_path}:{}",
                    rule.line
                ));
            }
        }
    }
    Ok(())
}

/// Collect warnings for invalid `builtin_*` assignments (check-attr continues).
pub fn builtin_warnings_for_rules(rules: &[AttrRule], display_path: &str) -> Vec<String> {
    let mut w = Vec::new();
    for rule in rules {
        if rule.skip {
            continue;
        }
        for (name, _) in &rule.attrs {
            if name == "builtin_objectmode" {
                w.push(format!(
                    "builtin_objectmode is not a valid attribute name: {display_path}:{}",
                    rule.line
                ));
            } else if name.starts_with("builtin_") && !is_reserved_builtin_name(name) {
                w.push(format!(
                    "{name} is not a valid attribute name: {display_path}:{}",
                    rule.line
                ));
            }
        }
    }
    w
}

fn default_global_attributes_path() -> Option<PathBuf> {
    let home = crate::environment::Environment::empty().var("HOME")?;
    if let Some(xdg) = crate::environment::Environment::empty().var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("git/attributes"));
        }
    }
    Some(PathBuf::from(home).join(".config/git/attributes"))
}

pub(crate) fn global_attributes_path(
    repo: &Repository,
) -> std::result::Result<Option<PathBuf>, crate::error::Error> {
    let config = repo.config()?;
    if let Some(path) = config.get("core.attributesfile") {
        return Ok(Some(PathBuf::from(parse_path(repo.environment(), &path))));
    }
    Ok(default_global_attributes_path())
}

/// Read a `.gitattributes` path; if it is a symlink, record an error and skip (in-tree rules).
fn read_gitattributes_maybe_symlink(
    path: &Path,
    display: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        warnings.push(format!(
            "unable to access '{display}': Too many levels of symbolic links"
        ));
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Parse one gitattributes file from disk (patterns are relative to `attr_base`, the directory
/// containing the file — use `""` for the repository root file).
pub fn parse_gitattributes_file_content(content: &str, display_path: &str) -> ParsedGitAttributes {
    parse_gitattributes_content_impl(content, display_path, false, "")
}

/// Parse attributes defined in a `.gitattributes` file located in `attr_base` (repo-relative,
/// `/` separators, no trailing slash; empty string for the repository root).
pub fn parse_gitattributes_file_content_with_base(
    content: &str,
    display_path: &str,
    attr_base: &str,
) -> ParsedGitAttributes {
    parse_gitattributes_content_impl(content, display_path, false, attr_base)
}

fn preprocess_gitattributes_blob_text(content: &str) -> Cow<'_, str> {
    if !content.contains("\\n") {
        return Cow::Borrowed(content);
    }
    Cow::Owned(content.replace("\\n", "\n"))
}

fn parse_gitattributes_content_impl(
    content: &str,
    display_path: &str,
    from_blob: bool,
    attr_base: &str,
) -> ParsedGitAttributes {
    let preprocessed = if from_blob {
        preprocess_gitattributes_blob_text(content)
    } else {
        Cow::Borrowed(content)
    };
    let content = preprocessed.as_ref();

    let mut out = ParsedGitAttributes::default();
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line_bytes = raw_line.as_bytes();
        if line_bytes.len() >= MAX_ATTR_LINE_BYTES {
            out.warnings.push(crate::diagnostics::warning_line(&format!(
                "ignoring overly long attributes line {line_no}"
            )));
            continue;
        }
        parse_one_line(
            raw_line,
            line_no,
            display_path,
            from_blob,
            attr_base,
            &mut out,
        );
    }
    out.warnings
        .extend(builtin_warnings_for_rules(&out.rules, display_path));
    out
}

/// Skip leading ASCII blanks only (matches Git's `blank` in `attr.c`).
fn skip_ascii_blank(s: &str) -> &str {
    s.trim_start_matches([' ', '\t', '\r', '\n'])
}

/// First whitespace-delimited token and the remainder (Git `strcspn` on `blank`).
fn split_at_first_blank(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let n = bytes
        .iter()
        .position(|&b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    s.split_at(n)
}

/// C-style unquote for a pattern that starts with `"` (see Git `unquote_c_style` in `quote.c`).
fn unquote_c_style(quoted: &str) -> Result<(String, &str), ()> {
    let b = quoted.as_bytes();
    if b.is_empty() || b[0] != b'"' {
        return Err(());
    }
    let mut q = &b[1..];
    let mut out = Vec::new();
    loop {
        let len = q
            .iter()
            .position(|&c| c == b'"' || c == b'\\')
            .unwrap_or(q.len());
        out.extend_from_slice(&q[..len]);
        q = &q[len..];
        if q.is_empty() {
            return Err(());
        }
        match q[0] {
            b'"' => {
                let rest = std::str::from_utf8(&q[1..]).map_err(|_| ())?;
                return Ok((String::from_utf8(out).map_err(|_| ())?, rest));
            }
            b'\\' => {
                q = &q[1..];
                if q.is_empty() {
                    return Err(());
                }
                let ch = q[0];
                q = &q[1..];
                match ch {
                    b'a' => out.push(0x07),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'v' => out.push(0x0b),
                    b'\\' => out.push(b'\\'),
                    b'"' => out.push(b'"'),
                    b'0'..=b'3' => {
                        let mut ac = u32::from(ch - b'0') << 6;
                        if q.len() < 2 {
                            return Err(());
                        }
                        let ch2 = q[0];
                        let ch3 = q[1];
                        if !(b'0'..=b'7').contains(&ch2) || !(b'0'..=b'7').contains(&ch3) {
                            return Err(());
                        }
                        ac |= u32::from(ch2 - b'0') << 3;
                        ac |= u32::from(ch3 - b'0');
                        q = &q[2..];
                        out.push(ac as u8);
                    }
                    _ => return Err(()),
                }
            }
            _ => return Err(()),
        }
    }
}

/// One attribute assignment token (`parse_attr` in Git `attr.c`).
fn parse_one_attr_token_git(s: &str) -> (&str, Option<&str>, &str) {
    let bytes = s.as_bytes();
    let token_end = bytes
        .iter()
        .position(|&b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    let eq_pos = s.find('=');
    let eq_in_token = eq_pos.filter(|&eq| eq < token_end);
    let (name, val) = if let Some(eq) = eq_in_token {
        (&s[..eq], Some(&s[eq + 1..token_end]))
    } else {
        (&s[..token_end], None)
    };
    let rest = skip_ascii_blank(&s[token_end..]);
    (name, val, rest)
}

fn accumulate_attr_states(
    mut states: &str,
    attrs: &mut Vec<(String, AttrValue)>,
    macros: &MacroTable,
    in_macro_def: bool,
) {
    loop {
        states = skip_ascii_blank(states);
        if states.is_empty() {
            break;
        }
        let (name, val, rest) = parse_one_attr_token_git(states);
        states = rest;
        let tok = match val {
            Some(v) => format!("{name}={v}"),
            None => name.to_string(),
        };
        push_attr_token(&tok, attrs, macros, in_macro_def);
    }
}

const ATTR_MACRO_PREFIX: &str = "[attr]";

fn parse_one_line(
    raw_line: &str,
    line_no: usize,
    display_path: &str,
    from_blob: bool,
    attr_base: &str,
    out: &mut ParsedGitAttributes,
) {
    let _ = from_blob;
    let cp = skip_ascii_blank(raw_line);
    if cp.is_empty() || cp.starts_with('#') {
        return;
    }

    let (pattern_token, states) = if cp.as_bytes().first() == Some(&b'"') {
        match unquote_c_style(cp) {
            Ok((pat, rest)) => (pat, rest),
            Err(()) => {
                let (a, b) = split_at_first_blank(cp);
                (a.to_string(), b)
            }
        }
    } else {
        let (a, b) = split_at_first_blank(cp);
        (a.to_string(), b)
    };

    if pattern_token.len() > ATTR_MACRO_PREFIX.len() && pattern_token.starts_with(ATTR_MACRO_PREFIX)
    {
        if !attr_base.is_empty() {
            out.warnings.push(format!(
                "{pattern_token} not allowed: {display_path}:{line_no}"
            ));
            return;
        }
        let rest = skip_ascii_blank(&pattern_token[ATTR_MACRO_PREFIX.len()..]);
        let (macro_name, leftover) = split_at_first_blank(rest);
        if !leftover.is_empty() || macro_name.is_empty() {
            return;
        }
        let mut attrs = Vec::new();
        accumulate_attr_states(states, &mut attrs, &out.macros, true);
        out.macros.defs.insert(macro_name.to_string(), attrs);
        return;
    }

    if pattern_token.starts_with('!') && !pattern_token.starts_with("\\!") {
        out.warnings
            .push("Negative patterns are ignored".to_string());
        return;
    }
    let pattern_raw = pattern_token.replace("\\!", "!");
    let (pattern, pattern_flags, nowildcardlen) = parse_attr_pattern_fields(&pattern_raw);
    let mut attrs = Vec::new();
    accumulate_attr_states(states, &mut attrs, &out.macros, false);
    if attrs.is_empty() {
        return;
    }
    out.rules.push(AttrRule {
        attr_base: attr_base.to_string(),
        pattern,
        pattern_flags,
        nowildcardlen,
        skip: false,
        line: line_no,
        attrs,
    });
}

fn push_attr_token(
    tok: &str,
    attrs: &mut Vec<(String, AttrValue)>,
    _macros: &MacroTable,
    in_macro_def: bool,
) {
    if tok == "binary" {
        attrs.push(("text".into(), AttrValue::Unset));
        attrs.push(("diff".into(), AttrValue::Unset));
        attrs.push(("merge".into(), AttrValue::Unset));
        attrs.push(("binary".into(), AttrValue::Set));
        return;
    }
    if in_macro_def {
        if let Some(rest) = tok.strip_prefix('!') {
            attrs.push((rest.to_string(), AttrValue::Clear));
            return;
        }
    }
    if let Some(rest) = tok.strip_prefix('-') {
        attrs.push((rest.to_string(), AttrValue::Unset));
        return;
    }
    if let Some((k, v)) = tok.split_once('=') {
        let v = v.trim_end_matches(|c: char| {
            matches!(c, ' ' | '\t' | '\r' | '\n') || c == '\u{000b}' || c == '\u{000c}'
        });
        attrs.push((k.to_string(), AttrValue::Value(v.to_string())));
        return;
    }
    attrs.push((tok.to_string(), AttrValue::Set));
}

fn fspathncmp(a: &[u8], b: &[u8], count: usize, icase: bool) -> bool {
    if a.len() < count || b.len() < count {
        return false;
    }
    if icase {
        a[..count]
            .iter()
            .zip(&b[..count])
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
    } else {
        a[..count] == b[..count]
    }
}

/// Git `match_basename` (`dir.c`) for attribute patterns.
fn match_basename_git(
    basename: &[u8],
    pattern: &[u8],
    prefix: usize,
    patternlen: usize,
    pat_flags: u32,
    icase: bool,
) -> bool {
    let basenamelen = basename.len();
    let wm_flags = if icase { WM_CASEFOLD } else { 0 };
    if prefix == patternlen {
        return patternlen == basenamelen && fspathncmp(pattern, basename, basenamelen, icase);
    }
    if (pat_flags & PAT_ENDSWITH) != 0 {
        if patternlen <= 1 {
            return false;
        }
        let lit_len = patternlen - 1;
        if lit_len > basenamelen {
            return false;
        }
        return fspathncmp(
            &pattern[1..patternlen],
            &basename[basenamelen - lit_len..],
            lit_len,
            icase,
        );
    }
    wildmatch(&pattern[..patternlen], basename, wm_flags)
}

/// Git `match_pathname` (`dir.c`) for attribute patterns.
#[allow(clippy::too_many_arguments)]
fn match_pathname_git(
    pathname: &[u8],
    pathlen: usize,
    base: &[u8],
    baselen: usize,
    mut pattern: &[u8],
    mut prefix: usize,
    mut patternlen: usize,
    icase: bool,
) -> bool {
    let pathname = &pathname[..pathlen.min(pathname.len())];

    if !pattern.is_empty() && pattern[0] == b'/' {
        pattern = &pattern[1..];
        patternlen -= 1;
        prefix = prefix.saturating_sub(1);
    }

    if pathlen < baselen + 1 {
        return false;
    }
    if baselen > 0 && pathname[baselen] != b'/' {
        return false;
    }
    if !fspathncmp(pathname, base, baselen, icase) {
        return false;
    }

    let namelen = if baselen == 0 {
        pathlen
    } else {
        pathlen - baselen - 1
    };
    let name = &pathname[pathlen - namelen..];

    if prefix > 0 {
        if prefix > namelen {
            return false;
        }
        if !fspathncmp(pattern, name, prefix, icase) {
            return false;
        }
        if patternlen == prefix && namelen == prefix {
            return true;
        }
        let advance = prefix - 1;
        pattern = &pattern[advance..];
        patternlen -= advance;
        let name = &name[advance..];
        let wm_flags = WM_PATHNAME | if icase { WM_CASEFOLD } else { 0 };
        return wildmatch(&pattern[..patternlen], name, wm_flags);
    }

    let wm_flags = WM_PATHNAME | if icase { WM_CASEFOLD } else { 0 };
    wildmatch(&pattern[..patternlen], name, wm_flags)
}

/// Directory prefix of `rel_path` (no trailing slash), or `""` for a top-level file.
fn path_dir_prefix(rel_path: &str) -> &str {
    match rel_path.rfind('/') {
        Some(i) => &rel_path[..i],
        None => "",
    }
}

/// Whether a rule from `dir/.gitattributes` may apply to `rel_path` (Git `prepare_attr_stack`).
///
/// Rules from nested attribute files only affect paths inside that directory tree.
#[must_use]
pub fn attr_rule_applies_to_path(attr_base: &str, rel_path: &str, icase: bool) -> bool {
    if attr_base.is_empty() {
        return true;
    }
    let dir = path_dir_prefix(rel_path);
    if dir.is_empty() {
        return false;
    }
    let prefix_eq = |d: &str, b: &str| {
        if icase {
            d.eq_ignore_ascii_case(b)
        } else {
            d == b
        }
    };
    if prefix_eq(dir, attr_base) {
        return true;
    }
    let bl = attr_base.len();
    if dir.len() > bl && dir.as_bytes()[bl] == b'/' && prefix_eq(&dir[..bl], attr_base) {
        return true;
    }
    false
}

/// Match one parsed rule against a repo-relative path (Git `path_matches` / `attr.c`).
#[must_use]
pub fn attr_rule_matches(rule: &AttrRule, rel_path: &str, icase: bool) -> bool {
    if !attr_rule_applies_to_path(&rule.attr_base, rel_path, icase) {
        return false;
    }
    let pathname = rel_path.as_bytes();
    let pathlen = pathname.len();
    let isdir = pathlen > 0 && pathname[pathlen - 1] == b'/';

    if (rule.pattern_flags & PAT_MUSTBEDIR) != 0 && !isdir {
        return false;
    }

    let eff_pathlen = if isdir { pathlen - 1 } else { pathlen };
    let pathname_trim = &pathname[..eff_pathlen];

    let basename_offset = pathname_trim
        .iter()
        .rposition(|&b| b == b'/')
        .map(|i| i + 1)
        .unwrap_or(0);

    let pat = rule.pattern.as_bytes();
    let prefix = rule.nowildcardlen.min(pat.len());
    let patternlen = pat.len();

    if (rule.pattern_flags & PAT_NODIR) != 0 {
        let bn = &pathname_trim[basename_offset..];
        return match_basename_git(bn, pat, prefix, patternlen, rule.pattern_flags, icase);
    }

    let base = rule.attr_base.as_bytes();
    match_pathname_git(
        pathname_trim,
        eff_pathlen,
        base,
        base.len(),
        pat,
        prefix,
        patternlen,
        icase,
    )
}

/// Expand macros and `binary` for one rule's assignments into source-order operations.
///
/// These must be applied in order to the same map as later rules (not folded into a local map),
/// so `!attr` / macro clears remove attributes set by earlier rules on the same path.
fn expand_rule_attrs_flat(rule: &AttrRule, macros: &MacroTable) -> Vec<(String, AttrValue)> {
    let mut flat: Vec<(String, AttrValue)> = Vec::new();
    for (name, val) in &rule.attrs {
        if name == "binary" {
            flat.push(("text".into(), AttrValue::Unset));
            flat.push(("diff".into(), AttrValue::Unset));
            flat.push(("merge".into(), AttrValue::Unset));
            flat.push(("binary".into(), AttrValue::Set));
            continue;
        }
        if let Some(exp) = macros.defs.get(name) {
            flat.push((name.clone(), val.clone()));
            for (n, v) in exp {
                flat.push((n.clone(), v.clone()));
            }
        } else {
            flat.push((name.clone(), val.clone()));
        }
    }
    flat
}

/// Merge assignments: later rules override earlier; within one expanded rule, last wins.
pub fn collect_attrs_for_path(
    rules: &[AttrRule],
    macros: &MacroTable,
    rel_path: &str,
    icase: bool,
) -> HashMap<String, AttrValue> {
    let mut map: HashMap<String, AttrValue> = HashMap::new();
    for rule in rules {
        if rule.skip {
            continue;
        }
        if !attr_rule_matches(rule, rel_path, icase) {
            continue;
        }
        let ops = expand_rule_attrs_flat(rule, macros);
        for (n, v) in ops {
            match v {
                AttrValue::Clear => {
                    map.remove(&n);
                }
                _ => {
                    map.insert(n, v);
                }
            }
        }
    }
    map
}

/// Quote a path for `check-attr` output (C-style) when needed.
#[must_use]
pub fn quote_path_for_check_attr(path: &str) -> String {
    let needs = path
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\');
    if !needs {
        return path.to_string();
    }
    let mut s = String::new();
    s.push('"');
    for c in path.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            _ if c.is_control() => s.push_str(&format!("\\{:o}", c as u32)),
            _ => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Normalize `.` / `..` segments in a repo-relative path string.
#[must_use]
pub fn normalize_rel_path(path: &str) -> String {
    let p = Path::new(path);
    let mut stack: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => stack.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                let _ = stack.pop();
            }
            Component::CurDir => {}
            _ => {}
        }
    }
    stack.join("/")
}

fn lexical_normalize_path(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(_) => out.push(c),
        }
    }
    out
}

/// Resolve a user path to a repo-relative path (forward slashes).
///
/// Uses [`std::fs::canonicalize`] when the target exists; otherwise resolves `..` lexically from the
/// current directory so paths like `../f` work for missing files (Git `prefix_path`, t0003).
pub fn path_relative_to_worktree(
    repo: &Repository,
    path_str: &str,
) -> std::result::Result<String, String> {
    let wt = repo
        .work_tree
        .as_ref()
        .ok_or_else(|| "bare repository — no work tree".to_string())?;
    let cwd = repo.environment().cwd.clone();
    let p = Path::new(path_str);
    let combined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };

    if let Ok(abs) = combined.canonicalize() {
        let rel = crate::git_path::strip_worktree_prefix(&abs, wt)
            .ok_or_else(|| format!("path outside repository: {}", path_str))?;
        return Ok(normalize_rel_path(&rel));
    }

    let abs_lex = lexical_normalize_path(combined);
    let rel = crate::git_path::strip_worktree_prefix(&abs_lex, wt)
        .ok_or_else(|| format!("path outside repository: {}", path_str))?;
    Ok(normalize_rel_path(&rel))
}

fn collect_nested_gitattributes_dirs(work_tree: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    walk_dirs(work_tree, work_tree, &mut dirs);
    dirs.sort_by(|a, b| {
        let da = a.components().count();
        let db = b.components().count();
        da.cmp(&db).then_with(|| a.cmp(b))
    });
    dirs
}

fn walk_dirs(root: &Path, cur: &Path, dirs: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(cur) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let ft = e.file_type().ok();
        if ft.is_some_and(|t| t.is_dir()) {
            if p.file_name() == Some(OsStr::new(".git")) {
                continue;
            }
            let rel = p.strip_prefix(root).unwrap_or(&p);
            dirs.push(rel.to_path_buf());
            walk_dirs(root, &p, dirs);
        }
    }
}

// ── Process-lifetime gitattributes cache ─────────────────────────────
//
// `load_gitattributes_stack` re-walks the entire working tree (`read_dir`
// per directory) and re-parses every `.gitattributes` on each call, and hot
// paths (grep/diff/add/checkout) call it per file. The parsed stack is
// memoized for the process lifetime and revalidated with stat stamps on
// every call:
//
// - the global attributes file, root `.gitattributes`, and
//   `info/attributes` are stamped (mtime + size, or "absent"), recorded
//   *before* the parse;
// - the work-tree root directory is mtime-stamped, so creating or deleting
//   a root-level entry forces a re-walk. Nested `.gitattributes` files are
//   *not* revalidated per query (see `collect_stack_stamps`); within one
//   process they behave like C git's process-lifetime attribute cache.
//
// Tree-sourced stacks (`attr.tree` / `GIT_ATTR_SOURCE`) are keyed by tree
// OID and never revalidated: tree objects are content-addressed and
// immutable.
//
// The resolved global-attributes *path* (from `core.attributesFile`) is
// recorded at parse time and only re-statted afterwards; a mid-process
// change to that config value is not detected. C git caches the attribute
// stack per directory for the whole process with no revalidation at all,
// so serving a stamped copy is strictly more conservative than upstream.

type AttrFileStamp = (PathBuf, Option<(SystemTime, u64)>);
type AttrDirStamp = (PathBuf, Option<SystemTime>);

pub(crate) struct AttrStackCacheEntry {
    pub(crate) file_stamps: Vec<AttrFileStamp>,
    pub(crate) dir_stamps: Vec<AttrDirStamp>,
    pub(crate) parsed: Arc<ParsedGitAttributes>,
}

/// `symlink_metadata`-based stamp, matching `read_gitattributes_maybe_symlink`
/// (symlinked `.gitattributes` files are skipped by the parser, but stamping
/// the link still detects replacement by a regular file).
pub(crate) fn attr_file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

pub(crate) fn attr_dir_stamp(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
}

pub(crate) fn attr_stamps_valid(entry: &AttrStackCacheEntry) -> bool {
    entry
        .file_stamps
        .iter()
        .all(|(path, stamp)| attr_file_stamp(path) == *stamp)
        && entry
            .dir_stamps
            .iter()
            .all(|(path, stamp)| attr_dir_stamp(path) == *stamp)
}

/// Stamp the cheap top-level inputs of the stack: the global attributes
/// file, root `.gitattributes`, `info/attributes`, and the work-tree root
/// directory's mtime (~4 stats per validation).
///
/// Nested per-directory `.gitattributes` files are deliberately *not*
/// stamped: revalidating them costs two stats per walked directory, which
/// dominates per-file hot loops on large trees. Within one process a change
/// to an already-loaded nested file is therefore served stale — matching
/// C git, which caches attribute stacks for the whole process with no
/// revalidation at all. The checkout/apply/merge materialization paths are
/// unaffected: they read attributes through
/// `crlf::load_gitattributes_for_checkout` (index/odb-sourced), not this
/// work-tree stack. Creating or deleting entries in the work-tree *root*
/// still bumps its stamped mtime and forces a fresh walk.
pub(crate) fn collect_stack_stamps(
    repo: &Repository,
    work_tree: &Path,
) -> std::result::Result<(Vec<AttrFileStamp>, Vec<AttrDirStamp>), crate::error::Error> {
    let mut file_stamps = Vec::new();
    if let Some(g) = global_attributes_path(repo)? {
        let stamp = attr_file_stamp(&g);
        file_stamps.push((g, stamp));
    }
    let root_ga = work_tree.join(".gitattributes");
    let stamp = attr_file_stamp(&root_ga);
    file_stamps.push((root_ga, stamp));
    let info = repo.git_dir.join("info/attributes");
    let stamp = attr_file_stamp(&info);
    file_stamps.push((info, stamp));
    let dir_stamps = vec![(work_tree.to_path_buf(), attr_dir_stamp(work_tree))];
    Ok((file_stamps, dir_stamps))
}

/// Load the full stack of attribute rules for a normal repository (working tree).
///
/// Results are memoized for the process lifetime and revalidated against
/// stat stamps on every call (see the cache notes above).
pub fn load_gitattributes_stack(
    repo: &Repository,
    work_tree: &Path,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    repo.caches().load_gitattributes_stack(repo, work_tree)
}

pub(crate) fn load_gitattributes_stack_uncached(
    repo: &Repository,
    work_tree: &Path,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    let mut merged = ParsedGitAttributes::default();

    if let Some(g) = global_attributes_path(repo)? {
        if g.exists() {
            if let Ok(content) = fs::read_to_string(&g) {
                if content.len() <= MAX_ATTR_FILE_BYTES {
                    let mut p =
                        parse_gitattributes_file_content(&content, g.to_string_lossy().as_ref());
                    merged.rules.append(&mut p.rules);
                    merged.macros.defs.extend(p.macros.defs.drain());
                    merged.warnings.append(&mut p.warnings);
                } else {
                    merged
                        .warnings
                        .push(crate::diagnostics::warning_line(&format!(
                            "ignoring overly large gitattributes file '{}'",
                            g.display()
                        )));
                }
            }
        }
    }

    let root_ga = work_tree.join(".gitattributes");
    if let Some(content) =
        read_gitattributes_maybe_symlink(&root_ga, ".gitattributes", &mut merged.warnings)
    {
        if content.len() <= MAX_ATTR_FILE_BYTES {
            let mut p = parse_gitattributes_file_content(&content, ".gitattributes");
            merged.rules.append(&mut p.rules);
            merged.macros.defs.extend(p.macros.defs.drain());
            merged.warnings.append(&mut p.warnings);
        } else {
            merged.warnings.push(crate::diagnostics::warning_line(
                "ignoring overly large gitattributes file '.gitattributes'",
            ));
        }
    }

    for rel in collect_nested_gitattributes_dirs(work_tree) {
        let ga = work_tree.join(&rel).join(".gitattributes");
        if let Some(content) = read_gitattributes_maybe_symlink(
            &ga,
            &format!("{}/.gitattributes", rel.display()),
            &mut merged.warnings,
        ) {
            if content.len() > MAX_ATTR_FILE_BYTES {
                merged
                    .warnings
                    .push(crate::diagnostics::warning_line(&format!(
                        "ignoring overly large gitattributes file '{}'",
                        ga.display()
                    )));
                continue;
            }
            let prefix = rel.to_string_lossy().replace('\\', "/");
            let mut p = parse_gitattributes_file_content_with_base(
                &content,
                &ga.to_string_lossy(),
                &prefix,
            );
            merged.rules.append(&mut p.rules);
            merged.warnings.append(&mut p.warnings);
        }
    }

    let info = repo.git_dir.join("info/attributes");
    if info.exists() {
        if let Ok(content) = fs::read_to_string(&info) {
            if content.len() <= MAX_ATTR_FILE_BYTES {
                let mut p = parse_gitattributes_file_content(&content, "info/attributes");
                merged.rules.append(&mut p.rules);
                merged.macros.defs.extend(p.macros.defs.drain());
                merged.warnings.append(&mut p.warnings);
            }
        }
    }

    Ok(merged)
}

/// Bare repository: only `info/attributes` from disk (no in-repo `.gitattributes` file).
///
/// Memoized like [`load_gitattributes_stack`], keyed by `git_dir`.
pub fn load_gitattributes_bare(
    repo: &Repository,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    repo.caches().load_gitattributes_bare(repo)
}

pub(crate) fn load_gitattributes_bare_uncached(
    repo: &Repository,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    let mut merged = ParsedGitAttributes::default();
    if let Some(g) = global_attributes_path(repo)? {
        if g.exists() {
            if let Ok(content) = fs::read_to_string(&g) {
                if content.len() <= MAX_ATTR_FILE_BYTES {
                    let mut p =
                        parse_gitattributes_file_content(&content, g.to_string_lossy().as_ref());
                    merged.rules.append(&mut p.rules);
                    merged.macros.defs.extend(p.macros.defs.drain());
                    merged.warnings.append(&mut p.warnings);
                }
            }
        }
    }
    let info = repo.git_dir.join("info/attributes");
    if info.exists() {
        if let Ok(content) = fs::read_to_string(&info) {
            if content.len() <= MAX_ATTR_FILE_BYTES {
                let mut p = parse_gitattributes_file_content(&content, "info/attributes");
                merged.rules.append(&mut p.rules);
                merged.macros.defs.extend(p.macros.defs.drain());
                merged.warnings.append(&mut p.warnings);
            }
        }
    }
    // Without a work tree, Git reads tracked `.gitattributes` from the index (Git
    // `read_attr_from_index`), so e.g. `git -C .git diff-tree --check` still honours a
    // committed `* -whitespace` attribute. Prepend index rules so work-tree-equivalent
    // ordering (closer paths win) is preserved relative to info/global.
    if let Ok(index) = Index::load(&repo.git_dir.join("index")) {
        if let Ok(mut from_index) = load_gitattributes_from_index(&index, &repo.odb, &repo.git_dir)
        {
            // info/global attributes are lower priority than per-tree `.gitattributes`,
            // so place the index rules ahead of what we have collected so far.
            from_index.rules.append(&mut merged.rules);
            merged.rules = from_index.rules;
            for (k, v) in from_index.macros.defs.drain() {
                merged.macros.defs.entry(k).or_insert(v);
            }
            merged.warnings.append(&mut from_index.warnings);
        }
    }
    Ok(merged)
}

/// Read `.gitattributes` blob from a tree object at `tree_oid`, recursively.
pub fn load_gitattributes_from_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    repo.caches()
        .load_gitattributes_from_tree_cached(&repo.odb, tree_oid, || {
            let mut merged = ParsedGitAttributes::default();
            walk_tree_attrs(&repo.odb, tree_oid, "", &mut merged)?;
            Ok(merged)
        })
}

fn walk_tree_attrs(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    merged: &mut ParsedGitAttributes,
) -> std::result::Result<(), crate::error::Error> {
    let obj = odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    let entries = parse_tree(&obj.data)?;
    for e in entries {
        let name = String::from_utf8_lossy(&e.name).to_string();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match e.mode {
            0o040000 => {
                walk_tree_attrs(odb, &e.oid, &path, merged)?;
            }
            0o100644 | 0o100755 | 0o120000 if name == ".gitattributes" => {
                let oid = e.oid;
                {
                    let blob = odb.read(&oid)?;
                    if blob.kind != ObjectKind::Blob {
                        continue;
                    }
                    if blob.data.len() > MAX_ATTR_FILE_BYTES {
                        merged.warnings.push(crate::diagnostics::warning_line(
                            "ignoring overly large gitattributes blob '.gitattributes'",
                        ));
                        continue;
                    }
                    let content = String::from_utf8_lossy(&blob.data).into_owned();
                    let display = format!("{path} (tree)");
                    let attr_base = Path::new(&path)
                        .parent()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    let mut p =
                        parse_gitattributes_content_impl(&content, &display, true, &attr_base);
                    merged.rules.append(&mut p.rules);
                    if attr_base.is_empty() {
                        merged.macros.defs.extend(p.macros.defs.drain());
                    }
                    merged.warnings.append(&mut p.warnings);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Load merged `.gitattributes` rules for diff and merge (respects `GIT_ATTR_SOURCE` / `attr.tree`).
///
/// Resolution order matches Git's attribute source for diff: optional tree from
/// [`resolve_attr_treeish`], then work tree stack (or bare `info/attributes` only).
///
/// # Errors
///
/// Returns an error when a tree-ish source is set from the environment or command line and cannot
/// be resolved (Git: *"bad --attr-source or GIT_ATTR_SOURCE"*).
pub fn load_gitattributes_for_diff(
    repo: &Repository,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    let (treeish, ignore_bad_tree) = resolve_attr_treeish(repo, None)?;
    if let Some(spec) = treeish.filter(|s| !s.is_empty()) {
        match resolve_tree_oid(repo, &spec) {
            Ok(oid) => return load_gitattributes_from_tree(repo, &oid),
            Err(_) if ignore_bad_tree => {}
            Err(_) => {
                return Err(crate::error::Error::InvalidRef(format!(
                    "bad --attr-source or GIT_ATTR_SOURCE: {spec}"
                )));
            }
        }
    }
    if let Some(wt) = repo.work_tree.as_deref() {
        return load_gitattributes_stack(repo, wt);
    }
    load_gitattributes_bare(repo)
}

/// Resolve `attr.tree`, `GIT_ATTR_SOURCE`, `--source` precedence for check-attr.
///
/// The second return value is `ignore_bad_resolution`: when true (only for `attr.tree` from
/// config), an unresolvable tree-ish falls back to reading `.gitattributes` from the work tree
/// or index instead of erroring (matches Git `compute_default_attr_source`).
pub fn resolve_attr_treeish(
    repo: &Repository,
    source_arg: Option<&str>,
) -> std::result::Result<(Option<String>, bool), crate::error::Error> {
    let env_src = repo
        .environment()
        .var("GIT_ATTR_SOURCE")
        .filter(|s| !s.is_empty());
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true)?;
    let cfg_tree = config.get("attr.tree");
    if let Some(s) = source_arg.map(|s| s.to_string()) {
        return Ok((Some(s), false));
    }
    if let Some(s) = env_src {
        return Ok((Some(s), false));
    }
    if let Some(s) = cfg_tree {
        return Ok((Some(s), true));
    }
    Ok((None, false))
}

/// Parse a revision to a tree OID for attribute loading.
pub fn resolve_tree_oid(repo: &Repository, spec: &str) -> std::result::Result<ObjectId, String> {
    let oid = resolve_revision(repo, spec).map_err(|e| e.to_string())?;
    let obj = repo.read_replaced(&oid).map_err(|e| e.to_string())?;
    match obj.kind {
        ObjectKind::Commit => {
            let c = crate::objects::parse_commit(&obj.data).map_err(|e| e.to_string())?;
            Ok(c.tree)
        }
        ObjectKind::Tree => Ok(oid),
        _ => Err("revision is not a commit or tree".to_string()),
    }
}

/// Load attributes from the index (stage 0) for `.gitattributes` paths only.
pub fn load_gitattributes_from_index(
    index: &Index,
    odb: &Odb,
    work_tree: &Path,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    let mut merged = ParsedGitAttributes::default();
    let mut paths: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.path.ends_with(b".gitattributes"))
        .map(|e| e.path.clone())
        .collect();
    paths.sort();
    for path_bytes in paths {
        let Ok(rel) = std::str::from_utf8(&path_bytes) else {
            continue;
        };
        let Some(entry) = index.get(&path_bytes, 0) else {
            continue;
        };
        let obj = odb.read(&entry.oid)?;
        if obj.data.len() > MAX_ATTR_FILE_BYTES {
            merged
                .warnings
                .push(crate::diagnostics::warning_line(&format!(
                    "ignoring overly large gitattributes blob '{rel}'"
                )));
            continue;
        }
        let content = String::from_utf8_lossy(&obj.data);
        let attr_base = Path::new(rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let mut p = parse_gitattributes_content_impl(&content, rel, true, &attr_base);
        merged.rules.append(&mut p.rules);
        if attr_base.is_empty() {
            merged.macros.defs.extend(p.macros.defs.drain());
        }
        merged.warnings.append(&mut p.warnings);
    }
    let _ = work_tree;
    Ok(merged)
}

fn merge_global_attributes_file(
    repo: &Repository,
    merged: &mut ParsedGitAttributes,
) -> std::result::Result<(), crate::error::Error> {
    if let Some(g) = global_attributes_path(repo)? {
        if g.exists() {
            if let Ok(content) = fs::read_to_string(&g) {
                if content.len() <= MAX_ATTR_FILE_BYTES {
                    let mut p =
                        parse_gitattributes_file_content(&content, g.to_string_lossy().as_ref());
                    merged.rules.append(&mut p.rules);
                    merged.macros.defs.extend(p.macros.defs.drain());
                    merged.warnings.append(&mut p.warnings);
                } else {
                    merged
                        .warnings
                        .push(crate::diagnostics::warning_line(&format!(
                            "ignoring overly large gitattributes file '{}'",
                            g.display()
                        )));
                }
            }
        }
    }
    Ok(())
}

fn merge_info_attributes(repo: &Repository, merged: &mut ParsedGitAttributes) {
    let info = repo.git_dir.join("info/attributes");
    if info.exists() {
        if let Ok(content) = fs::read_to_string(&info) {
            if content.len() <= MAX_ATTR_FILE_BYTES {
                let mut p = parse_gitattributes_file_content(&content, "info/attributes");
                merged.rules.append(&mut p.rules);
                merged.macros.defs.extend(p.macros.defs.drain());
                merged.warnings.append(&mut p.warnings);
            }
        }
    }
}

/// Whether attribute pattern matching should fold ASCII case (`core.ignorecase`).
#[must_use]
pub fn attribute_matching_ignore_case(repo: &Repository) -> bool {
    repo.config()
        .ok()
        .and_then(|c| c.get_bool("core.ignorecase"))
        .and_then(|r| r.ok())
        .unwrap_or(false)
}

/// Merged attribute rules for `git check-attr --cached` (global, index `.gitattributes`, `info/attributes`).
///
/// # Errors
///
/// Returns an error if the index cannot be read or global attributes path resolution fails.
pub fn load_gitattributes_for_check_attr_cached(
    repo: &Repository,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    let mut merged = ParsedGitAttributes::default();
    merge_global_attributes_file(repo, &mut merged)?;
    let index = Index::load(&repo.git_dir.join("index"))?;
    let wt = repo.work_tree.as_deref().unwrap_or(repo.git_dir.as_path());
    let mut from_index = load_gitattributes_from_index(&index, &repo.odb, wt)?;
    merged.rules.append(&mut from_index.rules);
    merged.macros.defs.extend(from_index.macros.defs.drain());
    merged.warnings.append(&mut from_index.warnings);
    merge_info_attributes(repo, &mut merged);
    Ok(merged)
}

/// Merged attribute rules for `git check-attr --source=<tree>` (global, tree blobs, `info/attributes`).
///
/// # Errors
///
/// Returns an error if the tree cannot be read or global attributes path resolution fails.
pub fn load_gitattributes_for_check_attr_source(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> std::result::Result<ParsedGitAttributes, crate::error::Error> {
    let mut merged = ParsedGitAttributes::default();
    merge_global_attributes_file(repo, &mut merged)?;
    walk_tree_attrs(&repo.odb, tree_oid, "", &mut merged)?;
    merge_info_attributes(repo, &mut merged);
    Ok(merged)
}

/// Return `builtin_objectmode` value for a path (working tree), or `None` if unavailable.
///
/// Submodule checkout directories (`.git` is a file containing `gitdir:`) report `160000`
/// like Git, not `040000`.
#[must_use]
pub fn builtin_objectmode_worktree(repo: &Repository, rel_path: &str) -> Option<String> {
    let wt = repo.work_tree.as_ref()?;
    let p = wt.join(rel_path);
    let meta = fs::symlink_metadata(&p).ok()?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Some("120000".to_string());
    }
    if ft.is_dir() {
        let git = p.join(".git");
        if let Ok(git_meta) = fs::symlink_metadata(&git) {
            if !git_meta.file_type().is_dir() {
                if let Ok(content) = fs::read_to_string(&git) {
                    if content.starts_with("gitdir:") {
                        return Some("160000".to_string());
                    }
                }
            }
        }
        return Some("040000".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = normalize_mode(meta.mode());
        Some(format!("{:06o}", m))
    }
    #[cfg(not(unix))]
    {
        let _ = repo;
        None
    }
}

/// `builtin_objectmode` from the index when `--cached` is used.
#[must_use]
pub fn builtin_objectmode_index(index: &Index, rel_path: &str) -> Option<String> {
    let key = rel_path.as_bytes();
    let e = index.get(key, 0)?;
    let m = e.mode;
    if m == MODE_SYMLINK {
        return Some("120000".to_string());
    }
    if m == MODE_GITLINK {
        return Some("160000".to_string());
    }
    if m == MODE_TREE {
        return Some("040000".to_string());
    }
    if m == MODE_EXECUTABLE {
        return Some("100755".to_string());
    }
    if m == MODE_REGULAR {
        return Some("100644".to_string());
    }
    Some(format!("{:06o}", m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unclosed_quote_falls_back_to_literal_pattern() {
        let parsed = parse_gitattributes_file_content("\"unclosed test=set\n", ".gitattributes");
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(parsed.rules[0].pattern, "\"unclosed");
    }

    #[test]
    fn directory_only_pattern_matches_trailing_slash_paths() {
        let parsed = parse_gitattributes_file_content("dir/ export-submodule\n", ".gitattributes");
        let rule = &parsed.rules[0];
        assert!(!attr_rule_matches(rule, "dir", false));
        assert!(attr_rule_matches(rule, "dir/", false));
    }

    #[test]
    fn builtin_objectmode_assignment_emits_check_attr_warning() {
        let parsed = parse_gitattributes_file_content("* builtin_objectmode=1\n", ".gitattributes");
        let warnings = builtin_warnings_for_rules(&parsed.rules, ".gitattributes");
        assert!(warnings.iter().any(|w| w.contains("builtin_objectmode")));
    }

    #[test]
    fn attr_rule_endswith_star_suffix() {
        let parsed = parse_gitattributes_file_content("*.txt text\n", ".gitattributes");
        let rule = &parsed.rules[0];
        assert!(attr_rule_matches(rule, "readme.txt", false));
        assert!(!attr_rule_matches(rule, "readme.txta", false));
    }

    #[test]
    fn normalize_rel_path_collapses_dot_dot() {
        assert_eq!(normalize_rel_path("a/b/../c"), "a/c");
        assert_eq!(normalize_rel_path("."), "");
    }

    #[test]
    fn attr_rule_applies_to_path_honors_nested_attr_base() {
        assert!(!attr_rule_applies_to_path("a/b", "a/c/f", false));
        assert!(attr_rule_applies_to_path("a/b", "a/b/f", false));
        assert!(attr_rule_applies_to_path("a/b", "a/b/c/f", false));
    }

    #[test]
    fn leading_slash_pattern_matches_from_repository_root() {
        let parsed = parse_gitattributes_file_content("/f test=set\n", ".gitattributes");
        let rule = &parsed.rules[0];
        assert!(attr_rule_matches(rule, "f", false));
        assert!(!attr_rule_matches(rule, "sub/f", false));
    }

    #[test]
    fn validate_rules_for_add_accepts_normal_attributes() {
        let parsed = parse_gitattributes_file_content("* text\n", ".gitattributes");
        assert!(validate_rules_for_add(&parsed.rules, ".gitattributes").is_ok());
    }

    #[test]
    fn quote_path_for_check_attr_leaves_simple_paths() {
        assert_eq!(quote_path_for_check_attr("a/b"), "a/b");
    }

    #[test]
    fn d_yes_rule_clears_test_after_d_star() {
        let mut merged = ParsedGitAttributes::default();
        let root = parse_gitattributes_file_content("[attr]notest !test\n", ".gitattributes");
        merged.macros.defs.extend(root.macros.defs);
        let mut ab = parse_gitattributes_file_content_with_base(
            "h test=a/b/h\nd/* test=a/b/d/*\nd/yes notest\n",
            "a/b/.gitattributes",
            "a/b",
        );
        assert_eq!(ab.rules.len(), 3);
        merged.rules.append(&mut ab.rules);
        merged.macros.defs.extend(ab.macros.defs);
        let d_yes = merged
            .rules
            .iter()
            .find(|r| r.pattern == "d/yes")
            .expect("d/yes rule");
        assert!(attr_rule_matches(d_yes, "a/b/d/yes", false));
        let m = collect_attrs_for_path(&merged.rules, &merged.macros, "a/b/d/yes", false);
        assert!(
            !m.contains_key("test"),
            "expected test cleared by notest macro, got {:?}",
            m.get("test")
        );
    }
}

#[cfg(test)]
mod attr_cache_tests {
    use super::*;
    use filetime::FileTime;

    fn test_repo(td: &Path) -> Repository {
        crate::repo::init_repository(td, false, "main", None, "files").expect("init repo")
    }

    fn rules_for(repo: &Repository, wt: &Path) -> Vec<String> {
        let parsed = load_gitattributes_stack(repo, wt).expect("load stack");
        parsed.rules.iter().map(|r| r.pattern.clone()).collect()
    }

    fn mtime_of(path: &Path) -> FileTime {
        FileTime::from_last_modification_time(&fs::symlink_metadata(path).expect("stat"))
    }

    fn restore_mtime(path: &Path, stamp: FileTime) {
        filetime::set_file_mtime(path, stamp).expect("restore mtime");
    }

    #[test]
    fn stack_cache_serves_same_stamp_and_invalidates_on_change() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        let ga = wt.join(".gitattributes");
        fs::write(&ga, "*.aaa text\n").expect("write v1");
        let wt_t0 = mtime_of(wt);
        restore_mtime(wt, wt_t0);
        let t0 = mtime_of(&ga);
        assert_eq!(rules_for(&repo, wt), vec!["*.aaa".to_string()]);

        // Same size + restored mtime (file and work-tree dir): stat cannot
        // tell the difference, so the cached parse is served. This is the
        // assertion that proves the cache is actually used.
        fs::write(&ga, "*.bbb text\n").expect("write v2");
        restore_mtime(&ga, t0);
        restore_mtime(wt, wt_t0);
        assert_eq!(rules_for(&repo, wt), vec!["*.aaa".to_string()]);

        // A size change invalidates even with restored mtimes.
        fs::write(&ga, "*.ccc-longer text\n").expect("write v3");
        restore_mtime(&ga, t0);
        restore_mtime(wt, wt_t0);
        assert_eq!(rules_for(&repo, wt), vec!["*.ccc-longer".to_string()]);
    }

    #[test]
    fn new_nested_gitattributes_is_detected() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        fs::write(wt.join(".gitattributes"), "root-rule text\n").expect("write root");
        // Pre-age the work-tree mtime so the upcoming mkdir visibly bumps it
        // even on filesystems with coarse mtime ticks.
        restore_mtime(wt, FileTime::from_unix_time(1_000_000_000, 0));
        assert_eq!(rules_for(&repo, wt), vec!["root-rule".to_string()]);

        // Creating a subdirectory bumps the stamped work-tree mtime, forcing
        // a re-walk that discovers the new nested file.
        fs::create_dir(wt.join("sub")).expect("mkdir");
        fs::write(wt.join("sub/.gitattributes"), "nested-rule text\n").expect("write nested");
        assert_eq!(
            rules_for(&repo, wt),
            vec!["root-rule".to_string(), "nested-rule".to_string()]
        );
    }

    #[test]
    fn modified_nested_gitattributes_follows_c_git_process_semantics() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        fs::create_dir(wt.join("sub")).expect("mkdir");
        let nested = wt.join("sub/.gitattributes");
        fs::write(&nested, "one text\n").expect("write v1");
        // Pre-age the work-tree mtime so the later root-level mkdir visibly
        // bumps it even on filesystems with coarse mtime ticks.
        restore_mtime(wt, FileTime::from_unix_time(1_000_000_000, 0));
        assert_eq!(rules_for(&repo, wt), vec!["one".to_string()]);

        // Nested files are not revalidated per query: within one process a
        // content edit is served from cache, matching C git's
        // process-lifetime attribute caching.
        fs::write(&nested, "two-longer text\n").expect("write v2");
        assert_eq!(rules_for(&repo, wt), vec!["one".to_string()]);

        // Any root-level signal (here: a new top-level directory) bumps the
        // stamped work-tree mtime and the fresh walk picks up the edit.
        fs::create_dir(wt.join("poke")).expect("mkdir poke");
        assert_eq!(rules_for(&repo, wt), vec!["two-longer".to_string()]);
    }

    #[test]
    fn oversized_worktree_gitattributes_file_is_skipped() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        let data = vec![b'x'; MAX_ATTR_FILE_BYTES + 1];
        fs::write(wt.join(".gitattributes"), &data).expect("large ga");
        let parsed = load_gitattributes_stack_uncached(&repo, wt).expect("stack");
        assert!(parsed.rules.is_empty());
        assert!(
            parsed
                .warnings
                .iter()
                .any(|w| w.contains("ignoring overly large")),
            "{:?}",
            parsed.warnings
        );
    }

    #[test]
    fn load_cached_and_source_loaders_merge_info() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        fs::write(wt.join(".gitattributes"), "f test=root\n").expect("ga");
        fs::write(repo.git_dir.join("info/attributes"), "f test=info\n").expect("info");
        let cached = load_gitattributes_for_check_attr_cached(&repo).expect("cached");
        assert_eq!(
            collect_attrs_for_path(&cached.rules, &cached.macros, "f", false).get("test"),
            Some(&AttrValue::Value("info".into()))
        );
    }

    #[test]
    fn source_loader_includes_tree_and_info_attributes() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        fs::write(wt.join(".gitattributes"), "tracked test=from-tree\n").expect("ga");
        fs::write(repo.git_dir.join("info/attributes"), "tracked test=info\n").expect("info");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["add", ".gitattributes"])
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git add");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["commit", "-m", "c", "--allow-empty"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("commit");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["add", ".gitattributes"])
            .status()
            .ok();
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["commit", "-m", "attrs"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("commit2");
        let oid = resolve_tree_oid(&repo, "HEAD").expect("head tree");
        let parsed = load_gitattributes_for_check_attr_source(&repo, &oid).expect("source");
        let v = collect_attrs_for_path(&parsed.rules, &parsed.macros, "tracked", false);
        assert_eq!(v.get("test"), Some(&AttrValue::Value("info".into())));
    }

    #[test]
    fn attribute_matching_ignore_case_follows_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["config", "core.ignorecase", "true"])
            .status()
            .expect("cfg");
        assert!(attribute_matching_ignore_case(&repo));
    }

    #[test]
    fn resolve_attr_treeish_prefers_explicit_source_argument() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        let (spec, ignore) = resolve_attr_treeish(&repo, Some("HEAD^{tree}")).expect("treeish");
        assert_eq!(spec.as_deref(), Some("HEAD^{tree}"));
        assert!(!ignore);
    }

    #[test]
    fn load_gitattributes_for_diff_honors_attr_tree_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        fs::write(wt.join(".gitattributes"), "f test=committed\n").expect("ga");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["add", ".gitattributes"])
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("add");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["commit", "-m", "ga"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("commit");
        fs::write(wt.join(".gitattributes"), "f test=worktree\n").expect("ga2");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["config", "attr.tree", "HEAD"])
            .status()
            .expect("cfg");
        let parsed = load_gitattributes_for_diff(&repo).expect("diff attrs");
        assert_eq!(
            collect_attrs_for_path(&parsed.rules, &parsed.macros, "f", false).get("test"),
            Some(&AttrValue::Value("committed".into()))
        );
    }

    #[test]
    fn load_gitattributes_from_tree_reads_nested_blob() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        fs::create_dir_all(wt.join("sub")).expect("mkdir");
        fs::write(wt.join("sub/.gitattributes"), "f test=nested\n").expect("ga");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["add", "sub/.gitattributes"])
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("add");
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["commit", "-m", "nested-attrs"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("commit");
        let oid = resolve_tree_oid(&repo, "HEAD").expect("tree");
        let parsed = load_gitattributes_from_tree(&repo, &oid).expect("from tree");
        assert_eq!(
            collect_attrs_for_path(&parsed.rules, &parsed.macros, "sub/f", false).get("test"),
            Some(&AttrValue::Value("nested".into()))
        );
    }

    #[test]
    fn path_relative_to_worktree_resolves_missing_relative_path() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        let mut env = repo.environment().clone();
        env.cwd = wt.join("a");
        fs::create_dir_all(wt.join("a")).expect("mkdir");
        let options = crate::environment::RepositoryOptions::with_environment(env);
        let repo2 = Repository::open_with(&options, &repo.git_dir, Some(wt)).expect("reopen");
        let rel = path_relative_to_worktree(&repo2, "../f").expect("rel");
        assert_eq!(rel, "f");
    }

    #[test]
    fn info_attributes_is_stamped() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let repo = test_repo(wt);
        assert!(rules_for(&repo, wt).is_empty());

        // info/attributes appearing after a cached empty load must be seen.
        fs::write(repo.git_dir.join("info/attributes"), "from-info text\n").expect("write info");
        assert_eq!(rules_for(&repo, wt), vec!["from-info".to_string()]);
    }
}
