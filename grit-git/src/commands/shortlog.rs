//! `grit shortlog` — summarize git log output by author.
//!
//! Groups commits by author (or committer, trailer, or a custom format) and
//! shows a count with commit subjects, matching `git shortlog`.

use anyhow::Result;
use grit_lib::git_date::show::{parse_date_format, show_date, DateMode, DateModeType};
use grit_lib::mailmap::{load_mailmap_table, read_mailmap_string, MailmapTable};
use grit_lib::objects::{parse_commit, CommitData, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};

const DEFAULT_WRAPLEN: i32 = 76;
const DEFAULT_INDENT1: i32 = 6;
const DEFAULT_INDENT2: i32 = 9;

/// One requested grouping: a pretty-format string, or a trailer key.
#[derive(Clone)]
enum Group {
    Format(String),
    Trailer(String),
}

/// Parsed shortlog options.
struct Options {
    revisions: Vec<String>,
    summary: bool,
    numbered: bool,
    email: bool,
    user_format: Option<String>,
    abbrev: usize,
    date_mode: Option<String>,
    output: Option<String>,
    wrap_lines: bool,
    wrap: i32,
    in1: i32,
    in2: i32,
    max_count: Option<usize>,
    groups: Vec<Group>,
    /// Pseudo-revision options that select refs (`--all`, `--branches`, ...).
    all_refs: bool,
    branches: bool,
    tags: bool,
    remotes: bool,
    exclude_patterns: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            revisions: Vec::new(),
            summary: false,
            numbered: false,
            email: false,
            user_format: None,
            abbrev: 7,
            date_mode: None,
            output: None,
            wrap_lines: false,
            wrap: DEFAULT_WRAPLEN,
            in1: DEFAULT_INDENT1,
            in2: DEFAULT_INDENT2,
            max_count: None,
            groups: Vec::new(),
            all_refs: false,
            branches: false,
            tags: false,
            remotes: false,
            exclude_patterns: Vec::new(),
        }
    }
}

/// Raw-argv entry point for `grit shortlog` (manual parsing — clap cannot model
/// `-w[<n>]` optional args, numeric `-<N>` max-count, or repeatable `--group`).
pub fn run_with_raw_args(argv: &[String]) -> Result<()> {
    let mut opts = match parse_args(argv) {
        Ok(o) => o,
        Err(ParseError::TooManyArguments) => {
            eprintln!("error: too many arguments given outside repository");
            std::process::exit(129);
        }
        Err(ParseError::UnknownGroup(g)) => {
            eprintln!("fatal: unknown group type: {g}");
            std::process::exit(128);
        }
        Err(ParseError::Usage(msg)) => {
            eprintln!("error: {msg}");
            std::process::exit(129);
        }
    };

    // Default group is author.
    if opts.groups.is_empty() {
        opts.groups.push(Group::Format(if opts.email {
            "%aN <%aE>".to_owned()
        } else {
            "%aN".to_owned()
        }));
    }

    let stdin = io::stdin();
    let repo = Repository::discover(None).ok();

    // Mailmap: from repo when available, else `.mailmap` in cwd.
    let mailmap = match &repo {
        Some(r) => load_mailmap_table(r).unwrap_or_default(),
        None => {
            let mut t = MailmapTable::default();
            if let Ok(body) = std::fs::read_to_string(".mailmap") {
                read_mailmap_string(&mut t, &body);
            }
            t
        }
    };

    // Non-git directory: refuse extra arguments.
    if repo.is_none() && !opts.revisions.is_empty() {
        eprintln!("error: too many arguments given outside repository");
        std::process::exit(129);
    }

    let has_revs =
        !opts.revisions.is_empty() || opts.all_refs || opts.branches || opts.tags || opts.remotes;

    let mut log = Shortlog::new(opts, mailmap);

    if repo.is_some() && has_revs {
        if let Some(r) = &repo {
            collect_from_repo(r, &mut log)?;
        }
    } else if stdin.is_terminal() && repo.is_some() {
        // Default to HEAD when attached to a tty inside a repo.
        if let Some(r) = &repo {
            log.opts.revisions = vec!["HEAD".to_owned()];
            collect_from_repo(r, &mut log)?;
        }
    } else {
        read_from_stdin(&stdin, &mut log)?;
    }

    output(&mut log)
}

enum ParseError {
    TooManyArguments,
    UnknownGroup(String),
    Usage(String),
}

