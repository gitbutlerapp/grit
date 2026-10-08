//! Commit message trailer parsing and rewriting (Git-compatible).
//!
//! Behaviour matches upstream `git/trailer.c` / `git interpret-trailers`.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{ConfigEntry, ConfigSet};

const CUT_LINE: &str = "------------------------ >8 ------------------------";

const GIT_GENERATED_PREFIXES: &[&str] = &["Signed-off-by: ", "(cherry picked from commit "];

const TRAILER_ARG_PLACEHOLDER: &str = "$ARG";

/// Placement of a new trailer relative to an anchor trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailerWhere {
    #[default]
    Default,
    End,
    After,
    Before,
    Start,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailerIfExists {
    #[default]
    Default,
    AddIfDifferentNeighbor,
    AddIfDifferent,
    Add,
    Replace,
    DoNothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailerIfMissing {
    #[default]
    Default,
    Add,
    DoNothing,
}

#[derive(Debug, Clone, Default)]
pub struct ProcessTrailerOptions {
    pub trim_empty: bool,
    pub only_trailers: bool,
    pub only_input: bool,
    pub unfold: bool,
    pub no_divider: bool,
}

#[derive(Debug, Clone)]
pub struct NewTrailerArg {
    pub text: String,
    pub where_: TrailerWhere,
    pub if_exists: TrailerIfExists,
    pub if_missing: TrailerIfMissing,
}

#[derive(Debug, Clone)]
struct ConfInfo {
    name: String,
    key: Option<String>,
    command: Option<String>,
    cmd: Option<String>,
    where_: TrailerWhere,
    if_exists: TrailerIfExists,
    if_missing: TrailerIfMissing,
}

#[derive(Debug, Clone)]
struct TrailerItem {
    token: Option<String>,
    value: String,
}

#[derive(Debug, Clone)]
struct ArgItem {
    token: String,
    value: String,
    conf: ConfInfo,
}

#[derive(Debug)]
struct TrailerBlock {
    blank_line_before: bool,
    start: usize,
    end: usize,
    lines: Vec<String>,
}

/// Parse `trailer.where` / `--where` values (Git spelling).
pub fn trailer_where_from_str(s: &str) -> Option<TrailerWhere> {
    set_where(s)
}

/// Parse `trailer.ifexists` / `--if-exists` values.
pub fn trailer_if_exists_from_str(s: &str) -> Option<TrailerIfExists> {
    set_if_exists(s)
}

/// Parse `trailer.ifmissing` / `--if-missing` values.
pub fn trailer_if_missing_from_str(s: &str) -> Option<TrailerIfMissing> {
    set_if_missing(s)
}

fn set_where(s: &str) -> Option<TrailerWhere> {
    match s.to_ascii_lowercase().as_str() {
        "after" => Some(TrailerWhere::After),
        "before" => Some(TrailerWhere::Before),
        "end" => Some(TrailerWhere::End),
        "start" => Some(TrailerWhere::Start),
        _ => None,
    }
}

fn set_if_exists(s: &str) -> Option<TrailerIfExists> {
    match s.to_ascii_lowercase().as_str() {
        "addifdifferent" => Some(TrailerIfExists::AddIfDifferent),
        "addifdifferentneighbor" => Some(TrailerIfExists::AddIfDifferentNeighbor),
        "add" => Some(TrailerIfExists::Add),
        "replace" => Some(TrailerIfExists::Replace),
        "donothing" => Some(TrailerIfExists::DoNothing),
        _ => None,
    }
}

fn set_if_missing(s: &str) -> Option<TrailerIfMissing> {
    match s.to_ascii_lowercase().as_str() {
        "add" => Some(TrailerIfMissing::Add),
        "donothing" => Some(TrailerIfMissing::DoNothing),
        _ => None,
    }
}

fn after_or_end(where_: TrailerWhere) -> bool {
    matches!(where_, TrailerWhere::After | TrailerWhere::End)
}

fn token_len_without_separator(token: &str) -> usize {
    let b = token.as_bytes();
    let mut len = token.len();
    while len > 0 && !b[len - 1].is_ascii_alphanumeric() {
        len -= 1;
    }
    len
}

fn same_token(a_token: Option<&str>, b_token: &str) -> bool {
    let Some(a) = a_token else {
        return false;
    };
    let a_len = token_len_without_separator(a);
    let b_len = token_len_without_separator(b_token);
    let min_len = a_len.min(b_len);
    a[..min_len].eq_ignore_ascii_case(&b_token[..min_len])
}

fn same_value(a: &TrailerItem, b_val: &str) -> bool {
    a.value.eq_ignore_ascii_case(b_val)
}

fn same_trailer(a: &TrailerItem, b: &ArgItem) -> bool {
    same_token(a.token.as_deref(), &b.token) && same_value(a, &b.value)
}

fn is_blank_line(s: &str) -> bool {
    s.chars().all(|c| c.is_whitespace())
}

fn last_non_space_char(s: &str) -> Option<char> {
    s.chars().rev().find(|c| !c.is_whitespace())
}

fn line_end(buf: &str, bol: usize, limit: usize) -> usize {
    bol + buf[bol..limit].find('\n').unwrap_or(limit - bol)
}

fn after_line(buf: &str, bol: usize, limit: usize) -> usize {
    let le = line_end(buf, bol, limit);
    if le < limit {
        le + 1
    } else {
        limit
    }
}

fn last_line_start(buf: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if len == 1 {
        return Some(0);
    }
    let slice = &buf.as_bytes()[..len];
    let mut i = len - 2;
    loop {
        if slice[i] == b'\n' {
            return Some(i + 1);
        }
        if i == 0 {
            return Some(0);
        }
        i -= 1;
    }
}

fn starts_with_comment_line(line: &str, prefix: &str) -> bool {
    line.starts_with(prefix)
}

fn wt_status_locate_end(s: &str, len: usize, comment_prefix: &str) -> usize {
    let pattern = format!("\n{comment_prefix} {CUT_LINE}\n");
    let head = format!("{comment_prefix} {CUT_LINE}\n");
    if s.len() >= head.len() && s[..head.len()] == head {
        return 0;
    }
    if let Some(p) = s[..len].find(&pattern) {
        let newlen = p + 1;
        if newlen < len {
            return newlen;
        }
    }
    len
}

fn ignored_log_message_bytes(buf: &str, len: usize, comment_prefix: &str) -> usize {
    let cutoff = wt_status_locate_end(buf, len, comment_prefix);
    let mut bol = 0usize;
    let mut boc = 0usize;
    let mut in_old_conflicts = false;

    while bol < cutoff {
        let le = line_end(buf, bol, cutoff);
        let line = &buf[bol..le];

        let is_comment = starts_with_comment_line(line, comment_prefix) || line.is_empty();
        if is_comment {
            if boc == 0 {
                boc = bol;
            }
        } else if line.starts_with("Conflicts:") {
            in_old_conflicts = true;
            if boc == 0 {
                boc = bol;
            }
        } else if in_old_conflicts && line.starts_with('\t') {
            // path in conflicts block
        } else if boc != 0 {
            boc = 0;
            in_old_conflicts = false;
        }

        bol = if le < cutoff { le + 1 } else { cutoff };
    }

    if boc != 0 {
        len - boc
    } else {
        len - cutoff
    }
}

fn find_end_of_log_message(input: &str, no_divider: bool, comment_prefix: &str) -> usize {
    let mut end = input.len();
    if !no_divider {
        let mut pos = 0usize;
        while pos < input.len() {
            let rest = &input[pos..];
            if rest.len() >= 3 && rest.as_bytes().get(0..3) == Some(b"---") {
                let after = rest.as_bytes().get(3).copied();
                if after.is_none() || after.is_some_and(|c| c.is_ascii_whitespace()) {
                    end = pos;
                    break;
                }
            }
            pos = after_line(input, pos, input.len());
            if pos >= input.len() {
                break;
            }
        }
    }
    end - ignored_log_message_bytes(input, end, comment_prefix)
}

fn find_separator(line: &str, separators: &str) -> Option<usize> {
    let mut whitespace_found = false;
    for (i, c) in line.char_indices() {
        if separators.contains(c) {
            return Some(i);
        }
        if !whitespace_found && (c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        if i > 0 && (c == ' ' || c == '\t') {
            whitespace_found = true;
            continue;
        }
        break;
    }
    None
}

fn token_matches_item(line: &str, item: &ConfInfo, sep_pos: usize) -> bool {
    let tok = line[..sep_pos].trim_end();
    let name = &item.name;
    let name_len = name.len();
    let tok_len = token_len_without_separator(tok);
    if tok_len >= name_len && tok[..name_len].eq_ignore_ascii_case(name) {
        return true;
    }
    if let Some(ref key) = item.key {
        let key_len = token_len_without_separator(key);
        if tok_len >= key_len && tok[..key_len].eq_ignore_ascii_case(&key[..key_len]) {
            return true;
        }
    }
    false
}

fn find_trailer_block_start(
    buf: &str,
    len: usize,
    conf: &[ConfInfo],
    separators: &str,
    comment_prefix: &str,
) -> usize {
    let mut end_of_title = len;
    let mut pos = 0usize;
    while pos < len {
        let le = line_end(buf, pos, len);
        let line = &buf[pos..le];
        if starts_with_comment_line(line, comment_prefix) {
            pos = if le < len { le + 1 } else { len };
            continue;
        }
        if is_blank_line(line) {
            end_of_title = pos;
            break;
        }
        pos = if le < len { le + 1 } else { len };
    }

    let mut l = last_line_start(buf, len);
    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines = 0i32;
    let mut non_trailer_lines = 0i32;
    let mut possible_continuation = 0i32;

    while let Some(bol) = l {
        if bol < end_of_title {
            break;
        }
        let le = line_end(buf, bol, len);
        let line = &buf[bol..le];

        if starts_with_comment_line(line, comment_prefix) {
            non_trailer_lines += possible_continuation;
            possible_continuation = 0;
            l = if bol == 0 {
                None
            } else {
                last_line_start(buf, bol)
            };
            continue;
        }

        if is_blank_line(line) {
            if only_spaces {
                l = if bol == 0 {
                    None
                } else {
                    last_line_start(buf, bol)
                };
                continue;
            }
            non_trailer_lines += possible_continuation;
            if recognized_prefix && trailer_lines * 3 >= non_trailer_lines {
                return after_line(buf, bol, len);
            }
            if trailer_lines > 0 && non_trailer_lines == 0 {
                return after_line(buf, bol, len);
            }
            return len;
        }
        only_spaces = false;

        let mut matched_gen = false;
        for p in GIT_GENERATED_PREFIXES {
            if line.starts_with(p) {
                trailer_lines += 1;
                possible_continuation = 0;
                recognized_prefix = true;
                matched_gen = true;
                break;
            }
        }
        if matched_gen {
            l = if bol == 0 {
                None
            } else {
                last_line_start(buf, bol)
            };
            continue;
        }

        if let Some(sep_pos) = find_separator(line, separators) {
            if sep_pos >= 1 && !line.starts_with(|c: char| c.is_whitespace()) {
                trailer_lines += 1;
                possible_continuation = 0;
                if !recognized_prefix {
                    for item in conf {
                        if token_matches_item(line, item, sep_pos) {
                            recognized_prefix = true;
                            break;
                        }
                    }
                }
            } else if line.starts_with(|c: char| c.is_whitespace()) {
                possible_continuation += 1;
            } else {
                non_trailer_lines += 1;
                non_trailer_lines += possible_continuation;
                possible_continuation = 0;
            }
        } else if line.starts_with(|c: char| c.is_whitespace()) {
            possible_continuation += 1;
        } else {
            non_trailer_lines += 1;
            non_trailer_lines += possible_continuation;
            possible_continuation = 0;
        }

        l = if bol == 0 {
            None
        } else {
            last_line_start(buf, bol)
        };
    }

    len
}

fn ends_with_blank_line(buf: &str, trailer_block_start: usize) -> bool {
    if trailer_block_start == 0 {
        return false;
    }
    let slice = &buf[..trailer_block_start];
    last_line_start(slice, slice.len()).is_some_and(|i| is_blank_line(&slice[i..]))
}

fn unfold_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            while chars.peek().is_some_and(|x| x.is_whitespace()) {
                chars.next();
            }
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

impl Default for ConfInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            key: None,
            command: None,
            cmd: None,
            where_: TrailerWhere::End,
            if_exists: TrailerIfExists::AddIfDifferentNeighbor,
            if_missing: TrailerIfMissing::Add,
        }
    }
}

