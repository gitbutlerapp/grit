//! Library hygiene ratchet: counts anti-patterns under `grit-lib/src` and compares
//! to [`hygiene-baseline.toml`](../hygiene-baseline.toml).
//!
//! `#[cfg(test)]` modules and items are stripped before counting (brace matching).
//! Lines exempted with `// hygiene: <reason>` on the same line or the line above are
//! tracked separately and do not contribute to ratchet counts.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Stable pattern names (also keys in the baseline TOML).
const PATTERNS: &[&str] = &[
    "println",
    "print",
    "eprintln",
    "eprint",
    "dbg",
    "process_exit",
    "env_var",
    "env_var_os",
    "env_vars",
    "env_set_var",
    "env_remove_var",
    "env_current_dir",
    "literal_fatal",
    "literal_error",
    "literal_hint",
    "literal_warning",
    "command_new",
    "static_global",
    "system_time_now",
];

type PatternCounts = BTreeMap<&'static str, u32>;

#[test]
fn hygiene_ratchet_matches_baseline() {
    if std::env::var_os("HYGIENE_REGEN").is_some() {
        let src = manifest_src_dir();
        let baseline_path = manifest_dir().join("hygiene-baseline.toml");
        let (counts, _exempt) = scan_tree(&src);
        write_baseline(&baseline_path, &counts);
        eprintln!("regenerated {}", baseline_path.display());
        return;
    }
    let src = manifest_src_dir();
    let baseline_path = manifest_dir().join("hygiene-baseline.toml");
    let baseline = load_baseline(&baseline_path);
    let (counts, exempt) = scan_tree(&src);

    let total_nonzero: u32 = counts
        .values()
        .flat_map(|file| PATTERNS.iter().map(move |p| *file.get(p).unwrap_or(&0)))
        .sum();
    assert_eq!(
        total_nonzero, 0,
        "grit-lib hygiene: all non-exempt pattern counts must be zero (got {total_nonzero}); \
         fix code or add `// hygiene: reason` on exempt lines only"
    );

    let mut failures: Vec<String> = Vec::new();

    for rel in counts.keys().chain(baseline.files.keys()) {
        let file_counts = counts.get(rel.as_str()).cloned().unwrap_or_default();
        let base_counts = baseline
            .files
            .get(rel.as_str())
            .cloned()
            .unwrap_or_default();
        for pattern in PATTERNS {
            let cur = *file_counts.get(pattern).unwrap_or(&0);
            let base = *base_counts.get(*pattern).unwrap_or(&0);
            if cur > base {
                failures.push(format!(
                    "  {rel}:{pattern} increased {base} -> {cur} (lower the code or add // hygiene: reason)"
                ));
            } else if cur < base {
                failures.push(format!(
                    "  {rel}:{pattern} decreased {base} -> {cur} (update hygiene-baseline.toml to ratchet down)"
                ));
            }
        }
    }

    for (rel, reasons) in &exempt {
        if !reasons.is_empty() {
            let _ = rel;
            // Exempt hits are informational only; listed when the test passes via `--nocapture`.
            eprintln!("exempt {rel}: {} line(s)", reasons.len());
        }
    }

    if !failures.is_empty() {
        eprintln!("\ngrit-lib hygiene ratchet failed:\n");
        format_failure_table(&counts, &baseline.files, &failures);
        panic!("{} hygiene violation(s); see table above", failures.len());
    }
}

struct Baseline {
    files: BTreeMap<String, BTreeMap<String, u32>>,
}

fn load_baseline(path: &Path) -> Baseline {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read baseline {}: {e}", path.display()));
    parse_baseline(&text)
}

fn parse_baseline(text: &str) -> Baseline {
    let mut files: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
    let mut current_file: Option<String> = None;

    for line in text.lines() {
        let line = line.split('#').next().unwrap_or(line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let inner = &line[1..line.len() - 1];
            if let Some(rest) = inner.strip_prefix("file.") {
                current_file = Some(rest.to_string());
                files.entry(rest.to_string()).or_default();
            } else {
                current_file = None;
            }
            continue;
        }
        if current_file.is_none() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if value.starts_with('"') {
                continue;
            }
            let value: u32 = value
                .parse()
                .unwrap_or_else(|_| panic!("invalid baseline integer: {line}"));
            if let Some(ref file) = current_file {
                files
                    .entry(file.clone())
                    .or_default()
                    .insert(key.to_string(), value);
            }
        }
    }

    Baseline { files }
}