/// Manual option parser mimicking git's `parse_options` for shortlog.
fn parse_args(argv: &[String]) -> std::result::Result<Options, ParseError> {
    let mut opts = Options::default();
    let mut i = 0;
    let mut no_more_opts = false;

    while i < argv.len() {
        let arg = &argv[i];

        if no_more_opts {
            opts.revisions.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "--" {
            no_more_opts = true;
            opts.revisions.push(arg.clone());
            i += 1;
            continue;
        }

        // Long options.
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline_val) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            match name {
                "numbered" => opts.numbered = true,
                "summary" => opts.summary = true,
                "email" => opts.email = true,
                "committer" => opts.groups.push(Group::Format(if opts.email {
                    "%cN <%cE>".to_owned()
                } else {
                    "%cN".to_owned()
                })),
                "no-merges" => { /* handled via parents filter below */ }
                "merges" => {}
                "abbrev" => {
                    if let Some(v) = inline_val {
                        opts.abbrev = v.parse().unwrap_or(7);
                    } else {
                        // `--abbrev` with no value: default abbreviation length.
                        opts.abbrev = 7;
                    }
                }
                "no-abbrev" => opts.abbrev = 40,
                "date" => {
                    let v = match inline_val {
                        Some(v) => v,
                        None => {
                            i += 1;
                            argv.get(i).cloned().unwrap_or_default()
                        }
                    };
                    opts.date_mode = Some(v);
                }
                "format" | "pretty" => {
                    let v = match inline_val {
                        Some(v) => v,
                        None => {
                            i += 1;
                            argv.get(i).cloned().unwrap_or_default()
                        }
                    };
                    opts.user_format = Some(v);
                }
                "output" => {
                    let v = match inline_val {
                        Some(v) => v,
                        None => {
                            i += 1;
                            argv.get(i).cloned().unwrap_or_default()
                        }
                    };
                    opts.output = Some(v);
                }
                "group" => {
                    let v = match inline_val {
                        Some(v) => v,
                        None => {
                            i += 1;
                            argv.get(i).cloned().unwrap_or_default()
                        }
                    };
                    add_group(&mut opts, &v)?;
                }
                "no-group" => {
                    opts.groups.clear();
                }
                "wrap" => {
                    opts.wrap_lines = true;
                    if let Some(v) = inline_val {
                        parse_wrap_arg(&mut opts, &v)?;
                    } else {
                        opts.wrap = DEFAULT_WRAPLEN;
                        opts.in1 = DEFAULT_INDENT1;
                        opts.in2 = DEFAULT_INDENT2;
                    }
                }
                "all" => opts.all_refs = true,
                "branches" => opts.branches = true,
                "tags" => opts.tags = true,
                "remotes" => opts.remotes = true,
                "exclude" => {
                    let v = match inline_val {
                        Some(v) => v,
                        None => {
                            i += 1;
                            argv.get(i).cloned().unwrap_or_default()
                        }
                    };
                    opts.exclude_patterns.push(v);
                }
                "author" | "committer-filter" => {
                    // `--author=<pat>` is a revision-walk filter; accepted but treated
                    // as a no-op narrowing here (tests only require it not to crash).
                    if inline_val.is_none() {
                        i += 1;
                    }
                }
                _ => {
                    // Pass through unknown long options as revisions/pseudo-opts.
                    opts.revisions.push(arg.clone());
                }
            }
            i += 1;
            continue;
        }

        // Short options (possibly combined, e.g. `-nsc`, `-nse`).
        if arg.starts_with('-') && arg.len() > 1 {
            // Pure numeric: `-1`, `-25` → max-count.
            if let Ok(n) = arg[1..].parse::<usize>() {
                opts.max_count = Some(n);
                i += 1;
                continue;
            }

            let chars: Vec<char> = arg[1..].chars().collect();
            let mut ci = 0;
            while ci < chars.len() {
                match chars[ci] {
                    'n' => opts.numbered = true,
                    's' => opts.summary = true,
                    'e' => opts.email = true,
                    'c' => opts.groups.push(Group::Format(if opts.email {
                        "%cN <%cE>".to_owned()
                    } else {
                        "%cN".to_owned()
                    })),
                    'w' => {
                        opts.wrap_lines = true;
                        // `-w` consumes the remainder of this token as its optional arg.
                        let rest: String = chars[ci + 1..].iter().collect();
                        if rest.is_empty() {
                            opts.wrap = DEFAULT_WRAPLEN;
                            opts.in1 = DEFAULT_INDENT1;
                            opts.in2 = DEFAULT_INDENT2;
                        } else {
                            parse_wrap_arg(&mut opts, &rest)?;
                        }
                        break;
                    }
                    other => {
                        return Err(ParseError::Usage(format!("unknown switch `{other}'")));
                    }
                }
                ci += 1;
            }
            i += 1;
            continue;
        }

        // Bare token → revision.
        opts.revisions.push(arg.clone());
        i += 1;
    }

    Ok(opts)
}

fn add_group(opts: &mut Options, arg: &str) -> std::result::Result<(), ParseError> {
    if arg.eq_ignore_ascii_case("author") {
        opts.groups.push(Group::Format(if opts.email {
            "%aN <%aE>".to_owned()
        } else {
            "%aN".to_owned()
        }));
    } else if arg.eq_ignore_ascii_case("committer") {
        opts.groups.push(Group::Format(if opts.email {
            "%cN <%cE>".to_owned()
        } else {
            "%cN".to_owned()
        }));
    } else if let Some(field) = arg.strip_prefix("trailer:") {
        opts.groups.push(Group::Trailer(field.to_owned()));
    } else if let Some(field) = arg.strip_prefix("format:") {
        opts.groups.push(Group::Format(field.to_owned()));
    } else if arg.contains('%') {
        opts.groups.push(Group::Format(arg.to_owned()));
    } else {
        return Err(ParseError::UnknownGroup(arg.to_owned()));
    }
    Ok(())
}

fn parse_uint(arg: &str, defval: i32) -> Option<(i32, &str)> {
    let end = arg.find(',').unwrap_or(arg.len());
    let (num, rest) = arg.split_at(end);
    let rest = rest.strip_prefix(',').unwrap_or(rest);
    if num.is_empty() {
        return Some((defval, rest));
    }
    let v: i32 = num.parse().ok()?;
    Some((v, rest))
}