fn duplicate_conf(src: &ConfInfo) -> ConfInfo {
    ConfInfo {
        name: src.name.clone(),
        key: src.key.clone(),
        command: src.command.clone(),
        cmd: src.cmd.clone(),
        where_: src.where_,
        if_exists: src.if_exists,
        if_missing: src.if_missing,
    }
}

fn token_from_item(item: &ConfInfo, tok_from_arg: Option<&str>) -> String {
    if let Some(k) = &item.key {
        return k.clone();
    }
    tok_from_arg.map_or_else(|| item.name.clone(), str::to_string)
}

fn parse_trailer_into(
    trailer: &str,
    separators: &str,
    conf: &[ConfInfo],
    apply_conf: bool,
) -> (String, String, ConfInfo) {
    let sep_pos = find_separator(trailer, separators);
    let (mut tok, val, mut picked) = if let Some(pos) = sep_pos {
        (
            trailer[..pos].trim().to_string(),
            trailer[pos + 1..].trim().to_string(),
            ConfInfo {
                name: String::new(),
                ..Default::default()
            },
        )
    } else {
        (
            trailer.trim().to_string(),
            String::new(),
            ConfInfo {
                name: String::new(),
                ..Default::default()
            },
        )
    };

    if apply_conf {
        let tok_len = token_len_without_separator(&tok);
        for item in conf {
            if tok_len >= item.name.len() && tok[..item.name.len()].eq_ignore_ascii_case(&item.name)
            {
                let tbuf = std::mem::take(&mut tok);
                tok = token_from_item(item, Some(&tbuf));
                picked = duplicate_conf(item);
                break;
            }
            if let Some(ref key) = item.key {
                let kl = token_len_without_separator(key);
                if tok_len >= kl && tok[..kl].eq_ignore_ascii_case(&key[..kl]) {
                    let tbuf = std::mem::take(&mut tok);
                    tok = token_from_item(item, Some(&tbuf));
                    picked = duplicate_conf(item);
                    break;
                }
            }
        }
    }

    (tok, val, picked)
}