fn write_baseline(path: &Path, counts: &BTreeMap<String, PatternCounts>) {
    let mut out = String::from("# grit-lib/src hygiene counts — ratchet baseline\n");
    out.push_str("# Regenerate with: HYGIENE_REGEN=1 cargo test -p grit-lib --test hygiene\n\n");
    out.push_str("[meta]\n");
    out.push_str("updated = \"2026-10-08\"\n\n");

    for (rel, file_counts) in counts {
        out.push_str(&format!("[file.{rel}]\n"));
        for pattern in PATTERNS {
            let n = *file_counts.get(pattern).unwrap_or(&0);
            out.push_str(&format!("{pattern} = {n}\n"));
        }
        out.push('\n');
    }

    fs::write(path, out).unwrap_or_else(|e| panic!("write baseline {}: {e}", path.display()));
}

fn scan_tree(
    src: &Path,
) -> (
    BTreeMap<String, PatternCounts>,
    BTreeMap<String, Vec<String>>,
) {
    let mut counts: BTreeMap<String, PatternCounts> = BTreeMap::new();
    let mut exempt: BTreeMap<String, Vec<String>> = BTreeMap::new();
    walk_rs_files(src, src, &mut counts, &mut exempt);
    (counts, exempt)
}

fn walk_rs_files(
    root: &Path,
    dir: &Path,
    counts: &mut BTreeMap<String, PatternCounts>,
    exempt: &mut BTreeMap<String, Vec<String>>,
) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk_rs_files(root, &path, counts, exempt);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let stripped = strip_cfg_test_modules(&source);
            let (file_counts, file_exempt) = count_patterns(&stripped, &source);
            if file_counts.values().any(|&n| n > 0) {
                counts.insert(rel.clone(), file_counts);
            }
            if !file_exempt.is_empty() {
                exempt.insert(rel, file_exempt);
            }
        }
    }
}

/// Blank out `#[cfg(test)]` items but keep newlines so line numbers match the original file.
fn strip_cfg_test_modules(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut keep = vec![true; bytes.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(skip_end) = cfg_test_item_end(bytes, i) {
            for b in &mut keep[i..skip_end] {
                *b = false;
            }
            i = skip_end;
            continue;
        }
        i += 1;
    }
    bytes
        .iter()
        .zip(keep.iter())
        .map(|(&b, &k)| {
            if k {
                b as char
            } else if b == b'\n' {
                '\n'
            } else {
                ' '
            }
        })
        .collect()
}

fn cfg_test_item_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    i = skip_ascii_whitespace(bytes, i)?;
    if i + 1 >= bytes.len() || bytes[i] != b'#' {
        return None;
    }
    if !starts_with(bytes, i, "#[cfg(test)]") {
        return None;
    }
    i += "#[cfg(test)]".len();
    loop {
        i = skip_ascii_whitespace(bytes, i)?;
        if starts_with(bytes, i, "#[") {
            i = skip_attribute(bytes, i)?;
            continue;
        }
        break;
    }
    let item_start = i;
    i = skip_item(bytes, i)?;
    Some(i.max(item_start))
}

fn skip_attribute(bytes: &[u8], mut i: usize) -> Option<usize> {
    if !starts_with(bytes, i, "#[") {
        return Some(i);
    }
    let mut depth = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            depth += 1;
        } else if bytes[i] == b']' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i + 1);
            }
        } else if bytes[i] == b'"' || bytes[i] == b'\'' {
            i = skip_string(bytes, i)?;
            continue;
        }
        i += 1;
    }
    None
}

