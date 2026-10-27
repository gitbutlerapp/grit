//! Commit message trailer parsing and rewriting (Git-compatible).
//!
//! Behaviour matches upstream `git/trailer.c` / `git interpret-trailers`.

use std::collections::HashMap;
use std::path::Path;

use crate::config::{ConfigEntry, ConfigSet};

const CUT_LINE: &str = "------------------------ >8 ------------------------";

const GIT_GENERATED_PREFIXES: &[&str] = &["Signed-off-by: ", "(cherry picked from commit "];

const TRAILER_ARG_PLACEHOLDER: &str = "$ARG";

/// Placement of a new trailer relative to an anchor trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailerWhere {
    #[default]
    /// `Default` variant.
    Default,
    /// `End` variant.
    End,
    /// `After` variant.
    After,
    /// `Before` variant.
    Before,
    /// `Start` variant.
    Start,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// `TrailerIfExists` enum.
pub enum TrailerIfExists {
    #[default]
    /// `Default` variant.
    Default,
    /// `AddIfDifferentNeighbor` variant.
    AddIfDifferentNeighbor,
    /// `AddIfDifferent` variant.
    AddIfDifferent,
    /// `Add` variant.
    Add,
    /// `Replace` variant.
    Replace,
    /// `DoNothing` variant.
    DoNothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// `TrailerIfMissing` enum.
pub enum TrailerIfMissing {
    #[default]
    /// `Default` variant.
    Default,
    /// `Add` variant.
    Add,
    /// `DoNothing` variant.
    DoNothing,
}

#[derive(Debug, Clone, Default)]
/// `ProcessTrailerOptions` struct.
pub struct ProcessTrailerOptions {
    /// `trim_empty` field.
    pub trim_empty: bool,
    /// `only_trailers` field.
    pub only_trailers: bool,
    /// `only_input` field.
    pub only_input: bool,
    /// `unfold` field.
    pub unfold: bool,
    /// `no_divider` field.
    pub no_divider: bool,
}

#[derive(Debug, Clone)]
/// `NewTrailerArg` struct.
pub struct NewTrailerArg {
    /// `text` field.
    pub text: String,
    /// `where_` field.
    pub where_: TrailerWhere,
    /// `if_exists` field.
    pub if_exists: TrailerIfExists,
    /// `if_missing` field.
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

impl ArgItem {
    fn into_item(self) -> TrailerItem {
        TrailerItem {
            token: Some(self.token),
            value: self.value,
        }
    }
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
    match s.to_ascii_lowercase().as_str() {
        "after" => Some(TrailerWhere::After),
        "before" => Some(TrailerWhere::Before),
        "end" => Some(TrailerWhere::End),
        "start" => Some(TrailerWhere::Start),
        _ => None,
    }
}

/// Parse `trailer.ifexists` / `--if-exists` values.
pub fn trailer_if_exists_from_str(s: &str) -> Option<TrailerIfExists> {
    match s.to_ascii_lowercase().as_str() {
        "addifdifferent" => Some(TrailerIfExists::AddIfDifferent),
        "addifdifferentneighbor" => Some(TrailerIfExists::AddIfDifferentNeighbor),
        "add" => Some(TrailerIfExists::Add),
        "replace" => Some(TrailerIfExists::Replace),
        "donothing" => Some(TrailerIfExists::DoNothing),
        _ => None,
    }
}

/// Parse `trailer.ifmissing` / `--if-missing` values.
pub fn trailer_if_missing_from_str(s: &str) -> Option<TrailerIfMissing> {
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

fn same_trailer(a: &TrailerItem, b: &ArgItem) -> bool {
    same_token(a.token.as_deref(), &b.token) && a.value.eq_ignore_ascii_case(&b.value)
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

/// Start of the line that ends at `len` (`None` when `len` is 0).
fn last_line_start(buf: &str, len: usize) -> Option<usize> {
    let before_last_byte = buf.as_bytes().get(..len.checked_sub(1)?)?;
    Some(
        before_last_byte
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1),
    )
}

/// Offset of the scissors line (`# --- >8 ---`) within `s[..len]`, or `len` when there is none.
fn cut_line_start(s: &str, len: usize, comment_prefix: &str) -> usize {
    let head = format!("{comment_prefix} {CUT_LINE}\n");
    if s.starts_with(&head) {
        return 0;
    }
    let pattern = format!("\n{head}");
    if let Some(p) = s[..len].find(&pattern) {
        let newlen = p + 1;
        if newlen < len {
            return newlen;
        }
    }
    len
}

fn ignored_log_message_bytes(buf: &str, len: usize, comment_prefix: &str) -> usize {
    let cutoff = cut_line_start(buf, len, comment_prefix);
    let mut bol = 0usize;
    let mut boc = 0usize;
    let mut in_old_conflicts = false;

    while bol < cutoff {
        let le = line_end(buf, bol, cutoff);
        let line = &buf[bol..le];

        let is_comment = line.starts_with(comment_prefix) || line.is_empty();
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

        bol = after_line(buf, bol, cutoff);
    }

    if boc != 0 {
        len - boc
    } else {
        len - cutoff
    }
}

fn find_end_of_log_message(input: &str, no_divider: bool, comment_prefix: &str) -> usize {
    let is_divider = |line: &str| {
        line.strip_prefix("---")
            .is_some_and(|rest| rest.chars().next().is_none_or(|c| c.is_ascii_whitespace()))
    };
    let mut end = input.len();
    let mut pos = 0usize;
    while !no_divider && pos < input.len() {
        if is_divider(&input[pos..]) {
            end = pos;
            break;
        }
        pos = after_line(input, pos, input.len());
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

impl ConfInfo {
    /// Whether `token` (text before the separator) starts with this trailer's alias name or with
    /// its configured key, compared case-insensitively and ignoring the key's trailing separator.
    fn matches_token(&self, token: &str) -> bool {
        let token_len = token_len_without_separator(token);
        let has_prefix =
            |want: &str| token_len >= want.len() && token[..want.len()].eq_ignore_ascii_case(want);
        has_prefix(&self.name)
            || self
                .key
                .as_deref()
                .is_some_and(|key| has_prefix(&key[..token_len_without_separator(key)]))
    }

    /// Applies a `where`, `ifexists` or `ifmissing` setting; unparsable values are ignored.
    /// Returns `false` when `var` is not one of those settings.
    fn apply_placement_setting(&mut self, var: &str, value: Option<&str>) -> bool {
        if var.eq_ignore_ascii_case("where") {
            if let Some(v) = value.and_then(trailer_where_from_str) {
                self.where_ = v;
            }
        } else if var.eq_ignore_ascii_case("ifexists") {
            if let Some(v) = value.and_then(trailer_if_exists_from_str) {
                self.if_exists = v;
            }
        } else if var.eq_ignore_ascii_case("ifmissing") {
            if let Some(v) = value.and_then(trailer_if_missing_from_str) {
                self.if_missing = v;
            }
        } else {
            return false;
        }
        true
    }
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
        let line = &buf[pos..line_end(buf, pos, len)];
        if !line.starts_with(comment_prefix) && is_blank_line(line) {
            end_of_title = pos;
            break;
        }
        pos = after_line(buf, pos, len);
    }

    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines = 0i32;
    let mut non_trailer_lines = 0i32;
    let mut possible_continuation = 0i32;

    let mut next = last_line_start(buf, len);
    while let Some(bol) = next {
        if bol < end_of_title {
            break;
        }
        next = last_line_start(buf, bol);
        let line = &buf[bol..line_end(buf, bol, len)];

        if line.starts_with(comment_prefix) {
            non_trailer_lines += possible_continuation;
            possible_continuation = 0;
            continue;
        }

        if is_blank_line(line) {
            if only_spaces {
                continue;
            }
            non_trailer_lines += possible_continuation;
            let is_trailer_block = (recognized_prefix && trailer_lines * 3 >= non_trailer_lines)
                || (trailer_lines > 0 && non_trailer_lines == 0);
            return if is_trailer_block {
                after_line(buf, bol, len)
            } else {
                len
            };
        }
        only_spaces = false;

        let indented = line.starts_with(char::is_whitespace);
        if GIT_GENERATED_PREFIXES.iter().any(|p| line.starts_with(p)) {
            trailer_lines += 1;
            possible_continuation = 0;
            recognized_prefix = true;
        } else if let Some(sep_pos) =
            find_separator(line, separators).filter(|&p| p >= 1 && !indented)
        {
            trailer_lines += 1;
            possible_continuation = 0;
            recognized_prefix = recognized_prefix
                || conf
                    .iter()
                    .any(|item| item.matches_token(line[..sep_pos].trim_end()));
        } else if indented {
            possible_continuation += 1;
        } else {
            non_trailer_lines += 1 + possible_continuation;
            possible_continuation = 0;
        }
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

fn token_from_item(item: &ConfInfo, fallback: &str) -> String {
    item.key.clone().unwrap_or_else(|| fallback.to_string())
}

/// Splits `trailer` into token and value. When the token matches a configured alias, the token
/// is replaced by the alias key and the matching configuration is returned alongside.
fn parse_trailer<'c>(
    trailer: &str,
    separators: &str,
    conf: &'c [ConfInfo],
) -> (String, String, Option<&'c ConfInfo>) {
    let (tok, val) = match find_separator(trailer, separators) {
        Some(pos) => (trailer[..pos].trim(), trailer[pos + 1..].trim()),
        None => (trailer.trim(), ""),
    };
    let picked = conf.iter().find(|item| item.matches_token(tok));
    let tok = picked.map_or_else(|| tok.to_string(), |item| token_from_item(item, tok));
    (tok, val.to_string(), picked)
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
    let mut default_conf = ConfInfo::default();
    let mut map: HashMap<String, ConfInfo> = HashMap::new();
    let mut separators = ":".to_string();

    for ConfigEntry { key, value, .. } in cfg.entries() {
        let Some(var) = key
            .strip_prefix("trailer.")
            .filter(|rest| !rest.contains('.'))
        else {
            continue;
        };
        if !default_conf.apply_placement_setting(var, value.as_deref())
            && var.eq_ignore_ascii_case("separators")
        {
            if let Some(v) = value {
                separators = v.clone();
            }
        }
    }

    for ConfigEntry { key, value, .. } in cfg.entries() {
        let Some((name_part, var)) = key
            .strip_prefix("trailer.")
            .and_then(|rest| rest.rsplit_once('.'))
        else {
            continue;
        };

        let entry = map
            .entry(name_part.to_string())
            .or_insert_with(|| ConfInfo {
                name: name_part.to_string(),
                ..default_conf.clone()
            });
        if entry.apply_placement_setting(var, value.as_deref()) {
            continue;
        }
        let slot = if var.eq_ignore_ascii_case("key") {
            &mut entry.key
        } else if var.eq_ignore_ascii_case("command") {
            &mut entry.command
        } else if var.eq_ignore_ascii_case("cmd") {
            &mut entry.cmd
        } else {
            continue;
        };
        if let Some(v) = value {
            *slot = Some(v.clone());
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
    let mut lines: Vec<String> = Vec::new();
    let mut continues_trailer = false;
    for line in input[trailer_start..end_of_log].split_terminator('\n') {
        if continues_trailer && line.starts_with([' ', '\t']) {
            if let Some(last) = lines.last_mut() {
                last.push('\n');
                last.push_str(line);
            }
            continue;
        }
        continues_trailer = find_separator(line, separators)
            .is_some_and(|p| p >= 1 && !line.starts_with(char::is_whitespace));
        lines.push(line.to_string());
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
    block
        .lines
        .iter()
        .filter(|line| !line.starts_with(comment_prefix))
        .filter_map(|line| match find_separator(line, separators) {
            Some(0) => None,
            Some(_) => {
                let (tok, val, _) = parse_trailer(line, separators, conf);
                Some(TrailerItem {
                    token: Some(tok),
                    value: if opts.unfold { unfold_value(&val) } else { val },
                })
            }
            None => (!opts.only_trailers).then(|| TrailerItem {
                token: None,
                value: line.clone(),
            }),
        })
        .collect()
}

fn apply_command(conf: &ConfInfo, arg: Option<&str>, cwd: Option<&Path>) -> String {
    let arg = arg.unwrap_or("");
    let dir = cwd.unwrap_or_else(|| Path::new("."));
    use crate::command_runner::{
        system_command_runner, CommandEnvironment, CommandSpec, CommandStdin, CommandStdio,
        ShellInvocation,
    };
    let runner = system_command_runner();
    let output = if let Some(cmd) = &conf.cmd {
        let script = format!("{cmd} \"$@\"");
        let spec = CommandSpec {
            program: "sh".into(),
            args: Vec::new(),
            shell: Some(ShellInvocation::DashC {
                script,
                argv0: Some(cmd.clone()),
                args: vec![std::ffi::OsString::from(arg)],
            }),
            cwd: Some(dir.to_path_buf()),
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Null,
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Inherit,
        };
        runner
            .spawn(&spec)
            .ok()
            .and_then(|h| h.wait_with_output().ok())
    } else if let Some(command) = &conf.command {
        let cmd_line = command.replace(TRAILER_ARG_PLACEHOLDER, arg);
        crate::command_runner::run_sh_dash_c(
            runner.as_ref(),
            &cmd_line,
            Some(dir),
            CommandStdin::Null,
            CommandStdio::Inherit,
        )
        .ok()
    } else {
        return String::new();
    };

    match output {
        Some(o) if o.status.success => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Whether `arg` differs from every trailer visited from `start_idx`: only that trailer when
/// `check_all` is false, otherwise every trailer from it towards the placement end.
fn check_if_different(
    head: &[TrailerItem],
    start_idx: usize,
    arg: &ArgItem,
    check_all: bool,
) -> bool {
    let visited = if !check_all {
        &head[start_idx..=start_idx]
    } else if after_or_end(arg.conf.where_) {
        &head[..=start_idx]
    } else {
        &head[start_idx..]
    };
    !visited.iter().any(|item| same_trailer(item, arg))
}

/// Inserts `item` after (`after` / `end`) or before `head[anchor_idx]` and returns its index.
fn insert_relative_to(
    head: &mut Vec<TrailerItem>,
    anchor_idx: usize,
    item: TrailerItem,
    where_: TrailerWhere,
) -> usize {
    let at = match head.len().checked_sub(1) {
        None => 0,
        Some(last) if after_or_end(where_) => anchor_idx.min(last) + 1,
        Some(last) => anchor_idx.min(last),
    };
    head.insert(at, item);
    at
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
    let in_val = in_idx
        .and_then(|i| head.get(i))
        .filter(|t| t.token.is_some())
        .map(|t| t.value.as_str());
    let arg_for_cmd = if arg.value.is_empty() {
        in_val
    } else {
        Some(arg.value.as_str())
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
    if matches!(
        if_exists,
        TrailerIfExists::DoNothing | TrailerIfExists::Default
    ) {
        return;
    }
    apply_item_command(head, Some(in_idx), &mut arg, cwd);
    let add = match if_exists {
        TrailerIfExists::AddIfDifferent => check_if_different(head, in_idx, &arg, true),
        TrailerIfExists::AddIfDifferentNeighbor => check_if_different(head, on_idx, &arg, false),
        _ => true,
    };
    if !add {
        return;
    }
    let where_ = arg.conf.where_;
    let at = insert_relative_to(head, on_idx, arg.into_item(), where_);
    if if_exists == TrailerIfExists::Replace {
        // The replacement is placed relative to the anchor before the old trailer is removed.
        head.remove(if at <= in_idx { in_idx + 1 } else { in_idx });
    }
}

fn apply_arg_if_missing(head: &mut Vec<TrailerItem>, mut arg: ArgItem, cwd: Option<&Path>) {
    if arg.conf.if_missing == TrailerIfMissing::DoNothing {
        return;
    }
    apply_item_command(head, None, &mut arg, cwd);
    let at = if after_or_end(arg.conf.where_) {
        head.len()
    } else {
        0
    };
    head.insert(at, arg.into_item());
}

/// Returns `None` if `arg` was applied to an existing trailer, `Some(arg)` if no match.
fn find_same_and_apply_arg(
    head: &mut Vec<TrailerItem>,
    arg: ArgItem,
    cwd: Option<&Path>,
) -> Option<ArgItem> {
    let where_ = arg.conf.where_;
    let backwards = after_or_end(where_);
    let same = |item: &TrailerItem| same_token(item.token.as_deref(), &arg.token);
    let found = if backwards {
        head.iter().rposition(same)
    } else {
        head.iter().position(same)
    };
    let Some(idx) = found else {
        return Some(arg);
    };
    let on_idx = match where_ {
        TrailerWhere::After | TrailerWhere::Before => idx,
        _ if backwards => head.len() - 1,
        _ => 0,
    };
    apply_arg_if_exists(head, idx, arg, on_idx, cwd);
    None
}

fn merge_arg_conf(mut conf: ConfInfo, new_arg: &NewTrailerArg) -> ConfInfo {
    if new_arg.where_ != TrailerWhere::Default {
        conf.where_ = new_arg.where_;
    }
    if new_arg.if_exists != TrailerIfExists::Default {
        conf.if_exists = new_arg.if_exists;
    }
    if new_arg.if_missing != TrailerIfMissing::Default {
        conf.if_missing = new_arg.if_missing;
    }
    conf
}

fn parse_trailers_from_config(conf_list: &[ConfInfo]) -> Vec<ArgItem> {
    conf_list
        .iter()
        .filter(|item| item.command.is_some())
        .map(|item| ArgItem {
            token: token_from_item(item, &item.name),
            value: String::new(),
            conf: item.clone(),
        })
        .collect()
}

fn parse_command_line_trailers(
    new_args: &[NewTrailerArg],
    default_conf: &ConfInfo,
    conf_list: &[ConfInfo],
    separators: &str,
) -> Vec<ArgItem> {
    let cl_separators = format!("={separators}");
    new_args
        .iter()
        .filter(|nt| find_separator(&nt.text, &cl_separators) != Some(0))
        .map(|nt| {
            let (token, value, picked) = parse_trailer(&nt.text, &cl_separators, conf_list);
            let base = picked
                .filter(|item| !item.name.is_empty())
                .unwrap_or(default_conf);
            ArgItem {
                token,
                value,
                conf: merge_arg_conf(base.clone(), nt),
            }
        })
        .collect()
}

fn process_trailers_lists(head: &mut Vec<TrailerItem>, args: Vec<ArgItem>, cwd: Option<&Path>) {
    for arg_tok in args {
        if let Some(a) = find_same_and_apply_arg(head, arg_tok, cwd) {
            apply_arg_if_missing(head, a, cwd);
        }
    }
}

fn format_trailers_list(
    items: &[TrailerItem],
    opts: &ProcessTrailerOptions,
    separators: &str,
    out: &mut String,
) {
    for item in items {
        match &item.token {
            None if opts.only_trailers => continue,
            None => {}
            Some(_) if opts.trim_empty && item.value.is_empty() => continue,
            Some(tok) => {
                out.push_str(tok);
                if last_non_space_char(tok).is_none_or(|ch| !separators.contains(ch)) {
                    out.push(separators.chars().next().unwrap_or(':'));
                    out.push(' ');
                }
            }
        }
        out.push_str(&item.value);
        out.push('\n');
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
    let cfg = ConfigSet::load(&crate::environment::Environment::empty(), git_dir, true)
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
        let cwd = Some(crate::environment::Environment::empty().cwd.clone());
        process_trailers_lists(&mut head, arg_queue, cwd.as_deref());
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
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

#[cfg(test)]
mod line_coverage_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn last_line_start_finds_the_line_ending_at_len() {
        let buf = "ab\ncd\n\né\n";
        assert_eq!(last_line_start(buf, 0), None);
        assert_eq!(last_line_start(buf, 1), Some(0));
        assert_eq!(last_line_start(buf, 3), Some(0));
        assert_eq!(last_line_start(buf, 6), Some(3));
        assert_eq!(last_line_start(buf, 7), Some(6));
        assert_eq!(last_line_start(buf, buf.len()), Some(7));
        assert_eq!(last_line_start("é", "é".len()), Some(0));
    }

    #[test]
    fn trailer_enum_parsers_and_process_trailers_matrix() {
        assert_eq!(trailer_where_from_str("after"), Some(TrailerWhere::After));
        assert_eq!(
            trailer_if_exists_from_str("addIfDifferent"),
            Some(TrailerIfExists::AddIfDifferent)
        );
        assert_eq!(
            trailer_if_missing_from_str("doNothing"),
            Some(TrailerIfMissing::DoNothing)
        );
        assert_eq!(complete_line("x"), "x\n");
        assert_eq!(complete_line("x\n"), "x\n");

        let dir = tempdir().expect("tempdir");
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q", "-b", "main", "."])
            .status()
            .expect("init");
        let git_dir = dir.path().join(".git");
        let input = "Subject\n\nSigned-off-by: A <a@example.com>\n";
        for only_input in [true, false] {
            for only_trailers in [true, false] {
                let opts = ProcessTrailerOptions {
                    trim_empty: true,
                    only_trailers,
                    only_input,
                    unfold: true,
                    no_divider: false,
                };
                let _ = process_trailers(input, &opts, &[], Some(git_dir.as_path()));
                let arg = NewTrailerArg {
                    text: "Reviewed-by: B".into(),
                    where_: TrailerWhere::End,
                    if_exists: TrailerIfExists::Add,
                    if_missing: TrailerIfMissing::Add,
                };
                let _ = process_trailers(input, &opts, &[arg], Some(git_dir.as_path()));
            }
        }
    }
}