fn var_ci_eq(s: &str, lit: &str) -> bool {
    s.eq_ignore_ascii_case(lit)
}

fn comment_line_prefix(cfg: &ConfigSet) -> String {
    match cfg.get("core.commentChar") {
        Some(s) => {
            let t = s.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("auto") {
                "#".to_string()
            } else {
                t.chars().next().unwrap_or('#').to_string()
            }
        }
        None => "#".to_string(),
    }
}

fn load_trailer_config(cfg: &ConfigSet) -> (ConfInfo, Vec<ConfInfo>, String) {
    // Git reads `trailer.*` globals first (`git_trailer_default_config`), then per-alias keys.
    // Each `trailer.<alias>.*` entry inherits the current global defaults at creation time.
    let hardcoded = ConfInfo::default();
    let mut default_conf = duplicate_conf(&hardcoded);
    let mut map: HashMap<String, ConfInfo> = HashMap::new();
    let mut separators = ":".to_string();

    for e in cfg.entries() {
        let ConfigEntry { key, value, .. } = e;
        let Some(rest) = key.strip_prefix("trailer.") else {
            continue;
        };
        if rest.rsplit_once('.').is_none() {
            if var_ci_eq(rest, "where") {
                if let Some(v) = value.as_deref().and_then(set_where) {
                    default_conf.where_ = v;
                }
            } else if var_ci_eq(rest, "ifexists") {
                if let Some(v) = value.as_deref().and_then(set_if_exists) {
                    default_conf.if_exists = v;
                }
            } else if var_ci_eq(rest, "ifmissing") {
                if let Some(v) = value.as_deref().and_then(set_if_missing) {
                    default_conf.if_missing = v;
                }
            } else if var_ci_eq(rest, "separators") {
                if let Some(v) = value {
                    separators = v.clone();
                }
            }
        }
    }

    for e in cfg.entries() {
        let ConfigEntry { key, value, .. } = e;
        let Some(rest) = key.strip_prefix("trailer.") else {
            continue;
        };
        let Some((name_part, var)) = rest.rsplit_once('.') else {
            continue;
        };

        let entry = map
            .entry(name_part.to_string())
            .or_insert_with(|| ConfInfo {
                name: name_part.to_string(),
                ..duplicate_conf(&default_conf)
            });

        if var_ci_eq(var, "key") {
            if let Some(v) = value {
                entry.key = Some(v.clone());
            }
        } else if var_ci_eq(var, "command") {
            if let Some(v) = value {
                entry.command = Some(v.clone());
            }
        } else if var_ci_eq(var, "cmd") {
            if let Some(v) = value {
                entry.cmd = Some(v.clone());
            }
        } else if var_ci_eq(var, "where") {
            if let Some(v) = value.as_deref().and_then(set_where) {
                entry.where_ = v;
            }
        } else if var_ci_eq(var, "ifexists") {
            if let Some(v) = value.as_deref().and_then(set_if_exists) {
                entry.if_exists = v;
            }
        } else if var_ci_eq(var, "ifmissing") {
            if let Some(v) = value.as_deref().and_then(set_if_missing) {
                entry.if_missing = v;
            }
        }
    }

    (default_conf, map.into_values().collect(), separators)
}