fn skip_item(bytes: &[u8], mut i: usize) -> Option<usize> {
    i = skip_ascii_whitespace(bytes, i)?;
    if i >= bytes.len() {
        return Some(i);
    }
    // Skip visibility and unsafe/const/async/extern "C" prefixes loosely.
    loop {
        i = skip_ascii_whitespace(bytes, i)?;
        if starts_with(bytes, i, "pub ") {
            i += 4;
            if starts_with(bytes, i, "(") {
                i = skip_balanced(bytes, i, b'(', b')')? + 1;
            }
            continue;
        }
        if starts_with(bytes, i, "pub(") {
            i = skip_balanced(bytes, i, b'(', b')')? + 1;
            continue;
        }
        if starts_with(bytes, i, "unsafe ")
            || starts_with(bytes, i, "const ")
            || starts_with(bytes, i, "async ")
            || starts_with(bytes, i, "default ")
        {
            i = bytes[i..]
                .iter()
                .position(|c| c.is_ascii_whitespace())
                .map(|p| i + p + 1)
                .unwrap_or(bytes.len());
            continue;
        }
        if starts_with(bytes, i, "extern ") {
            i += 7;
            i = skip_ascii_whitespace(bytes, i)?;
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                i = skip_string(bytes, i)? + 1;
            }
            continue;
        }
        break;
    }

    if starts_with(bytes, i, "mod ") {
        i += 4;
        i = skip_ident(bytes, i)?;
        i = skip_ascii_whitespace(bytes, i)?;
        if i < bytes.len() && bytes[i] == b';' {
            return Some(i + 1);
        }
        if i < bytes.len() && bytes[i] == b'{' {
            return skip_balanced(bytes, i, b'{', b'}').map(|e| e + 1);
        }
        return Some(i);
    }

    // macro_rules!, thread_local!, etc.
    if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
        i = skip_ident(bytes, i)?;
        i = skip_ascii_whitespace(bytes, i)?;
        if i < bytes.len() && bytes[i] == b'!' {
            i += 1;
            i = skip_ascii_whitespace(bytes, i)?;
            if i < bytes.len() && bytes[i] == b'{' {
                return skip_balanced(bytes, i, b'{', b'}').map(|e| e + 1);
            }
            if i < bytes.len() && bytes[i] == b'(' {
                return skip_balanced(bytes, i, b'(', b')').map(|e| e + 1);
            }
            return Some(i);
        }
        // fn/struct/enum/impl/trait/type/const/static/use — scan to body or ;
        if i < bytes.len() && bytes[i] == b'(' {
            i = skip_balanced(bytes, i, b'(', b')')? + 1;
        }
        i = skip_ascii_whitespace(bytes, i)?;
        while i < bytes.len() && bytes[i] != b'{' && bytes[i] != b';' {
            if bytes[i] == b'(' {
                i = skip_balanced(bytes, i, b'(', b')')? + 1;
            } else if bytes[i] == b'<' {
                i = skip_balanced(bytes, i, b'<', b'>')? + 1;
            } else {
                i += 1;
            }
        }
        if i < bytes.len() && bytes[i] == b';' {
            return Some(i + 1);
        }
        if i < bytes.len() && bytes[i] == b'{' {
            return skip_balanced(bytes, i, b'{', b'}').map(|e| e + 1);
        }
        return Some(i);
    }

    Some(i)
}

fn skip_ident(bytes: &[u8], mut i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return Some(i);
    }
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    Some(i)
}

fn skip_string(bytes: &[u8], mut i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return Some(i);
    }
    let quote = bytes[i];
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return Some(i);
        }
        i += 1;
    }
    Some(bytes.len().saturating_sub(1))
}