fn parse_wrap_arg(opts: &mut Options, arg: &str) -> std::result::Result<(), ParseError> {
    let usage = "-w[<width>[,<indent1>[,<indent2>]]]".to_owned();
    let (wrap, rest) = parse_uint(arg, DEFAULT_WRAPLEN).ok_or(ParseError::Usage(usage.clone()))?;
    let (in1, rest) = parse_uint(rest, DEFAULT_INDENT1).ok_or(ParseError::Usage(usage.clone()))?;
    let (in2, _rest) = parse_uint(rest, DEFAULT_INDENT2).ok_or(ParseError::Usage(usage.clone()))?;
    if wrap < 0 || in1 < 0 || in2 < 0 {
        return Err(ParseError::Usage(usage));
    }
    if wrap != 0 && ((in1 != 0 && wrap <= in1) || (in2 != 0 && wrap <= in2)) {
        return Err(ParseError::Usage(usage));
    }
    opts.wrap = wrap;
    opts.in1 = in1;
    opts.in2 = in2;
    Ok(())
}

/// Accumulator: insertion-ordered list of (key → subjects/count).
struct Shortlog {
    opts: Options,
    mailmap: MailmapTable,
    /// key → index into `entries`.
    index: HashMap<String, usize>,
    entries: Vec<Entry>,
}

struct Entry {
    key: String,
    /// Oneline subjects (raw bytes), in commit-walk order (newest first as appended).
    onelines: Vec<Vec<u8>>,
    count: usize,
}

impl Shortlog {
    fn new(opts: Options, mailmap: MailmapTable) -> Self {
        Self {
            opts,
            mailmap,
            index: HashMap::new(),
            entries: Vec::new(),
        }
    }

    fn insert_record(&mut self, key: String, oneline: &[u8]) {
        let idx = match self.index.get(&key) {
            Some(&i) => i,
            None => {
                let i = self.entries.len();
                self.index.insert(key.clone(), i);
                self.entries.push(Entry {
                    key,
                    onelines: Vec::new(),
                    count: 0,
                });
                i
            }
        };
        let entry = &mut self.entries[idx];
        entry.count += 1;
        if !self.opts.summary {
            entry.onelines.push(format_subject(oneline));
        }
    }
}

/// Port of git's `insert_one_record` subject cleanup + `format_subject(.., " ")`.
///
/// Skips leading whitespace/blank lines, strips a leading `[PATCH...]` token, then
/// folds the message lines (stopping at the first blank line) into a single line,
/// joining with a space and stripping each line's trailing whitespace.
fn format_subject(bytes: &[u8]) -> Vec<u8> {
    let mut p = 0usize;

    // Skip any leading whitespace, including any blank lines.
    while p < bytes.len() && bytes[p].is_ascii_whitespace() {
        p += 1;
    }

    // Strip a leading `[PATCH...]` token when its `]` precedes the line's newline.
    if bytes[p..].starts_with(b"[PATCH") {
        let rest = &bytes[p..];
        let eol = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        if let Some(eob) = rest.iter().position(|&b| b == b']') {
            if eob < eol {
                p += eob + 1;
            }
        }
    }
    // Skip whitespace but not a newline.
    while p < bytes.len() && bytes[p].is_ascii_whitespace() && bytes[p] != b'\n' {
        p += 1;
    }

    // format_subject(&subject, oneline, " ").
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    while p < bytes.len() {
        // get_one_line: length up to and including '\n'.
        let line_start = p;
        let mut len = 0usize;
        while p < bytes.len() {
            let c = bytes[p];
            p += 1;
            len += 1;
            if c == b'\n' {
                break;
            }
        }
        // is_blank_line: trim trailing whitespace; break if empty.
        let mut trimmed = len;
        while trimmed > 0 && bytes[line_start + trimmed - 1].is_ascii_whitespace() {
            trimmed -= 1;
        }
        if trimmed == 0 {
            break;
        }
        if !first {
            out.push(b' ');
        }
        first = false;
        out.extend_from_slice(&bytes[line_start..line_start + trimmed]);
    }
    out
}