fn trailer_block_get(
    input: &str,
    opts: &ProcessTrailerOptions,
    conf: &[ConfInfo],
    separators: &str,
    comment_prefix: &str,
) -> TrailerBlock {
    let end_of_log = find_end_of_log_message(input, opts.no_divider, comment_prefix);
    let trailer_start =
        find_trailer_block_start(input, end_of_log, conf, separators, comment_prefix);
    let slice = &input[trailer_start..end_of_log];
    let mut lines: Vec<String> = Vec::new();
    let mut pos = 0usize;
    let mut last_trailer_idx: Option<usize> = None;

    while pos < slice.len() {
        let le = line_end(slice, pos, slice.len());
        let line = slice[pos..le].to_string();
        pos = if le < slice.len() {
            le + 1
        } else {
            slice.len()
        };

        if let Some(idx) = last_trailer_idx {
            if !line.is_empty() && line.chars().next().is_some_and(|c| c == ' ' || c == '\t') {
                lines[idx].push('\n');
                lines[idx].push_str(&line);
                continue;
            }
        }

        let is_trailer_line = find_separator(&line, separators)
            .is_some_and(|p| p >= 1 && !line.starts_with(|c: char| c.is_whitespace()));
        last_trailer_idx = if is_trailer_line {
            lines.push(line);
            Some(lines.len() - 1)
        } else {
            lines.push(line);
            None
        };
    }

    TrailerBlock {
        blank_line_before: ends_with_blank_line(input, trailer_start),
        start: trailer_start,
        end: end_of_log,
        lines,
    }
}

