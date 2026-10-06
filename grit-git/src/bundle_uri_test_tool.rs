//! `test-tool bundle-uri parse-key-values` / `parse-config` — matches Git's `bundle-uri.c`
//! and `t/helper/test-bundle-uri.c` for `t5750-bundle-uri-parse.sh`.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Result;

use grit_lib::git_path;

/// Prototype base URI used by the Git test helper (`test-bundle-uri.c`).
const TEST_BASE_URI: &str = "<uri>";

#[derive(Debug, Clone)]
struct RemoteBundleInfo {
    id: String,
    uri: Option<String>,
    creation_token: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum BundleMode {
    #[default]
    All,
    Any,
    None,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum BundleHeuristic {
    #[default]
    None,
    CreationToken,
}

#[derive(Debug)]
struct BundleList {
    base_uri: Option<String>,
    mode: BundleMode,
    version: i32,
    heuristic: BundleHeuristic,
    bundles: HashMap<String, RemoteBundleInfo>,
}

impl BundleList {
    fn new() -> Self {
        Self {
            base_uri: None,
            mode: BundleMode::All,
            version: 1,
            heuristic: BundleHeuristic::None,
            bundles: HashMap::new(),
        }
    }
}

fn heuristic_name(h: BundleHeuristic) -> Option<&'static str> {
    match h {
        BundleHeuristic::CreationToken => Some("creationToken"),
        BundleHeuristic::None => None,
    }
}

/// Split `bundle.<subsection?>.<subkey>` the same way as Git `parse_config_key(..., "bundle", ...)`.
fn parse_bundle_key(full_key: &str) -> Option<(Option<String>, String)> {
    let rest = full_key.strip_prefix("bundle.")?;
    if rest.is_empty() {
        return None;
    }
    match rest.rsplit_once('.') {
        Some((subsection, subkey)) if !subsection.is_empty() && !subkey.is_empty() => {
            Some((Some(subsection.to_string()), subkey.to_string()))
        }
        _ => Some((None, rest.to_string())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleUpdateOutcome {
    Ok,
    Ignored,
    DuplicateUri,
}

fn bundle_list_update(key: &str, value: &str, list: &mut BundleList) -> BundleUpdateOutcome {
    let Some((subsection, subkey)) = parse_bundle_key(key) else {
        return BundleUpdateOutcome::Ignored;
    };
    let subkey_lc = subkey.to_ascii_lowercase();

    if subsection.is_none() {
        match subkey_lc.as_str() {
            "version" => {
                if let Ok(v) = value.parse::<i32>() {
                    if v == 1 {
                        list.version = v;
                        return BundleUpdateOutcome::Ok;
                    }
                }
                BundleUpdateOutcome::Ignored
            }
            "mode" => {
                if value == "all" {
                    list.mode = BundleMode::All;
                    BundleUpdateOutcome::Ok
                } else if value == "any" {
                    list.mode = BundleMode::Any;
                    BundleUpdateOutcome::Ok
                } else {
                    BundleUpdateOutcome::Ignored
                }
            }
            "heuristic" => {
                if value.eq_ignore_ascii_case("creationtoken") {
                    list.heuristic = BundleHeuristic::CreationToken;
                    BundleUpdateOutcome::Ok
                } else {
                    BundleUpdateOutcome::Ignored
                }
            }
            _ => BundleUpdateOutcome::Ignored,
        }
    } else {
        let Some(id) = subsection else {
            return BundleUpdateOutcome::Ignored;
        };
        let entry = list
            .bundles
            .entry(id.clone())
            .or_insert_with(|| RemoteBundleInfo {
                id,
                uri: None,
                creation_token: None,
            });

        if subkey_lc == "uri" {
            if entry.uri.is_some() {
                return BundleUpdateOutcome::DuplicateUri;
            }
            let base = list.base_uri.as_deref().unwrap_or(TEST_BASE_URI);
            let resolved = match git_path::relative_url(base, value, None) {
                Ok(u) => u,
                Err(_) => {
                    eprintln!("fatal: cannot strip one component off url '{base}'");
                    std::process::exit(128);
                }
            };
            entry.uri = Some(resolved);
            BundleUpdateOutcome::Ok
        } else if subkey_lc == "creationtoken" {
            match value.parse::<u64>() {
                Ok(t) => {
                    entry.creation_token = Some(t);
                    BundleUpdateOutcome::Ok
                }
                Err(_) => {
                    eprintln!(
                        "warning: could not parse bundle list key creationToken with value '{value}'"
                    );
                    BundleUpdateOutcome::Ok
                }
            }
        } else {
            BundleUpdateOutcome::Ignored
        }
    }
}

fn print_bundle_list(list: &BundleList) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mode_str = match list.mode {
        BundleMode::All => "all",
        BundleMode::Any => "any",
        BundleMode::None => "<unknown>",
    };
    writeln!(out, "[bundle]")?;
    writeln!(out, "\tversion = {}", list.version)?;
    writeln!(out, "\tmode = {mode_str}")?;
    if let Some(name) = heuristic_name(list.heuristic) {
        writeln!(out, "\theuristic = {name}")?;
    }

    let mut entries: Vec<_> = list.bundles.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (id, info) in entries {
        writeln!(out, "[bundle \"{id}\"]")?;
        if let Some(ref u) = info.uri {
            writeln!(out, "\turi = {u}")?;
        } else {
            writeln!(out, "\t# uri = (missing)")?;
        }
        if let Some(t) = info.creation_token {
            if t != 0 {
                writeln!(out, "\tcreationToken = {t}")?;
            }
        }
    }
    Ok(())
}

/// `bundle_uri_parse_line` + error handling like Git `test-bundle-uri.c`.
pub(crate) fn parse_key_values_file(path: &str) -> Result<i32> {
    let mut list = BundleList::new();
    list.base_uri = Some(TEST_BASE_URI.to_string());

    let file = fs::File::open(path).map_err(|e| anyhow::anyhow!("failed to open '{path}': {e}"))?;
    let reader = std::io::BufReader::new(file);
    let mut err = 0i32;

    for line in reader.lines() {
        let line = line?;
        if bundle_uri_parse_line(&mut list, &line).is_err() {
            err = 1;
            eprintln!("error: bad line: '{line}'");
        }
    }

    print_bundle_list(&list)?;
    Ok(err)
}

fn bundle_uri_parse_line(list: &mut BundleList, line: &str) -> Result<(), ()> {
    if line.is_empty() {
        eprintln!("error: bundle-uri: got an empty line");
        return Err(());
    }
    let Some(eq) = line.find('=') else {
        eprintln!("error: bundle-uri: line is not of the form 'key=value'");
        return Err(());
    };
    if eq == 0 || eq + 1 >= line.len() {
        eprintln!("error: bundle-uri: line has empty key or value");
        return Err(());
    }
    let key = &line[..eq];
    let value = &line[eq + 1..];
    match bundle_list_update(key, value, list) {
        BundleUpdateOutcome::DuplicateUri => Err(()),
        _ => Ok(()),
    }
}

#[derive(Default, Clone)]
struct IniParserState {
    section_bundle: bool,
    subsection: Option<String>,
}

/// Validate a config line like Git's core scanner: first content character on an entry line must
/// be alphabetic; `key = value` must have non-empty trimmed key and value.
///
/// Returns `false` when the line is invalid (after printing the same message as Git's config
/// parser).
fn validate_git_config_entry_line(raw: &str, line_no: usize, path: &Path) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
        return true;
    }
    if trimmed.starts_with('[') {
        return true;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return true;
    }
    if !bytes[i].is_ascii_alphabetic() {
        eprintln!(
            "error: bad config line {} in file {}",
            line_no,
            path.display()
        );
        return false;
    }
    let Some(eq_pos) = trimmed.find('=') else {
        eprintln!(
            "error: bad config line {} in file {}",
            line_no,
            path.display()
        );
        return false;
    };
    let key_part = trimmed[..eq_pos].trim();
    let value_part = strip_inline_comment_config(trimmed[eq_pos + 1..].trim());
    if key_part.is_empty() || value_part.trim().is_empty() {
        eprintln!(
            "error: bad config line {} in file {}",
            line_no,
            path.display()
        );
        return false;
    }
    true
}

/// Strip `#` / `;` comments not inside double quotes (Git config semantics, simplified).
fn strip_inline_comment_config(s: &str) -> &str {
    let mut in_quote = false;
    let chars = s.char_indices().peekable();
    for (i, ch) in chars {
        match ch {
            '"' => in_quote = !in_quote,
            '#' | ';' if !in_quote => return &s[..i],
            _ => {}
        }
    }
    s
}

/// Build `bundle.full.key` from current ini section and raw variable name.
fn make_config_key(state: &IniParserState, raw_var: &str) -> Option<String> {
    if !state.section_bundle {
        return None;
    }
    let var = raw_var.trim();
    let var_lc = var.to_ascii_lowercase();
    match &state.subsection {
        None => Some(format!("bundle.{var_lc}")),
        Some(sub) => Some(format!("bundle.{sub}.{var_lc}")),
    }
}

pub(crate) fn parse_config_file(uri_param: &str, path: &str) -> Result<i32> {
    let path = Path::new(path);
    let content = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to open '{}': {e}", path.display()))?;

    let mut list = BundleList::new();
    list.base_uri = Some(if uri_param == TEST_BASE_URI {
        TEST_BASE_URI.to_string()
    } else {
        let mut b = uri_param.to_string();
        if !b.ends_with('/') {
            if let Some(pos) = b.rfind('/') {
                b.truncate(pos + 1);
            } else {
                b.clear();
            }
        }
        b
    });

    let mut err = 0i32;
    let mut state = IniParserState::default();
    let raw_lines: Vec<&str> = content.lines().collect();
    let mut idx = 0usize;

    while idx < raw_lines.len() {
        let line_no = idx + 1;
        let line = raw_lines[idx].strip_suffix('\r').unwrap_or(raw_lines[idx]);
        idx += 1;

        if line.trim().starts_with('#') || line.trim().starts_with(';') {
            continue;
        }

        // Continuation lines (backslash at end of value portion)
        let mut logical = line.to_string();
        while line_continues(&logical) && idx < raw_lines.len() {
            let t = logical.trim_end();
            logical = format!("{}{}", &t[..t.len() - 1], raw_lines[idx].trim_start());
            idx += 1;
        }

        let trimmed = logical.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('[') {
            if let Some(s) = parse_section_header(trimmed) {
                state = s;
            } else {
                state = IniParserState::default();
            }
            continue;
        }

        if !validate_git_config_entry_line(&logical, line_no, path) {
            err = 1;
            continue;
        }

        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        let raw_name = trimmed[..eq].trim();
        let raw_value = strip_inline_comment_config(trimmed[eq + 1..].trim());
        let value = unescape_config_value(raw_value.trim());

        let Some(full_key) = make_config_key(&state, raw_name) else {
            continue;
        };

        if bundle_list_update(&full_key, &value, &mut list) == BundleUpdateOutcome::DuplicateUri {
            err = 1;
            eprintln!("error: bad line: '{trimmed}'");
        }
    }

    if list.mode == BundleMode::None {
        eprintln!("warning: bundle list at '{}' has no mode", uri_param);
        err = 1;
    }

    for (id, info) in &list.bundles {
        if info.uri.is_none() {
            eprintln!("error: bundle list at '{uri_param}': bundle '{id}' has no uri");
            err = 1;
        }
    }

    print_bundle_list(&list)?;
    Ok(err)
}

fn line_continues(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
        return false;
    }
    let Some(pos) = trimmed.find('=') else {
        return false;
    };
    let value_part = &trimmed[pos + 1..];
    let mut in_quote = false;
    let mut escaped = false;
    let mut in_comment = false;
    let mut last_bs = false;
    for ch in value_part.chars() {
        if in_comment {
            last_bs = false;
            continue;
        }
        match ch {
            '"' if !escaped => in_quote = !in_quote,
            '\\' if !escaped => {
                escaped = true;
                last_bs = true;
                continue;
            }
            '#' | ';' if !in_quote && !escaped => in_comment = true,
            _ => {}
        }
        escaped = false;
        last_bs = false;
    }
    last_bs && !in_comment
}

fn unescape_config_value(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&n) = chars.peek() {
                chars.next();
                if n == '\n' {
                    continue;
                }
                out.push(n);
                continue;
            }
        }
        out.push(ch);
    }
    out
}

fn parse_section_header(line: &str) -> Option<IniParserState> {
    let t = line.trim();
    let end = t.find(']')?;
    let inside = t.get(1..end)?.trim();
    if inside == "bundle" {
        return Some(IniParserState {
            section_bundle: true,
            subsection: None,
        });
    }
    // `[bundle "subsection"]` — subsection may contain escapes.
    let prefix = "bundle \"";
    if inside.starts_with(prefix) && inside.ends_with('"') {
        let quoted = &inside[prefix.len()..inside.len() - 1];
        let mut unescaped = String::new();
        let mut ch = quoted.chars().peekable();
        while let Some(c) = ch.next() {
            if c == '\\' {
                if let Some(n) = ch.next() {
                    unescaped.push(n);
                }
            } else {
                unescaped.push(c);
            }
        }
        return Some(IniParserState {
            section_bundle: true,
            subsection: Some(unescaped),
        });
    }
    None
}