fn skip_balanced(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut i = start;
    if i >= bytes.len() || bytes[i] != open {
        return Some(start);
    }
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            c if c == open => depth += 1,
            c if c == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' | b'\'' => {
                i = skip_string(bytes, i)?;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_ascii_whitespace(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if starts_with(bytes, i, "//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if starts_with(bytes, i, "/*") {
            i += 2;
            while i + 1 < bytes.len() && !starts_with(bytes, i, "*/") {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        break;
    }
    Some(i)
}

fn starts_with(bytes: &[u8], i: usize, needle: &str) -> bool {
    bytes[i..].starts_with(needle.as_bytes())
}

fn count_patterns(stripped: &str, original: &str) -> (PatternCounts, Vec<String>) {
    let mut counts = empty_counts();
    let mut exempt_reasons: Vec<String> = Vec::new();

    let stripped_lines: Vec<&str> = stripped.lines().collect();
    let orig_lines: Vec<&str> = original.lines().collect();

    for (idx, line) in stripped_lines.iter().enumerate() {
        let orig_line = orig_lines.get(idx).copied().unwrap_or(line);
        if is_exempt(orig_line, orig_lines.get(idx.wrapping_sub(1)).copied()) {
            if pattern_hits(line).iter().any(|(_, n)| *n > 0) {
                exempt_reasons.push(format!("L{}: {orig_line}", idx + 1));
            }
            continue;
        }
        for (pattern, n) in pattern_hits(line) {
            if n > 0 {
                *counts.get_mut(pattern).unwrap() += n;
            }
        }
    }

    *counts.get_mut("static_global").unwrap() += count_static_global_items(stripped, &orig_lines);

    (counts, exempt_reasons)
}

fn is_exempt(line: &str, prev: Option<&str>) -> bool {
    let has_marker = |s: &str| s.contains("// hygiene:");
    has_marker(line) || prev.is_some_and(has_marker)
}

fn empty_counts() -> PatternCounts {
    PATTERNS.iter().copied().map(|p| (p, 0)).collect()
}

/// Count `name!(` invocations, not prefix matches inside `println!` / `eprintln!` / `eprint!`.
fn count_macro(code: &str, name: &str) -> u32 {
    let needle = format!("{name}!(");
    let mut n = 0u32;
    let mut start = 0usize;
    while let Some(rel) = code[start..].find(&needle) {
        let abs = start + rel;
        if abs > 0 {
            let prev = code.as_bytes()[abs - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                start = abs + 1;
                continue;
            }
        }
        let rest = &code[abs..];
        if name == "print"
            && (rest.starts_with("println!(")
                || rest.starts_with("eprintln!(")
                || rest.starts_with("eprint!("))
        {
            start = abs + 1;
            continue;
        }
        if name == "eprint" && rest.starts_with("eprintln!(") {
            start = abs + 1;
            continue;
        }
        n += 1;
        start = abs + needle.len();
    }
    n
}

fn count_substring_occurrences(code: &str, needle: &str) -> u32 {
    let mut n = 0u32;
    let mut start = 0usize;
    while let Some(rel) = code[start..].find(needle) {
        n += 1;
        start = start + rel + needle.len();
    }
    n
}

fn count_std_env_call(code: &str, call: &str, exclude_longer: &str) -> u32 {
    let mut n = 0u32;
    let mut start = 0usize;
    while let Some(rel) = code[start..].find(call) {
        let abs = start + rel;
        if code[abs..].starts_with(exclude_longer) {
            start = abs + 1;
            continue;
        }
        n += 1;
        start = abs + call.len();
    }
    n
}

fn count_env_var(code: &str) -> u32 {
    count_std_env_call(code, "std::env::var(", "std::env::var_os(")
        + count_standalone_env_call(code, "env::var(", Some("env::var_os("))
}

fn count_standalone_env_call(code: &str, call: &str, exclude_longer: Option<&str>) -> u32 {
    let mut n = 0u32;
    let mut start = 0usize;
    while let Some(rel) = code[start..].find(call) {
        let abs = start + rel;
        if exclude_longer.is_some_and(|ex| code[abs..].starts_with(ex)) {
            start = abs + 1;
            continue;
        }
        if code
            .get(..abs)
            .is_some_and(|before| before.ends_with("std::"))
        {
            start = abs + 1;
            continue;
        }
        n += 1;
        start = abs + call.len();
    }
    n
}

fn count_system_time_now(code: &str) -> u32 {
    count_substring_occurrences(code, "std::time::SystemTime::now")
        + count_system_time_now_short(code)
}

fn count_system_time_now_short(code: &str) -> u32 {
    let mut n = 0u32;
    let mut start = 0usize;
    while let Some(rel) = code[start..].find("SystemTime::now") {
        let abs = start + rel;
        if code
            .get(..abs)
            .is_some_and(|before| before.ends_with("std::time::"))
        {
            start = abs + 1;
            continue;
        }
        n += 1;
        start = abs + "SystemTime::now".len();
    }
    n
}

fn count_static_global_items(stripped: &str, orig_lines: &[&str]) -> u32 {
    let mut count = 0u32;
    let bytes = stripped.as_bytes();
    let mut search = 0usize;
    while search < stripped.len() {
        let tail = &stripped[search..];
        let next_tl = tail.find("thread_local!");
        let next_static = tail.find("static");
        let advance = match (next_tl, next_static) {
            (None, None) => break,
            (Some(t), None) => (t, Some(true)),
            (None, Some(s)) => (s, Some(false)),
            (Some(t), Some(s)) => {
                if t <= s {
                    (t, Some(true))
                } else {
                    (s, Some(false))
                }
            }
        };
        let abs = search + advance.0;
        if advance.1 == Some(true) {
            let line = line_index_for_byte(stripped, abs);
            if !line_is_exempt(line, orig_lines) {
                count += 1;
            }
            search = abs + "thread_local!".len();
            continue;
        }
        if is_static_item_start(stripped, abs) {
            if let Some(end) = static_item_end(bytes, abs) {
                let start_line = line_index_for_byte(stripped, abs);
                let end_line = line_index_for_byte(stripped, end.saturating_sub(1));
                if !lines_range_exempt(start_line, end_line, orig_lines) {
                    let item = &stripped[abs..end];
                    if item.contains("OnceLock")
                        || item.contains("Mutex")
                        || item.contains("RwLock")
                    {
                        count += 1;
                    }
                }
                search = end;
                continue;
            }
        }
        search = abs + 1;
    }
    count
}

fn line_index_for_byte(source: &str, byte: usize) -> usize {
    source[..byte.min(source.len())].matches('\n').count()
}

fn line_is_exempt(line_idx: usize, orig_lines: &[&str]) -> bool {
    let orig = orig_lines.get(line_idx).copied().unwrap_or("");
    let prev = line_idx
        .checked_sub(1)
        .and_then(|i| orig_lines.get(i).copied());
    is_exempt(orig, prev)
}

fn lines_range_exempt(start: usize, end: usize, orig_lines: &[&str]) -> bool {
    (start..=end).any(|line| line_is_exempt(line, orig_lines))
}

fn is_static_item_start(source: &str, abs: usize) -> bool {
    if !source.is_char_boundary(abs) {
        return false;
    }
    let bytes = source.as_bytes();
    if !bytes[abs..].starts_with(b"static") {
        return false;
    }
    let before_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric() && bytes[abs - 1] != b'_';
    let after = abs + 6;
    let after_ok =
        after >= bytes.len() || bytes[after].is_ascii_whitespace() || bytes[after] == b'_';
    before_ok && after_ok
}

fn static_item_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut angle = 0i32;
    let mut paren = 0i32;
    let mut brace = 0i32;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            i = skip_string(bytes, i)? + 1;
            continue;
        }
        match b {
            b'<' => angle += 1,
            b'>' => angle -= 1,
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'{' => brace += 1,
            b'}' => brace -= 1,
            b';' if angle == 0 && paren == 0 && brace == 0 => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn pattern_hits(line: &str) -> PatternCounts {
    let code = line.split("//").next().unwrap_or(line);
    let mut counts = empty_counts();

    macro_rules! bump_macro {
        ($key:expr, $name:expr) => {
            let hits = count_macro(code, $name);
            if hits > 0 {
                *counts.get_mut($key).unwrap() += hits;
            }
        };
    }
    macro_rules! bump_count {
        ($key:expr, $expr:expr) => {
            let hits = $expr;
            if hits > 0 {
                *counts.get_mut($key).unwrap() += hits;
            }
        };
    }

    bump_macro!("println", "println");
    bump_macro!("print", "print");
    bump_macro!("eprintln", "eprintln");
    bump_macro!("eprint", "eprint");
    bump_count!("dbg", count_macro(code, "dbg"));
    bump_count!(
        "process_exit",
        count_substring_occurrences(code, "process::exit")
    );
    bump_count!("env_var", count_env_var(code));
    bump_count!(
        "env_var_os",
        count_substring_occurrences(code, "std::env::var_os(")
            + count_standalone_env_call(code, "env::var_os(", None)
    );
    bump_count!(
        "env_vars",
        count_substring_occurrences(code, "std::env::vars(")
            + count_standalone_env_call(code, "env::vars(", None)
    );
    bump_count!(
        "env_set_var",
        count_substring_occurrences(code, "std::env::set_var(")
            + count_standalone_env_call(code, "env::set_var(", None)
    );
    bump_count!(
        "env_remove_var",
        count_substring_occurrences(code, "std::env::remove_var(")
            + count_standalone_env_call(code, "env::remove_var(", None)
    );
    bump_count!(
        "env_current_dir",
        count_substring_occurrences(code, "std::env::current_dir(")
            + count_standalone_env_call(code, "env::current_dir(", None)
    );
    bump_count!(
        "literal_fatal",
        count_substring_occurrences(code, "\"fatal: ")
    );
    bump_count!(
        "literal_error",
        count_substring_occurrences(code, "\"error: ")
    );
    bump_count!(
        "literal_hint",
        count_substring_occurrences(code, "\"hint: ")
    );
    bump_count!(
        "literal_warning",
        count_substring_occurrences(code, "\"warning: ")
    );
    bump_count!(
        "command_new",
        count_substring_occurrences(code, "Command::new")
    );
    bump_count!("system_time_now", count_system_time_now(code));

    counts
}

fn format_failure_table(
    current: &BTreeMap<String, PatternCounts>,
    baseline: &BTreeMap<String, BTreeMap<String, u32>>,
    failures: &[String],
) {
    for msg in failures {
        eprintln!("{msg}");
    }
    eprintln!("\n--- counts (current vs baseline), non-zero only ---");
    let mut files: BTreeMap<String, ()> = BTreeMap::new();
    for k in current.keys() {
        files.insert(k.clone(), ());
    }
    for k in baseline.keys() {
        files.insert(k.clone(), ());
    }
    for rel in files.keys() {
        let cur = current.get(rel).cloned().unwrap_or_default();
        let base = baseline.get(rel).cloned().unwrap_or_default();
        for pattern in PATTERNS {
            let c = *cur.get(pattern).unwrap_or(&0);
            let b = *base.get(*pattern).unwrap_or(&0);
            if c != 0 || b != 0 {
                eprintln!("  {rel}:{pattern}  baseline={b}  current={c}");
            }
        }
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn manifest_src_dir() -> PathBuf {
    manifest_dir().join("src")
}

#[test]
fn hygiene_scanner_detects_eprintln_in_active_code() {
    let sample = r#"
fn ok() {}
#[cfg(test)]
mod tests {
    fn t() { eprintln!("test"); }
}
fn bad() { eprintln!("nope"); }
"#;
    let stripped = strip_cfg_test_modules(sample);
    let (counts, _) = count_patterns(&stripped, sample);
    assert_eq!(*counts.get("eprintln").unwrap_or(&0), 1);
    assert_eq!(*counts.get("println").unwrap_or(&0), 0);
}

#[test]
fn hygiene_scanner_counts_repeated_command_new_on_one_line() {
    let sample = r#"fn f() { Command::new("a"); Command::new("b"); }"#;
    let (counts, _) = count_patterns(sample, sample);
    assert_eq!(*counts.get("command_new").unwrap_or(&0), 2);
}

#[test]
fn hygiene_scanner_counts_multiline_static_global() {
    let sample = r"
static STATE:
    OnceLock<u8> = OnceLock::new();
";
    let (counts, _) = count_patterns(sample, sample);
    assert_eq!(*counts.get("static_global").unwrap_or(&0), 1);
}