fn parse_trailers_from_input(
    block: &TrailerBlock,
    opts: &ProcessTrailerOptions,
    conf: &[ConfInfo],
    separators: &str,
    comment_prefix: &str,
) -> Vec<TrailerItem> {
    let mut out = Vec::new();
    for line in &block.lines {
        if starts_with_comment_line(line, comment_prefix) {
            continue;
        }
        if let Some(sep) = find_separator(line, separators) {
            if sep >= 1 {
                let (tok, mut val, _) = parse_trailer_into(line, separators, conf, true);
                if opts.unfold {
                    val = unfold_value(&val);
                }
                out.push(TrailerItem {
                    token: Some(tok),
                    value: val,
                });
            }
        } else if !opts.only_trailers {
            out.push(TrailerItem {
                token: None,
                value: line.clone(),
            });
        }
    }
    out
}

fn apply_command(conf: &ConfInfo, arg: Option<&str>, cwd: Option<&Path>) -> String {
    let arg = arg.unwrap_or("");
    let dir = cwd.unwrap_or_else(|| Path::new("."));
    let output = if let Some(cmd) = &conf.cmd {
        // Match Git `prepare_shell_cmd`: `sh -c '$cmd \"$@\"' $cmd <trailer-arg>` (always).
        let script = format!("{cmd} \"$@\"");
        Command::new("sh")
            .arg("-c")
            .arg(&script)
            .arg(cmd)
            .arg(arg)
            .stdin(Stdio::null())
            .current_dir(dir)
            .output()
    } else if let Some(command) = &conf.command {
        let cmd_line = command.replace(TRAILER_ARG_PLACEHOLDER, arg);
        Command::new("sh")
            .arg("-c")
            .arg(&cmd_line)
            .stdin(Stdio::null())
            .current_dir(dir)
            .output()
    } else {
        return String::new();
    };

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Git `check_if_different`: walk the trailer list in placement direction; return false if any
/// visited line is the same trailer (token + value) as `arg`.
fn check_if_different(
    head: &[TrailerItem],
    start_idx: usize,
    arg: &ArgItem,
    check_all: bool,
) -> bool {
    let where_ = arg.conf.where_;
    let mut idx = start_idx;
    loop {
        if same_trailer(&head[idx], arg) {
            return false;
        }
        let next = if after_or_end(where_) {
            idx.checked_sub(1)
        } else if idx + 1 < head.len() {
            Some(idx + 1)
        } else {
            None
        };
        let Some(ni) = next else {
            return true;
        };
        idx = ni;
        if !check_all {
            return true;
        }
    }
}

fn insert_relative_to(
    head: &mut Vec<TrailerItem>,
    anchor_idx: usize,
    item: TrailerItem,
    where_: TrailerWhere,
) {
    if head.is_empty() {
        head.push(item);
        return;
    }
    let anchor_idx = anchor_idx.min(head.len().saturating_sub(1));
    let at = if after_or_end(where_) {
        (anchor_idx + 1).min(head.len())
    } else {
        anchor_idx.min(head.len())
    };
    head.insert(at, item);
}

fn apply_item_command(
    head: &[TrailerItem],
    in_idx: Option<usize>,
    arg: &mut ArgItem,
    cwd: Option<&Path>,
) {
    if arg.conf.command.is_none() && arg.conf.cmd.is_none() {
        return;
    }
    let in_val = in_idx.and_then(|i| head.get(i)).and_then(|t| {
        if t.token.is_some() {
            Some(t.value.as_str())
        } else {
            None
        }
    });
    let arg_for_cmd = if !arg.value.is_empty() {
        Some(arg.value.as_str())
    } else {
        in_val
    };
    arg.value = apply_command(&arg.conf, arg_for_cmd, cwd);
}

fn apply_arg_if_exists(
    head: &mut Vec<TrailerItem>,
    in_idx: usize,
    mut arg: ArgItem,
    on_idx: usize,
    cwd: Option<&Path>,
) {
    let if_exists = arg.conf.if_exists;
    match if_exists {
        TrailerIfExists::DoNothing => {}
        TrailerIfExists::Replace => {
            apply_item_command(head, Some(in_idx), &mut arg, cwd);
            let where_for_insert = arg.conf.where_;
            let new_item = TrailerItem {
                token: Some(arg.token),
                value: arg.value,
            };
            // Git adds the new trailer next to `on_tok`, then deletes `in_tok`.
            let on_idx = on_idx.min(head.len().saturating_sub(1));
            let insert_pos = if after_or_end(where_for_insert) {
                (on_idx + 1).min(head.len())
            } else {
                on_idx.min(head.len())
            };
            head.insert(insert_pos, new_item);
            let del = if insert_pos <= in_idx {
                in_idx + 1
            } else {
                in_idx
            };
            head.remove(del);
        }
        TrailerIfExists::Add => {
            apply_item_command(head, Some(in_idx), &mut arg, cwd);
            let new_item = TrailerItem {
                token: Some(arg.token),
                value: arg.value,
            };
            insert_relative_to(head, on_idx, new_item, arg.conf.where_);
        }
        TrailerIfExists::AddIfDifferent => {
            apply_item_command(head, Some(in_idx), &mut arg, cwd);
            if check_if_different(head, in_idx, &arg, true) {
                let new_item = TrailerItem {
                    token: Some(arg.token.clone()),
                    value: arg.value.clone(),
                };
                insert_relative_to(head, on_idx, new_item, arg.conf.where_);
            }
        }
        TrailerIfExists::AddIfDifferentNeighbor => {
            apply_item_command(head, Some(in_idx), &mut arg, cwd);
            if check_if_different(head, on_idx, &arg, false) {
                let new_item = TrailerItem {
                    token: Some(arg.token.clone()),
                    value: arg.value.clone(),
                };
                insert_relative_to(head, on_idx, new_item, arg.conf.where_);
            }
        }
        TrailerIfExists::Default => {}
    }
}

fn apply_arg_if_missing(head: &mut Vec<TrailerItem>, mut arg: ArgItem, cwd: Option<&Path>) {
    match arg.conf.if_missing {
        TrailerIfMissing::DoNothing => {}
        TrailerIfMissing::Add | TrailerIfMissing::Default => {
            apply_item_command(head, None, &mut arg, cwd);
            let where_ = arg.conf.where_;
            let item = TrailerItem {
                token: Some(arg.token),
                value: arg.value,
            };
            if after_or_end(where_) {
                head.push(item);
            } else {
                head.insert(0, item);
            }
        }
    }
}

/// Returns `None` if `arg` was applied to an existing trailer, `Some(arg)` if no match.
fn find_same_and_apply_arg(
    head: &mut Vec<TrailerItem>,
    arg: ArgItem,
    cwd: Option<&Path>,
) -> Option<ArgItem> {
    let where_ = arg.conf.where_;
    let middle = matches!(where_, TrailerWhere::After | TrailerWhere::Before);
    let backwards = after_or_end(where_);

    if head.is_empty() {
        return Some(arg);
    }

    let start_idx = if backwards { head.len() - 1 } else { 0 };

    let mut i: isize = if backwards {
        head.len() as isize - 1
    } else {
        0
    };

    loop {
        if i < 0 || (i as usize) >= head.len() {
            break;
        }
        let idx = i as usize;
        if same_token(head[idx].token.as_deref(), &arg.token) {
            let on_idx = if middle { idx } else { start_idx };
            apply_arg_if_exists(head, idx, arg, on_idx, cwd);
            return None;
        }
        i = if backwards { i - 1 } else { i + 1 };
    }
    Some(arg)
}

fn merge_arg_conf(base: &ConfInfo, new_arg: &NewTrailerArg) -> ConfInfo {
    let mut c = duplicate_conf(base);
    if new_arg.where_ != TrailerWhere::Default {
        c.where_ = new_arg.where_;
    }
    if new_arg.if_exists != TrailerIfExists::Default {
        c.if_exists = new_arg.if_exists;
    }
    if new_arg.if_missing != TrailerIfMissing::Default {
        c.if_missing = new_arg.if_missing;
    }
    c
}

fn parse_trailers_from_config(conf_list: &[ConfInfo]) -> Vec<ArgItem> {
    let mut v = Vec::new();
    for item in conf_list {
        if item.command.is_some() {
            v.push(ArgItem {
                token: token_from_item(item, None),
                value: String::new(),
                conf: duplicate_conf(item),
            });
        }
    }
    v
}

fn parse_command_line_trailers(
    new_args: &[NewTrailerArg],
    default_conf: &ConfInfo,
    conf_list: &[ConfInfo],
    separators: &str,
) -> Vec<ArgItem> {
    let cl_separators: String = format!("={separators}");
    let mut out = Vec::new();
    for nt in new_args {
        let sep = find_separator(&nt.text, &cl_separators);
        if sep == Some(0) {
            continue;
        }
        let (tok, val, picked) = parse_trailer_into(&nt.text, &cl_separators, conf_list, true);
        let base = if !picked.name.is_empty() {
            picked
        } else {
            duplicate_conf(default_conf)
        };
        let conf = merge_arg_conf(&base, nt);
        out.push(ArgItem {
            token: tok,
            value: val,
            conf,
        });
    }
    out
}

fn process_trailers_lists(head: &mut Vec<TrailerItem>, args: Vec<ArgItem>, cwd: Option<&Path>) {
    for arg_tok in args {
        if let Some(a) = find_same_and_apply_arg(head, arg_tok, cwd) {
            apply_arg_if_missing(head, a, cwd);
        }
    }
}

fn format_one_trailer(
    item: &TrailerItem,
    trim_empty: bool,
    only_trailers: bool,
    separators: &str,
    out: &mut String,
) {
    let Some(ref tok) = item.token else {
        if only_trailers {
            return;
        }
        out.push_str(&item.value);
        out.push('\n');
        return;
    };
    if trim_empty && item.value.is_empty() {
        return;
    }
    out.push_str(tok);
    let c = last_non_space_char(tok);
    let need_sep = c.is_none_or(|ch| !separators.contains(ch));
    if need_sep {
        out.push(separators.chars().next().unwrap_or(':'));
        out.push(' ');
    }
    out.push_str(&item.value);
    out.push('\n');
}

fn format_trailers_list(
    items: &[TrailerItem],
    opts: &ProcessTrailerOptions,
    separators: &str,
    out: &mut String,
) {
    for item in items {
        format_one_trailer(item, opts.trim_empty, opts.only_trailers, separators, out);
    }
}

/// Process a commit message: parse trailer block, apply config and `--trailer` args, emit result.
///
/// `git_dir` selects which repository config to load (with standard cascade). When `None`, only
/// non-repo config layers are used (matches `git interpret-trailers` outside a repo).
pub fn process_trailers(
    input: &str,
    opts: &ProcessTrailerOptions,
    new_trailer_args: &[NewTrailerArg],
    git_dir: Option<&Path>,
) -> String {
    let cfg = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        git_dir,
        true,
    )
    .unwrap_or_default();
    let comment_prefix = comment_line_prefix(&cfg);
    let (default_conf, conf_list, separators) = load_trailer_config(&cfg);

    let block = trailer_block_get(input, opts, &conf_list, &separators, &comment_prefix);
    let mut head =
        parse_trailers_from_input(&block, opts, &conf_list, &separators, &comment_prefix);

    if !opts.only_input {
        let mut arg_queue = parse_trailers_from_config(&conf_list);
        arg_queue.extend(parse_command_line_trailers(
            new_trailer_args,
            &default_conf,
            &conf_list,
            &separators,
        ));
        let cwd = crate::environment::Environment::empty().cwd;
        process_trailers_lists(&mut head, arg_queue, Some(cwd.as_path()));
    }

    let mut out = String::new();
    if !opts.only_trailers {
        out.push_str(&input[..block.start]);
        if !block.blank_line_before {
            out.push('\n');
        }
    }
    format_trailers_list(&head, opts, &separators, &mut out);
    if !opts.only_trailers {
        out.push_str(&input[block.end..]);
    }
    out
}

/// Complete stdin/file input with a trailing newline when missing (Git `strbuf_complete_line`).
pub fn complete_line(s: &str) -> String {
    if s.is_empty() || !s.ends_with('\n') {
        let mut o = s.to_string();
        o.push('\n');
        o
    } else {
        s.to_string()
    }
}