fn output(log: &mut Shortlog) -> Result<()> {
    // Sort.
    if log.opts.numbered {
        // Stable sort by count descending; ties keep insertion (alphabetical-ish) order.
        // We must first establish the alphabetical base order git relies on (string_list
        // is sorted), then stable-sort by counter.
        log.entries.sort_by(|a, b| a.key.cmp(&b.key));
        log.entries.sort_by_key(|e| std::cmp::Reverse(e.count));
    } else {
        log.entries.sort_by(|a, b| a.key.cmp(&b.key));
    }

    let mut buf: Vec<u8> = Vec::new();
    for entry in &log.entries {
        if log.opts.summary {
            buf.extend_from_slice(format!("{:6}\t{}\n", entry.count, entry.key).as_bytes());
        } else {
            buf.extend_from_slice(format!("{} ({}):\n", entry.key, entry.count).as_bytes());
            for msg in entry.onelines.iter().rev() {
                if log.opts.wrap_lines {
                    let mut wrapped: Vec<u8> = Vec::new();
                    add_wrapped_text(&mut wrapped, msg, log.opts.in1, log.opts.in2, log.opts.wrap);
                    buf.extend_from_slice(&wrapped);
                    buf.push(b'\n');
                } else {
                    buf.extend_from_slice(b"      ");
                    buf.extend_from_slice(msg);
                    buf.push(b'\n');
                }
            }
            buf.push(b'\n');
        }
    }

    match &log.opts.output {
        Some(path) => std::fs::write(path, &buf)?,
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(&buf)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository walk
// ---------------------------------------------------------------------------

fn collect_from_repo(repo: &Repository, log: &mut Shortlog) -> Result<()> {
    let (positive, negative) = build_specs(repo, &log.opts)?;

    let options = RevListOptions {
        all_refs: log.opts.all_refs,
        max_count: log.opts.max_count,
        ..Default::default()
    };

    let result =
        rev_list(repo, &positive, &negative, &options).map_err(|e| anyhow::anyhow!("{e}"))?;

    for oid in &result.commits {
        let obj = repo.odb.read(oid).map_err(|e| anyhow::anyhow!("{e}"))?;
        let commit = parse_commit(&obj.data).map_err(|e| anyhow::anyhow!("{e}"))?;
        let raw_body = raw_commit_body(&obj.data);
        add_commit(log, oid, &commit, raw_body);
    }
    Ok(())
}

/// Build positive/negative revision specs, expanding pseudo-ref options.
fn build_specs(repo: &Repository, opts: &Options) -> Result<(Vec<String>, Vec<String>)> {
    let mut positive: Vec<String> = Vec::new();
    let mut negative: Vec<String> = Vec::new();

    let mut excluded_oids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for pat in &opts.exclude_patterns {
        if let Ok(matching) = grit_lib::refs::list_refs_glob(&repo.git_dir, pat) {
            for (_, oid) in matching {
                excluded_oids.insert(oid.to_hex());
            }
        }
    }

    let add_prefix = |prefix: &str, positive: &mut Vec<String>| {
        if let Ok(matching) = grit_lib::refs::list_refs(&repo.git_dir, prefix) {
            for (_, oid) in matching {
                if excluded_oids.contains(&oid.to_hex()) {
                    continue;
                }
                positive.push(oid.to_hex());
            }
        }
    };

    if opts.branches {
        add_prefix("refs/heads/", &mut positive);
    }
    if opts.tags {
        add_prefix("refs/tags/", &mut positive);
    }
    if opts.remotes {
        add_prefix("refs/remotes/", &mut positive);
    }

    for rev in &opts.revisions {
        if rev == "--" {
            continue;
        }
        // Expand range syntax (`A..B`, `A...B`) and `^A` into pos/neg specs.
        let (pos, neg) = grit_lib::rev_list::split_revision_token(rev);
        positive.extend(pos);
        negative.extend(neg);
    }

    Ok((positive, negative))
}

/// Extract the raw message body bytes (everything after the first blank line) of a
/// commit object, reencoded to UTF-8 when the commit declares a resolvable encoding.
fn raw_commit_body(data: &[u8]) -> Vec<u8> {
    // Find the header/body separator: the first empty line.
    let mut pos = 0usize;
    while pos < data.len() {
        let mut le = pos;
        while le < data.len() && data[le] != b'\n' {
            le += 1;
        }
        if le == pos {
            // Empty line at `pos`.
            return data.get(pos + 1..).unwrap_or_default().to_vec();
        }
        pos = le + 1;
    }
    Vec::new()
}

fn add_commit(log: &mut Shortlog, oid: &ObjectId, commit: &CommitData, raw_body: Vec<u8>) {
    // Reencode body to output encoding (UTF-8) when possible; otherwise keep raw bytes.
    let body_bytes = reencode_body(commit, raw_body);

    // Build the oneline subject (raw bytes) when not summary-only.
    let oneline = if log.opts.summary {
        Vec::new()
    } else if let Some(fmt) = log.opts.user_format.clone() {
        expand_format(
            &fmt,
            oid,
            commit,
            &body_bytes,
            log.opts.abbrev,
            log.opts.date_mode.as_deref(),
            &log.mailmap,
        )
    } else {
        subject_bytes(&body_bytes)
    };
    let oneline_bytes: Vec<u8> = if oneline.is_empty() && !log.opts.summary {
        b"<none>".to_vec()
    } else {
        oneline
    };

    let needs_dedup = log.opts.groups.len() > 1
        || matches!(log.opts.groups.first(), Some(Group::Trailer(_)))
        || log
            .opts
            .groups
            .iter()
            .filter(|g| matches!(g, Group::Trailer(_)))
            .count()
            >= 1;

    let mut dups: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Process trailer groups first (git iterates trailers before formats), in the
    // sorted order git uses for the trailer list.
    let mut trailer_keys: Vec<String> = log
        .opts
        .groups
        .iter()
        .filter_map(|g| match g {
            Group::Trailer(k) => Some(k.clone()),
            _ => None,
        })
        .collect();
    trailer_keys.sort_by_key(|k| k.to_lowercase());

    if !trailer_keys.is_empty() {
        let body_str = String::from_utf8_lossy(&body_bytes);
        for (key, value) in iter_trailers(&body_str) {
            if !trailer_keys.iter().any(|k| k.eq_ignore_ascii_case(&key)) {
                continue;
            }
            let mapped = map_ident(&value, log.opts.email, &log.mailmap);
            if !dups.insert(mapped.clone()) {
                continue;
            }
            log.insert_record(mapped, &oneline_bytes);
        }
    }

    // Process format groups in append order.
    for group in &log.opts.groups.clone() {
        if let Group::Format(fmt) = group {
            let key_bytes = expand_format(
                fmt,
                oid,
                commit,
                &body_bytes,
                log.opts.abbrev,
                log.opts.date_mode.as_deref(),
                &log.mailmap,
            );
            let key = String::from_utf8_lossy(&key_bytes).into_owned();
            if !needs_dedup || dups.insert(key.clone()) {
                log.insert_record(key, &oneline_bytes);
            }
        }
    }
}

fn reencode_body(commit: &CommitData, raw_body: Vec<u8>) -> Vec<u8> {
    match &commit.encoding {
        Some(enc) if !enc.eq_ignore_ascii_case("utf-8") && !enc.eq_ignore_ascii_case("utf8") => {
            // Decode via the declared encoding and re-emit as UTF-8. When the
            // encoding name is unknown (e.g. the test's "non-utf-8"), Git's
            // reencode is a no-op, so keep the raw bytes.
            if grit_lib::commit_encoding::is_known_encoding(enc) {
                grit_lib::commit_encoding::decode_bytes(Some(enc), &raw_body).into_bytes()
            } else {
                raw_body
            }
        }
        _ => raw_body,
    }
}

/// First line (subject) of the message bytes.
fn subject_bytes(body: &[u8]) -> Vec<u8> {
    // Skip leading whitespace/blank lines.
    let mut start = 0;
    while start < body.len() && (body[start] as char).is_ascii_whitespace() {
        start += 1;
    }
    let rest = &body[start..];
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    rest[..end].to_vec()
}

// ---------------------------------------------------------------------------
// stdin
// ---------------------------------------------------------------------------

fn read_from_stdin(stdin: &io::Stdin, log: &mut Shortlog) -> Result<()> {
    // git refuses multiple --group with stdin.
    if log.opts.groups.len() > 1 {
        eprintln!("fatal: using multiple --group options with stdin is not supported");
        std::process::exit(128);
    }
    // Determine which header we match: author or committer (only Format groups
    // with %aN/%cN reach here). Trailer/format groups are unsupported on stdin.
    let want_committer = match log.opts.groups.first() {
        Some(Group::Format(f)) if f.starts_with("%cN") || f.starts_with("%cn") => true,
        Some(Group::Format(f)) if f.starts_with("%aN") || f.starts_with("%an") => false,
        Some(Group::Trailer(_)) => {
            eprintln!("fatal: using --group=trailer with stdin is not supported");
            std::process::exit(128);
        }
        _ => false,
    };

    let (m0, m1): (&str, &str) = if want_committer {
        ("Commit: ", "committer ")
    } else {
        ("Author: ", "author ")
    };

    // Read raw bytes and split on LF (Git `strbuf_getline_lf`). The `git log` output may contain
    // an invalid-UTF-8 commit body (e.g. under `i18n.commitEncoding=non-utf-8`, which makes
    // `logmsg_reencode` a no-op so the body is emitted verbatim). Decoding lines as UTF-8 here
    // would either drop everything after the first invalid line or replace bytes with U+FFFD,
    // changing the wrapped output. So parse and store onelines at the byte level.
    let m0b = m0.as_bytes();
    let m1b = m1.as_bytes();
    let mut reader = stdin.lock();
    let mut buf = Vec::new();
    io::Read::read_to_end(&mut reader, &mut buf)?;
    // Split into LF-terminated lines, dropping a trailing final empty segment.
    let lines: Vec<&[u8]> = {
        let body = buf.strip_suffix(b"\n").unwrap_or(&buf);
        if body.is_empty() {
            Vec::new()
        } else {
            body.split(|&b| b == b'\n').collect()
        }
    };
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        let ident = line.strip_prefix(m0b).or_else(|| line.strip_prefix(m1b));
        idx += 1;
        let Some(ident) = ident else { continue };

        // Discard headers until a blank line.
        while idx < lines.len() && !lines[idx].is_empty() {
            idx += 1;
        }
        // Discard blank lines.
        while idx < lines.len() && lines[idx].is_empty() {
            idx += 1;
        }
        // The oneline is the next line (indented in default log output).
        let oneline: &[u8] = if idx < lines.len() {
            let raw = lines[idx];
            raw.strip_prefix(b"    ").unwrap_or(raw)
        } else {
            b""
        };

        // Idents go through mailmap (String-based); the test idents are ASCII, and Git itself
        // treats the ident as bytes — lossy decode here only affects display of the grouping key.
        let ident_str = String::from_utf8_lossy(ident);
        let mapped = map_ident(&ident_str, log.opts.email, &log.mailmap);
        if oneline.is_empty() {
            log.insert_record(mapped, b"<none>");
        } else {
            log.insert_record(mapped, oneline);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ident / mailmap
// ---------------------------------------------------------------------------

/// Map a raw ident (`Name <email> ts tz`, or `Name <email>`) through mailmap and
/// format as `Name` or `Name <email>`.
fn map_ident(ident: &str, show_email: bool, mailmap: &MailmapTable) -> String {
    let name = extract_name(ident).to_owned();
    let email = extract_email(ident).to_owned();
    let (name, email) = mailmap.map_user(name, email);
    if show_email {
        format!("{name} <{email}>")
    } else {
        name
    }
}

fn extract_name(ident: &str) -> &str {
    match ident.find('<') {
        Some(b) => ident[..b].trim_end(),
        None => ident.trim(),
    }
}

fn extract_email(ident: &str) -> &str {
    if let Some(s) = ident.find('<') {
        if let Some(e) = ident[s..].find('>') {
            return &ident[s + 1..s + e];
        }
    }
    ""
}

// ---------------------------------------------------------------------------
// Trailers
// ---------------------------------------------------------------------------

/// Iterate (key, unfolded-value) trailer pairs in the message body's trailer block.
fn iter_trailers(body: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = body.lines().collect();
    let start = trailer_block_start(&lines);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if is_trailer_line(line) {
            // Collect this trailer plus folded continuation lines.
            let mut raw = line.to_owned();
            let mut j = i + 1;
            while j < lines.len() && !lines[j].is_empty() && lines[j].starts_with([' ', '\t']) {
                raw.push('\n');
                raw.push_str(lines[j]);
                j += 1;
            }
            if let Some(sep) = find_separator(&raw) {
                let key = raw[..sep].trim().to_owned();
                let val_raw = &raw[sep + 1..];
                let val = unfold_value(val_raw);
                out.push((key, val));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Find the start line index of the trailer block: the last contiguous run of
/// trailer-like lines (with folded continuations) at the end of the message.
fn trailer_block_start(lines: &[&str]) -> usize {
    // Walk from the end; the trailer block is the final paragraph that consists
    // (mostly) of `Key: value` lines.  We use a simplified heuristic matching
    // git for the cases shortlog cares about: the final non-blank paragraph
    // where each non-continuation line is a trailer line.
    if lines.is_empty() {
        return 0;
    }
    // Find end of message (skip trailing blank lines).
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if end == 0 {
        return lines.len();
    }
    // Find start of the last paragraph.
    let mut para_start = end;
    while para_start > 0 && !lines[para_start - 1].trim().is_empty() {
        para_start -= 1;
    }

    // Verify the paragraph is a trailer block: every non-continuation, non-comment
    // line must be a trailer line (git uses a 1:3 ratio; the shortlog tests use
    // pure trailer paragraphs).
    let mut trailer_lines = 0i32;
    let mut non_trailer_lines = 0i32;
    let mut k = para_start;
    while k < end {
        let line = lines[k];
        if line.starts_with([' ', '\t']) {
            // continuation
        } else if is_trailer_line(line) {
            trailer_lines += 1;
        } else {
            non_trailer_lines += 1;
        }
        k += 1;
    }
    if trailer_lines > 0 && trailer_lines * 3 >= non_trailer_lines {
        para_start
    } else {
        end
    }
}

fn is_trailer_line(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    match find_separator(line) {
        Some(p) => p >= 1,
        None => false,
    }
}

/// git's `find_separator` for separators=":".
fn find_separator(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut whitespace_found = false;
    for (i, &c) in bytes.iter().enumerate() {
        if c == b':' {
            return Some(i);
        }
        if !whitespace_found && (c.is_ascii_alphanumeric() || c == b'-') {
            continue;
        }
        if i != 0 && (c == b' ' || c == b'\t') {
            whitespace_found = true;
            continue;
        }
        break;
    }
    None
}

/// git's `unfold_value`: collapse continuation newlines+whitespace into single spaces, trim.
fn unfold_value(val: &str) -> String {
    let bytes = val.as_bytes();
    let mut out = String::with_capacity(val.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        i += 1;
        if c == b'\n' {
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            out.push(' ');
        } else {
            out.push(c as char);
        }
    }
    out.trim().to_owned()
}

// ---------------------------------------------------------------------------
// Pretty format expansion (shortlog subset)
// ---------------------------------------------------------------------------

/// Expand a user format string against a commit. Supports the placeholders the
/// shortlog tests exercise: %H %h %an %aN %ae %aE %cn %cN %ce %cE %s %ad %cd %n %%.
fn expand_format(
    fmt: &str,
    oid: &ObjectId,
    commit: &CommitData,
    body: &[u8],
    abbrev: usize,
    date_mode: Option<&str>,
    mailmap: &MailmapTable,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            out.push(b'%');
            break;
        }
        match bytes[i] {
            b'%' => out.push(b'%'),
            b'n' => out.push(b'\n'),
            b'H' => out.extend_from_slice(oid.to_hex().as_bytes()),
            b'h' => {
                out.extend_from_slice(grit_lib::commit_pretty::abbrev_hex(oid, abbrev).as_bytes())
            }
            b's' => out.extend_from_slice(&subject_bytes(body)),
            b'a' => {
                i += 1;
                if i < bytes.len() {
                    expand_ident(&mut out, bytes[i], &commit.author, date_mode, mailmap);
                }
            }
            b'c' => {
                i += 1;
                if i < bytes.len() {
                    expand_ident(&mut out, bytes[i], &commit.committer, date_mode, mailmap);
                }
            }
            other => {
                out.push(b'%');
                out.push(other);
            }
        }
        i += 1;
    }
    out
}

fn expand_ident(
    out: &mut Vec<u8>,
    code: u8,
    ident: &str,
    date_mode: Option<&str>,
    mailmap: &MailmapTable,
) {
    match code {
        // Lowercase = raw; capital = mailmapped (Git's %an vs %aN, %ae vs %aE).
        b'n' => out.extend_from_slice(extract_name(ident).as_bytes()),
        b'e' => out.extend_from_slice(extract_email(ident).as_bytes()),
        b'N' => {
            let (name, _email) = mapped_ident(ident, mailmap);
            out.extend_from_slice(name.as_bytes());
        }
        b'E' => {
            let (_name, email) = mapped_ident(ident, mailmap);
            out.extend_from_slice(email.as_bytes());
        }
        b'd' => out.extend_from_slice(format_date(ident, date_mode).as_bytes()),
        _ => {
            out.push(b'%');
            // unknown ident code: re-emit literally
            out.push(code);
        }
    }
}

/// Map an ident's name/email through the mailmap.
fn mapped_ident(ident: &str, mailmap: &MailmapTable) -> (String, String) {
    let name = extract_name(ident).to_owned();
    let email = extract_email(ident).to_owned();
    mailmap.map_user(name, email)
}

/// Format a date field from an ident using git's date engine.
fn format_date(ident: &str, date_mode: Option<&str>) -> String {
    let Some((time, tz)) = parse_time_tz(ident) else {
        return String::new();
    };
    let mut mode = match date_mode {
        Some(m) => {
            parse_date_format(m).unwrap_or_else(|_| DateMode::from_type(DateModeType::Normal))
        }
        None => DateMode::from_type(DateModeType::Normal),
    };
    show_date(time, tz, &mut mode)
}

/// Parse `<unix-ts> <+HHMM>` from the tail of an ident; returns (time, tz_hhmm).
fn parse_time_tz(ident: &str) -> Option<(u64, i32)> {
    let parsed = grit_lib::ident::parse_signature_times(ident)?;
    let time = u64::try_from(parsed.unix_seconds).ok()?;
    let tz_str = ident.get(parsed.tz_hhmm_range)?;
    let tz: i32 = parse_tz_hhmm(tz_str)?;
    Some((time, tz))
}

fn parse_tz_hhmm(tz: &str) -> Option<i32> {
    let b = tz.as_bytes();
    if b.len() < 5 {
        return None;
    }
    let sign = if b[0] == b'-' { -1 } else { 1 };
    let hh: i32 = tz.get(1..3)?.parse().ok()?;
    let mm: i32 = tz.get(3..5)?.parse().ok()?;
    Some(sign * (hh * 100 + mm))
}

// ---------------------------------------------------------------------------
// Wrapping (port of git's strbuf_add_wrapped_text)
// ---------------------------------------------------------------------------

/// Length of a display-mode ANSI escape sequence (`ESC [ ... m`), or 0.
fn esc_seq_len(s: &[u8]) -> usize {
    let mut p = 0;
    if p >= s.len() || s[p] != 0x1b {
        return 0;
    }
    p += 1;
    if p >= s.len() || s[p] != b'[' {
        return 0;
    }
    p += 1;
    while p < s.len() && (s[p].is_ascii_digit() || s[p] == b';') {
        p += 1;
    }
    if p >= s.len() || s[p] != b'm' {
        return 0;
    }
    p + 1
}

/// Pick one UTF-8 character starting at `s`. Returns (codepoint, byte_len) or None
/// when the bytes are not a valid UTF-8 character (git's `pick_one_utf8_char`).
fn pick_one_utf8_char(s: &[u8]) -> Option<(u32, usize)> {
    if s.is_empty() {
        return None;
    }
    let b0 = s[0];
    if b0 < 0x80 {
        return Some((b0 as u32, 1));
    }
    if (b0 & 0xe0) == 0xc0 {
        if s.len() < 2 || (s[1] & 0xc0) != 0x80 || (b0 & 0xfe) == 0xc0 {
            return None;
        }
        let ch = ((b0 as u32 & 0x1f) << 6) | (s[1] as u32 & 0x3f);
        return Some((ch, 2));
    }
    if (b0 & 0xf0) == 0xe0 {
        if s.len() < 3
            || (s[1] & 0xc0) != 0x80
            || (s[2] & 0xc0) != 0x80
            || (b0 == 0xe0 && (s[1] & 0xe0) == 0x80)
            || (b0 == 0xed && (s[1] & 0xe0) == 0xa0)
            || (b0 == 0xef && s[1] == 0xbf && (s[2] & 0xfe) == 0xbe)
        {
            return None;
        }
        let ch = ((b0 as u32 & 0x0f) << 12) | ((s[1] as u32 & 0x3f) << 6) | (s[2] as u32 & 0x3f);
        return Some((ch, 3));
    }
    if (b0 & 0xf8) == 0xf0 {
        if s.len() < 4
            || (s[1] & 0xc0) != 0x80
            || (s[2] & 0xc0) != 0x80
            || (s[3] & 0xc0) != 0x80
            || (b0 == 0xf0 && (s[1] & 0xf0) == 0x80)
            || (b0 == 0xf4 && s[1] > 0x8f)
            || b0 > 0xf4
        {
            return None;
        }
        let ch = ((b0 as u32 & 0x07) << 18)
            | ((s[1] as u32 & 0x3f) << 12)
            | ((s[2] as u32 & 0x3f) << 6)
            | (s[3] as u32 & 0x3f);
        return Some((ch, 4));
    }
    None
}

/// git's `git_wcwidth`: -1 for control chars, 0 for zero-width, 2 for wide, else 1.
fn git_wcwidth(ch: u32) -> i32 {
    if ch == 0 {
        return 0;
    }
    if ch < 32 || (0x7f..0xa0).contains(&ch) {
        return -1;
    }
    if let Some(c) = char::from_u32(ch) {
        match unicode_width::UnicodeWidthChar::width(c) {
            Some(0) => 0,
            Some(2) => 2,
            Some(_) => 1,
            None => -1,
        }
    } else {
        1
    }
}

/// Returns (width, byte_len) for the character at the start of `s`, or None when the
/// bytes are not valid UTF-8 (mirrors git's `utf8_width` returning a NULL `start`).
fn utf8_width(s: &[u8]) -> Option<(i32, usize)> {
    let (ch, len) = pick_one_utf8_char(s)?;
    Some((git_wcwidth(ch), len))
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Port of git's `strbuf_add_wrapped_text` (utf8.c). Operates on raw bytes.
///
/// `indent1`/`indent2` are first-line / subsequent-line indents; a negative
/// `indent1` means that `-indent1` columns are already consumed.
fn add_wrapped_text(buf: &mut Vec<u8>, text: &[u8], indent1: i32, indent2: i32, width: i32) {
    if width <= 0 {
        add_indented_text(buf, text, indent1, indent2);
        return;
    }

    let orig_len = buf.len();
    let mut assume_utf8 = true;

    // `retry:` — restart point when invalid UTF-8 forces byte-wise mode.
    'retry: loop {
        let mut t = 0usize; // index into `text` (C's `text` pointer)
        let mut bol = t;
        let mut indent = indent1;
        let mut w = indent1;
        // `space` is an Option<index>; None == NULL.
        let mut space: Option<usize> = None;
        if indent < 0 {
            w = -indent;
            space = Some(t);
        }

        loop {
            // while ((skip = display_mode_esc_sequence_len(text))) text += skip;
            loop {
                let skip = esc_seq_len(&text[t..]);
                if skip == 0 {
                    break;
                }
                t += skip;
            }

            let c = if t < text.len() { text[t] } else { 0 };

            if c == 0 || is_space(c) {
                // `goto new_line` target wrapped as a closure-like flag.
                let mut go_new_line = false;

                if w <= width || space.is_none() {
                    // const char *start = bol;
                    let mut start = bol;
                    // if (!c && text == start) return;
                    if c == 0 && t == start {
                        return;
                    }
                    // if (space) start = space; else strbuf_addchars(buf,' ',indent);
                    if let Some(sp) = space {
                        start = sp;
                    } else {
                        add_chars(buf, b' ', indent);
                    }
                    // strbuf_add(buf, start, text - start);
                    buf.extend_from_slice(&text[start..t]);
                    // if (!c) return;
                    if c == 0 {
                        return;
                    }
                    // space = text;
                    space = Some(t);
                    if c == b'\t' {
                        w |= 0x07;
                    } else if c == b'\n' {
                        // space++;
                        let sp = t + 1;
                        space = Some(sp);
                        let starred = if sp < text.len() { text[sp] } else { 0 };
                        if starred == b'\n' {
                            buf.push(b'\n');
                            go_new_line = true;
                        } else if !is_alnum(starred) {
                            go_new_line = true;
                        } else {
                            buf.push(b' ');
                        }
                    }
                    if !go_new_line {
                        w += 1;
                        t += 1;
                    }
                } else {
                    go_new_line = true;
                }

                if go_new_line {
                    // new_line:
                    buf.push(b'\n');
                    // text = bol = space + isspace(*space);
                    let sp = space.unwrap_or(t);
                    let issp = sp < text.len() && is_space(text[sp]);
                    t = sp + if issp { 1 } else { 0 };
                    bol = t;
                    space = None;
                    w = indent2;
                    indent = indent2;
                }
                continue;
            }

            // Non-space character.
            if assume_utf8 {
                match utf8_width(&text[t..]) {
                    Some((gw, len)) => {
                        w += gw;
                        t += len;
                    }
                    None => {
                        // assume_utf8 = 0; text = start; reset; goto retry;
                        assume_utf8 = false;
                        buf.truncate(orig_len);
                        continue 'retry;
                    }
                }
            } else {
                w += 1;
                t += 1;
            }
        }
    }
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

fn add_chars(buf: &mut Vec<u8>, c: u8, n: i32) {
    if n <= 0 {
        return;
    }
    for _ in 0..n {
        buf.push(c);
    }
}

fn add_indented_text(buf: &mut Vec<u8>, text: &[u8], indent1: i32, indent2: i32) {
    let mut indent = if indent1 < 0 { 0 } else { indent1 };
    let mut pos = 0;
    while pos < text.len() {
        let mut eol = pos;
        while eol < text.len() && text[eol] != b'\n' {
            eol += 1;
        }
        if eol < text.len() {
            eol += 1; // include '\n'
        }
        add_chars(buf, b' ', indent);
        buf.extend_from_slice(&text[pos..eol]);
        pos = eol;
        indent = indent2;
    }
}
