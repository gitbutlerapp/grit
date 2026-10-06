//! `grit blame` — show what revision and author last modified each line of a file.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::blame::{
    apply_annotate_huge_graft_fixup, apply_final_content_overlay, apply_worktree_overlay,
    build_uncommitted_blame, compute_blame, compute_reverse_blame, load_graft_parents,
    parse_diff_algorithm_name, peel_to_commit_oid, read_object_for_blame,
    set_blame_indent_heuristic, set_promisor_hydrate_hook, BlameDiffAlgorithm, BlameLine,
    BlameTextconvContext,
};
use grit_lib::commit_encoding;
use grit_lib::config::{parse_color, ConfigSet};
use grit_lib::git_date::approx::approxidate_careful;
use grit_lib::mailmap::load_mailmap_table;
use grit_lib::objects::{parse_commit, CommitData, ObjectId};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{resolve_revision, resolve_revision_without_index_dwim};
use grit_lib::state::resolve_head;
use grit_lib::userdiff;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Arguments for `grit blame`.
#[derive(Debug, ClapArgs)]
#[command(about = "Show what revision and author last modified each line of a file")]
pub struct Args {
    /// Limit output to the given line range (e.g. -L 10,20).
    #[arg(short = 'L', action = clap::ArgAction::Append)]
    pub line_range: Vec<String>,

    /// Show long (full) commit hashes.
    #[arg(short = 'l')]
    pub long_hash: bool,

    /// Show blank object names for boundary commits.
    #[arg(short = 'b')]
    pub blank_boundary: bool,

    /// Suppress author name and timestamp.
    #[arg(short = 's')]
    pub suppress: bool,

    /// Use git-annotate compatible output.
    #[arg(short = 'c')]
    pub compatibility_output: bool,

    /// Show author email instead of name.
    #[arg(short = 'e', long = "show-email")]
    pub email: bool,

    /// Show author name even when blame.showEmail is enabled.
    #[arg(long = "no-show-email")]
    pub no_show_email: bool,

    /// Porcelain format for machine consumption.
    #[arg(short = 'p', long = "porcelain")]
    pub porcelain: bool,

    /// Like --porcelain but outputs header for every line.
    #[arg(long = "line-porcelain")]
    pub line_porcelain: bool,

    /// Ignore a specific revision when assigning blame.
    #[arg(long = "ignore-rev")]
    pub ignore_rev: Vec<String>,

    /// File listing revisions to ignore (one hex SHA per line).
    #[arg(long = "ignore-revs-file")]
    pub ignore_revs_file: Vec<String>,

    /// Color lines from the same commit in alternating colors.
    #[arg(long = "color-lines")]
    pub color_lines: bool,

    /// Color lines by age of the commit.
    #[arg(long = "color-by-age")]
    pub color_by_age: bool,

    /// Detect copies from other files (`-C[<score>]`).
    /// May be repeated (`-C -C -C`) for deeper copy search.
    #[arg(
        short = 'C',
        long = "find-copies",
        value_name = "score",
        num_args = 0..=1,
        default_missing_value = "",
        action = clap::ArgAction::Append
    )]
    pub copy_detection: Vec<String>,

    /// Detect moved lines within a file (`-M[<score>]`).
    #[arg(
        short = 'M',
        long = "find-renames",
        value_name = "score",
        num_args = 0..=1,
        default_missing_value = "",
        action = clap::ArgAction::Append
    )]
    pub move_detection: Vec<String>,

    /// Show the filename in the output.
    #[arg(short = 'f', long = "show-name")]
    pub show_name: bool,

    /// Use N digits to display object names (default 8, min 4).
    #[arg(long = "abbrev")]
    pub abbrev: Option<usize>,

    /// Show full object names (same as --abbrev=40).
    #[arg(long = "no-abbrev")]
    pub no_abbrev: bool,

    /// Treat root commits as normal commits (not boundaries).
    #[arg(long = "root")]
    pub root: bool,

    /// Walk history from older to newer (expects a revision range).
    #[arg(long = "reverse")]
    pub reverse: bool,

    /// Follow only first parents when walking merges.
    #[arg(long = "first-parent")]
    pub first_parent: bool,

    /// Choose diff algorithm.
    #[arg(long = "diff-algorithm")]
    pub diff_algorithm: Option<String>,

    /// Spend extra cycles to find better matches.
    #[arg(long = "minimal")]
    pub minimal: bool,

    /// Use the indent heuristic when diffing parent/child file versions.
    #[arg(long = "indent-heuristic", overrides_with = "no_indent_heuristic")]
    pub indent_heuristic: bool,

    /// Disable the indent heuristic.
    #[arg(long = "no-indent-heuristic", overrides_with = "indent_heuristic")]
    pub no_indent_heuristic: bool,

    /// Blame transformed (textconv) content.
    #[arg(long = "textconv")]
    pub textconv: bool,

    /// Disable textconv.
    #[arg(long = "no-textconv")]
    pub no_textconv: bool,

    /// Use this file's contents as the final image to annotate (git `--contents`).
    #[arg(long = "contents", value_name = "file")]
    pub contents: Option<String>,

    /// Report progress to stderr (honours `GIT_PROGRESS_DELAY`).
    #[arg(long = "progress")]
    pub progress: bool,

    /// Emit porcelain output incrementally (same headers as `--porcelain`).
    #[arg(long = "incremental")]
    pub incremental: bool,

    /// Override output encoding for commit metadata (`none` = raw object bytes).
    #[arg(long = "encoding", value_name = "ENC", allow_hyphen_values = true)]
    pub encoding: Option<String>,

    /// When true, emit git-annotate style output (tab-separated metadata).
    #[arg(skip)]
    pub annotate_output: bool,

    /// When true, treat lines as coming from a revision boundary.
    #[arg(skip)]
    pub boundary_revision: bool,

    /// Revision to blame from (and optional file after `--`).
    #[arg()]
    pub args: Vec<String>,
}

/// Parsed author/committer string.
#[derive(Debug, Clone)]
struct AuthorInfo {
    name: String,
    email: String,
    timestamp: i64,
    tz: String,
}

fn parse_author_field(raw: &str) -> AuthorInfo {
    // "Name <email> timestamp tz"
    let (name, rest) = match raw.find('<') {
        Some(lt) => (raw[..lt].trim().to_string(), &raw[lt..]),
        None => (raw.to_string(), ""),
    };
    let (email, rest) = match rest.find('>') {
        Some(gt) => (rest[1..gt].to_string(), rest[gt + 1..].trim()),
        None => (String::new(), ""),
    };
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let timestamp = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let tz = parts.get(1).unwrap_or(&"+0000").to_string();
    AuthorInfo {
        name,
        email,
        timestamp,
        tz,
    }
}

fn format_time(timestamp: i64, tz: &str) -> String {
    let offset_secs = parse_tz_offset_seconds(tz);
    let dt = OffsetDateTime::from_unix_timestamp(timestamp + offset_secs as i64)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let rendered = match time::format_description::parse_borrowed::<1>(
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    ) {
        Ok(fmt) => dt
            .format(&fmt)
            .unwrap_or_else(|_| "1970-01-01 00:00:00".to_owned()),
        Err(_) => "1970-01-01 00:00:00".to_owned(),
    };
    format!("{rendered} {tz}")
}

fn parse_tz_offset_seconds(tz: &str) -> i32 {
    if tz.len() < 5 {
        return 0;
    }
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let hours: i32 = tz[1..3].parse().unwrap_or(0);
    let minutes: i32 = tz[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

fn trim_ascii_end_bytes(mut s: &[u8]) -> &[u8] {
    while let Some(last) = s.last().copied() {
        if last == b' ' || last == b'\t' {
            s = &s[..s.len() - 1];
        } else {
            break;
        }
    }
    s
}

fn trim_ascii_start_bytes(mut s: &[u8]) -> &[u8] {
    while let Some(first) = s.first().copied() {
        if first == b' ' || first == b'\t' {
            s = &s[1..];
        } else {
            break;
        }
    }
    s
}

fn split_ident_line_bytes(raw: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let lt = raw.iter().position(|&b| b == b'<')?;
    let gt = raw[lt + 1..].iter().position(|&b| b == b'>')? + lt + 1;
    let name = trim_ascii_end_bytes(&raw[..lt]);
    let email = raw.get(lt + 1..gt)?;
    let tail = trim_ascii_start_bytes(raw.get(gt + 1..).unwrap_or_default());
    Some((name, email, tail))
}

fn angle_bracket_mail_bytes(email_inner: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(email_inner.len() + 2);
    v.push(b'<');
    v.extend_from_slice(email_inner);
    v.push(b'>');
    v
}

fn blame_summary_unicode(commit: &CommitData) -> String {
    commit
        .message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_owned()
}

#[derive(Clone)]
enum BlameMetaEncoding {
    Utf8,
    Raw,
    Reencode(String),
}

impl BlameMetaEncoding {
    fn from_config_and_cli(config: &ConfigSet, encoding_cli: Option<&str>) -> Result<Self> {
        if let Some(v) = encoding_cli {
            if v.eq_ignore_ascii_case("none") {
                return Ok(Self::Raw);
            }
            return Ok(Self::Reencode(v.to_owned()));
        }
        let log_enc = config
            .get("i18n.logOutputEncoding")
            .or_else(|| config.get("i18n.logoutputencoding"));
        let commit_enc = config
            .get("i18n.commitEncoding")
            .or_else(|| config.get("i18n.commitencoding"));
        if let Some(enc) = log_enc {
            if enc.is_empty() {
                return Ok(Self::Raw);
            }
            return Ok(Self::Reencode(enc));
        }
        if let Some(enc) = commit_enc {
            if enc.eq_ignore_ascii_case("utf-8") || enc.eq_ignore_ascii_case("utf8") {
                return Ok(Self::Utf8);
            }
            return Ok(Self::Reencode(enc));
        }
        Ok(Self::Utf8)
    }

    fn author_name_bytes(&self, commit: &CommitData) -> Result<Vec<u8>> {
        let raw = if commit.author_raw.is_empty() {
            commit.author.as_bytes()
        } else {
            &commit.author_raw
        };
        match self {
            Self::Utf8 => {
                let ai = parse_author_field(&commit.author);
                Ok(ai.name.into_bytes())
            }
            Self::Raw => {
                let (n, _, _) = split_ident_line_bytes(raw)
                    .ok_or_else(|| anyhow::anyhow!("malformed author line in commit object"))?;
                Ok(n.to_vec())
            }
            Self::Reencode(label) => {
                let ai = parse_author_field(&commit.author);
                commit_encoding::reencode_utf8_to_label(label, &ai.name)
                    .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}"))
            }
        }
    }

    fn author_mail_bytes(&self, commit: &CommitData) -> Result<Vec<u8>> {
        let raw = if commit.author_raw.is_empty() {
            commit.author.as_bytes()
        } else {
            &commit.author_raw
        };
        match self {
            Self::Utf8 => {
                let ai = parse_author_field(&commit.author);
                Ok(format!("<{}>", ai.email).into_bytes())
            }
            Self::Raw => {
                let (_, mail, _) = split_ident_line_bytes(raw)
                    .ok_or_else(|| anyhow::anyhow!("malformed author line in commit object"))?;
                Ok(angle_bracket_mail_bytes(mail))
            }
            Self::Reencode(label) => {
                let ai = parse_author_field(&commit.author);
                let line = format!("<{}>", ai.email);
                commit_encoding::reencode_utf8_to_label(label, &line)
                    .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}"))
            }
        }
    }

    /// Author name bytes after optional mailmap (UTF-8 canonical name from map).
    fn author_name_bytes_mailmapped(
        &self,
        commit: &CommitData,
        mailmap: &grit_lib::mailmap::MailmapTable,
    ) -> Result<Vec<u8>> {
        if mailmap.is_empty() {
            return self.author_name_bytes(commit);
        }
        let ai = parse_author_field(&commit.author);
        let (n, _) = mailmap.map_user(ai.name, ai.email);
        match self {
            Self::Utf8 => Ok(n.into_bytes()),
            Self::Raw => self.author_name_bytes(commit),
            Self::Reencode(label) => commit_encoding::reencode_utf8_to_label(label, &n)
                .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}")),
        }
    }

    /// `author-mail` bytes after optional mailmap.
    fn author_mail_bytes_mailmapped(
        &self,
        commit: &CommitData,
        mailmap: &grit_lib::mailmap::MailmapTable,
    ) -> Result<Vec<u8>> {
        if mailmap.is_empty() {
            return self.author_mail_bytes(commit);
        }
        let ai = parse_author_field(&commit.author);
        let (_, e) = mailmap.map_user(ai.name, ai.email);
        let line = format!("<{e}>");
        match self {
            Self::Utf8 => Ok(line.into_bytes()),
            Self::Raw => self.author_mail_bytes(commit),
            Self::Reencode(label) => commit_encoding::reencode_utf8_to_label(label, &line)
                .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}")),
        }
    }

    fn committer_name_bytes(&self, commit: &CommitData) -> Result<Vec<u8>> {
        let raw = if commit.committer_raw.is_empty() {
            commit.committer.as_bytes()
        } else {
            &commit.committer_raw
        };
        match self {
            Self::Utf8 => {
                let ci = parse_author_field(&commit.committer);
                Ok(ci.name.into_bytes())
            }
            Self::Raw => {
                let (n, _, _) = split_ident_line_bytes(raw)
                    .ok_or_else(|| anyhow::anyhow!("malformed committer line in commit object"))?;
                Ok(n.to_vec())
            }
            Self::Reencode(label) => {
                let ci = parse_author_field(&commit.committer);
                commit_encoding::reencode_utf8_to_label(label, &ci.name)
                    .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}"))
            }
        }
    }

    fn committer_mail_bytes(&self, commit: &CommitData) -> Result<Vec<u8>> {
        let raw = if commit.committer_raw.is_empty() {
            commit.committer.as_bytes()
        } else {
            &commit.committer_raw
        };
        match self {
            Self::Utf8 => {
                let ci = parse_author_field(&commit.committer);
                Ok(format!("<{}>", ci.email).into_bytes())
            }
            Self::Raw => {
                let (_, mail, _) = split_ident_line_bytes(raw)
                    .ok_or_else(|| anyhow::anyhow!("malformed committer line in commit object"))?;
                Ok(angle_bracket_mail_bytes(mail))
            }
            Self::Reencode(label) => {
                let ci = parse_author_field(&commit.committer);
                let line = format!("<{}>", ci.email);
                commit_encoding::reencode_utf8_to_label(label, &line)
                    .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}"))
            }
        }
    }

    fn summary_bytes(&self, commit: &CommitData) -> Result<Vec<u8>> {
        match self {
            Self::Utf8 => Ok(blame_summary_unicode(commit).into_bytes()),
            Self::Raw => {
                let body = commit
                    .raw_message
                    .as_deref()
                    .unwrap_or(commit.message.as_bytes());
                let first = body
                    .split(|&b| b == b'\n')
                    .find(|line| !line.iter().all(|&b| matches!(b, b' ' | b'\t' | b'\r')))
                    .unwrap_or_default();
                Ok(first.to_vec())
            }
            Self::Reencode(label) => {
                let sum = blame_summary_unicode(commit);
                commit_encoding::reencode_utf8_to_label(label, &sum)
                    .ok_or_else(|| anyhow::anyhow!("unsupported blame output encoding: {label}"))
            }
        }
    }
}

fn config_bool(config: &ConfigSet, key: &str) -> bool {
    matches!(config.get_bool(key), Some(Ok(true)))
}

/// Git `color.blame.highlightRecent` / `parse_color_fields` in `git/builtin/blame.c`: comma-separated
/// tokens alternate color, date, color, date, … and must end after a color; the final slot's hop is `TIME_MAX`.
fn parse_blame_highlight_recent(value: &str) -> Result<Vec<(i64, String)>> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    let mut slots: Vec<(i64, String)> = Vec::new();
    let mut expect_date = false;
    for item in parts {
        if item.is_empty() {
            continue;
        }
        if !expect_date {
            let ansi = parse_color(item).map_err(|e| {
                anyhow::anyhow!("invalid color in color.blame.highlightRecent: {e}")
            })?;
            slots.push((0, ansi));
            expect_date = true;
        } else {
            let hop = approxidate_careful(item, None) as i64;
            let last = slots.last_mut().ok_or_else(|| {
                anyhow::anyhow!("color.blame.highlightRecent: internal parse error")
            })?;
            last.0 = hop;
            expect_date = false;
        }
    }
    if !expect_date {
        bail!("color.blame.highlightRecent must end with a color");
    }
    let last = slots.last_mut().ok_or_else(|| {
        anyhow::anyhow!("color.blame.highlightRecent must contain at least one color")
    })?;
    last.0 = i64::MAX;
    Ok(slots)
}

/// ANSI sequence for repeated-line blame metadata (Git default cyan when unset).
const GIT_COLOR_CYAN: &str = "\x1b[36m";

/// Applies `blame.coloring` when no `--color-lines` / `--color-by-age` flags are given, and loads
/// color strings from `color.blame.*` (matches `git/builtin/blame.c`).
fn apply_blame_color_config(config: &ConfigSet, args: &mut Args) -> Result<BlameColorStyle> {
    let cli_color = args.color_lines || args.color_by_age;
    if !cli_color {
        match config.get("blame.coloring").as_deref() {
            Some("repeatedLines") => args.color_lines = true,
            Some("highlightRecent") => args.color_by_age = true,
            Some("none") => {}
            Some(other) => {
                eprintln!("warning: invalid value for 'blame.coloring': '{other}'");
            }
            None => {}
        }
    }

    let mut style = BlameColorStyle::default();

    if args.color_lines {
        if let Some(raw) = config.get("color.blame.repeatedLines") {
            if raw.trim().is_empty() {
                style.repeated_lines_ansi.clear();
            } else {
                style.repeated_lines_ansi = parse_color(raw.trim()).map_err(|e| {
                    anyhow::anyhow!("invalid value for 'color.blame.repeatedLines': {e}")
                })?;
            }
        }
        if style.repeated_lines_ansi.is_empty() {
            style.repeated_lines_ansi = GIT_COLOR_CYAN.to_string();
        }
    }

    if args.color_by_age {
        let default_spec = "blue,12 month ago,white,1 month ago,red";
        let spec = config
            .get("color.blame.highlightRecent")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| default_spec.to_string());
        style.age_buckets = parse_blame_highlight_recent(&spec)?;
    }

    Ok(style)
}

/// Parsed `color.blame.*` settings for default blame output.
#[derive(Debug, Default)]
struct BlameColorStyle {
    /// ANSI start sequence for contiguous lines from the same commit (`--color-lines`).
    repeated_lines_ansi: String,
    /// `(hop, ansi)` buckets for `--color-by-age` / `highlightRecent` (last hop is `i64::MAX`).
    age_buckets: Vec<(i64, String)>,
}

/// Promisor-hydration hook handed to `grit_lib::blame`: on a missing object the
/// engine asks the CLI to lazily fetch it from the promisor remote. Transport
/// stays in the CLI; the lib only invokes this callback.
fn blame_promisor_hydrate(repo: &Repository, oid: ObjectId) {
    let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(repo, oid);
}

pub fn run(mut args: Args) -> Result<()> {
    if args.compatibility_output {
        args.annotate_output = true;
    }

    set_promisor_hydrate_hook(blame_promisor_hydrate);

    let repo = Repository::discover(None).context("not a git repository")?;
    let odb = Odb::new(&repo.git_dir.join("objects"));
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let grafts = load_graft_parents(&repo.git_dir);

    if !args.email && config_bool(&config, "blame.showEmail") {
        args.email = true;
    }
    if args.no_show_email {
        args.email = false;
    }

    let mut diff_algorithm = config
        .get("diff.algorithm")
        .and_then(|name| parse_diff_algorithm_name(&name))
        .unwrap_or(BlameDiffAlgorithm::Myers);

    if args.minimal {
        diff_algorithm = BlameDiffAlgorithm::Minimal;
    }
    if let Some(name) = args.diff_algorithm.as_deref() {
        diff_algorithm = parse_diff_algorithm_name(name)
            .ok_or_else(|| anyhow::anyhow!("invalid --diff-algorithm: {name}"))?;
    }

    // Indent heuristic: CLI flags override `diff.indentHeuristic` (t4061).
    let mut indent_heuristic = config_bool(&config, "diff.indentHeuristic");
    if args.indent_heuristic {
        indent_heuristic = true;
    }
    if args.no_indent_heuristic {
        indent_heuristic = false;
    }
    set_blame_indent_heuristic(indent_heuristic);

    let mut normalized_positional = Vec::new();
    normalize_detection_args(&args.copy_detection, &mut normalized_positional);
    normalize_detection_args(&args.move_detection, &mut normalized_positional);
    normalized_positional.extend(args.args.iter().cloned());

    let (rev, mut file_path) = parse_blame_args(&odb, &repo, &normalized_positional)?;
    if let Some(work_tree) = repo.work_tree.as_deref() {
        let cwd = std::env::current_dir().context("getting current directory")?;
        let prefix = crate::pathspec::pathdiff(&cwd, work_tree);
        file_path =
            crate::pathspec::normalize_worktree_file_path(&file_path, work_tree, prefix.as_deref());
    }

    // Working-copy blame (no revision, no --contents) blames the file on disk; git lstat's
    // it first and aborts when it is absent (e.g. a skip-worktree out-of-cone path that is
    // not materialized). Match git's exact message (t1092 blame outside sparse definition).
    if rev.is_none() && args.contents.is_none() {
        if let Some(work_tree) = repo.work_tree.as_deref() {
            let abs_path = work_tree.join(&file_path);
            if std::fs::symlink_metadata(&abs_path).is_err() {
                bail!("fatal: Cannot lstat '{file_path}': No such file or directory");
            }
        }
    }

    let use_textconv = !args.no_textconv;
    let copy_depth = args.copy_detection.len();
    let textconv_ctx = Some(BlameTextconvContext::new(&repo));

    if rev.as_deref().is_some_and(|r| r.starts_with('^')) {
        args.boundary_revision = true;
    }

    let start_oid = match &rev {
        Some(r) => {
            let oid = resolve_blame_start_oid(&repo, r)?;
            peel_to_commit_oid(&odb, oid)?
                .ok_or_else(|| anyhow::anyhow!("revision does not resolve to a commit"))?
        }
        None => {
            let head = resolve_head(&repo.git_dir)?;
            match head.oid() {
                Some(oid) => *oid,
                None => bail!("cannot blame on unborn branch"),
            }
        }
    };

    // Build the set of revisions to ignore.
    // Sources:
    // 1) config blame.ignoreRevsFile (can be multi-valued)
    // 2) CLI --ignore-revs-file (processed after config; empty string resets)
    // 3) CLI --ignore-rev
    let mut ignore_revs = HashSet::new();
    let mut ignore_revs_files = config.get_all("blame.ignoreRevsFile");
    for file in &args.ignore_revs_file {
        if file.is_empty() {
            ignore_revs_files.clear();
        } else {
            ignore_revs_files.push(file.clone());
        }
    }

    for file in &ignore_revs_files {
        let contents = std::fs::read_to_string(file)
            .with_context(|| format!("could not open file with revisions to ignore: {file}"))?;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let oid = resolve_revision(&repo, line)
                .map_err(|_| anyhow::anyhow!("invalid object name: {line}"))?;
            if let Some(oid) = peel_to_commit_oid(&odb, oid)? {
                ignore_revs.insert(oid);
            }
        }
    }

    for rev_str in &args.ignore_rev {
        let oid = resolve_revision(&repo, rev_str)
            .with_context(|| format!("cannot find revision {rev_str} to ignore"))?;
        let oid = peel_to_commit_oid(&odb, oid)?
            .ok_or_else(|| anyhow::anyhow!("cannot find revision {rev_str} to ignore"))?;
        ignore_revs.insert(oid);
    }

    let mark_unblamable = config_bool(&config, "blame.markUnblamableLines");
    let mark_ignored = config_bool(&config, "blame.markIgnoredLines");

    let blame_color_style = apply_blame_color_config(&config, &mut args)?;

    let contents_override = if let Some(ref p) = args.contents {
        let path = PathBuf::from(p);
        Some(
            std::fs::read_to_string(&path)
                .with_context(|| format!("could not read --contents file: {p}"))?,
        )
    } else {
        None
    };

    let mut blame_lines = if args.reverse {
        let rev_spec = rev
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--reverse requires a <rev1>..<rev2> range"))?;
        let (range_start, range_end) = parse_reverse_range_oids(&repo, rev_spec)?;
        compute_reverse_blame(
            &odb,
            range_start,
            range_end,
            &file_path,
            diff_algorithm,
            textconv_ctx.as_ref(),
            use_textconv,
            args.first_parent,
        )?
    } else {
        match compute_blame(
            &odb,
            start_oid,
            &file_path,
            &ignore_revs,
            diff_algorithm,
            textconv_ctx.as_ref(),
            use_textconv,
            copy_depth,
            args.first_parent,
            &grafts,
        ) {
            Ok(lines) => lines,
            Err(e) if rev.is_none() => {
                // When no explicit revision is given and the file is not in HEAD's
                // tree (e.g. during a conflicted merge), fall back to reading the
                // working tree (or index conflict stage) file and best-effort
                // attribute against HEAD history.
                ensure_index_knows_path(&repo, &file_path)
                    .with_context(|| format!("file '{file_path}' not found in revision"))?;
                let content = if let Some(work_tree) = repo.work_tree.as_deref() {
                    let abs_path = work_tree.join(&file_path);
                    if abs_path.exists() {
                        std::fs::read_to_string(&abs_path)
                            .with_context(|| format!("file '{file_path}' not found"))?
                    } else {
                        // File not in worktree; try reading from highest conflict stage in index
                        read_from_index_conflict(&repo, &odb, &file_path)
                            .with_context(|| format!("file '{file_path}' not found in revision"))?
                    }
                } else {
                    return Err(e.into());
                };
                build_uncommitted_blame(
                    &odb,
                    start_oid,
                    &file_path,
                    &content,
                    &ignore_revs,
                    diff_algorithm,
                    textconv_ctx.as_ref(),
                    use_textconv,
                    copy_depth,
                    args.first_parent,
                    &grafts,
                )?
            }
            Err(e) => return Err(e.into()),
        }
    };

    if !args.reverse {
        if let Some(ref final_text) = contents_override {
            if let Some(overlaid) = apply_final_content_overlay(
                &odb,
                start_oid,
                &file_path,
                &blame_lines,
                final_text,
                textconv_ctx.as_ref(),
                use_textconv,
            )? {
                blame_lines = overlaid;
            }
        } else if rev.is_none() {
            if let Some(overlaid) = apply_worktree_overlay(
                &repo,
                &odb,
                start_oid,
                &file_path,
                &blame_lines,
                textconv_ctx.as_ref(),
                use_textconv,
            )? {
                blame_lines = overlaid;
            }
        }
    }

    if !args.reverse {
        apply_annotate_huge_graft_fixup(&odb, start_oid, &file_path, &grafts, &mut blame_lines)?;
    }

    // Apply line range filters (`-L` can be repeated; semantics match git `line-range.c`).
    if !args.line_range.is_empty() {
        let line_texts = build_final_line_texts(&blame_lines);
        let mut keep = HashSet::new();
        let mut range_ctx = LineRangeParseCtx {
            blame_lines: &blame_lines,
            file_path: &file_path,
            textconv: textconv_ctx.as_ref(),
        };
        let mut anchor: i64 = 1;
        for range in &args.line_range {
            let (start, end) =
                parse_blame_line_range_arg(range, &line_texts, anchor, &mut range_ctx)?;
            for lineno in start..=end {
                keep.insert(lineno);
            }
            anchor = end as i64 + 1;
        }
        blame_lines.retain(|b| keep.contains(&b.final_lineno));
    }

    if args.progress
        && !args.reverse
        && !blame_lines.is_empty()
        && !(args.porcelain || args.line_porcelain || args.incremental)
    {
        let delay_ms: u64 = std::env::var("GIT_PROGRESS_DELAY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let total = blame_lines.len();
        if delay_ms == 0 {
            eprintln!("Blaming lines: 100% ({total}/{total}), done.");
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Preload commits for display
    let zero = grit_lib::diff::zero_oid();
    let mut commits: HashMap<ObjectId, CommitData> = HashMap::new();
    for bl in &blame_lines {
        if let std::collections::hash_map::Entry::Vacant(e) = commits.entry(bl.oid) {
            if bl.oid == zero {
                // Fake commit for uncommitted/conflicted content
                e.insert(CommitData {
                    tree: zero,
                    parents: vec![],
                    author: "Not Committed Yet <not.committed.yet> 0 +0000".to_string(),
                    committer: "Not Committed Yet <not.committed.yet> 0 +0000".to_string(),
                    author_raw: Vec::new(),
                    committer_raw: Vec::new(),
                    encoding: None,
                    message: String::new(),
                    raw_message: None,
                });
            } else {
                let obj = read_object_for_blame(&odb, &bl.oid)?;
                e.insert(parse_commit(&obj.data)?);
            }
        }
    }

    let meta_enc = BlameMetaEncoding::from_config_and_cli(&config, args.encoding.as_deref())?;
    let mailmap = load_mailmap_table(&repo).unwrap_or_default();

    if args.porcelain || args.line_porcelain || args.incremental {
        write_porcelain(
            &mut out,
            &blame_lines,
            &commits,
            &file_path,
            args.line_porcelain,
            args.incremental,
            &meta_enc,
            &mailmap,
            mark_unblamable,
            mark_ignored,
        )?;
    } else if args.annotate_output {
        write_annotate(
            &mut out,
            &blame_lines,
            &commits,
            &args,
            &mailmap,
            &file_path,
        )?;
    } else {
        write_default(
            &mut out,
            &blame_lines,
            &commits,
            &args,
            &mailmap,
            &blame_color_style,
            &file_path,
            mark_unblamable,
            mark_ignored,
        )?;
    }

    Ok(())
}

/// Read file content from index conflict stages (for blame during merge conflicts).
fn read_from_index_conflict(repo: &Repository, odb: &Odb, file_path: &str) -> Result<String> {
    let index = repo.load_index().context("loading index")?;
    let path_bytes = file_path.as_bytes();
    // Find the highest-stage entry for this path (prefer stage 3, then 2, then 1)
    let mut best: Option<&grit_lib::index::IndexEntry> = None;
    for entry in &index.entries {
        if entry.path == path_bytes
            && entry.stage() > 0
            && best.is_none_or(|b| entry.stage() > b.stage())
        {
            best = Some(entry);
        }
    }
    let entry = best.ok_or_else(|| anyhow::anyhow!("file not in index"))?;
    let obj = read_object_for_blame(odb, &entry.oid)?;
    String::from_utf8(obj.data).context("blob is not valid UTF-8")
}

fn normalize_detection_args(values: &[String], positional: &mut Vec<String>) {
    for value in values {
        if value.is_empty() {
            continue;
        }
        if value.parse::<usize>().ok().is_some_and(|n| n > 0) {
            continue;
        }
        positional.push(value.clone());
    }
}

fn looks_like_object_id(s: &str) -> bool {
    let b = s.as_bytes();
    if !(4..=40).contains(&b.len()) {
        return false;
    }
    b.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// True when `spec` resolves to an object that peels to a commit (not a lone blob/tree).
fn spec_resolves_to_commit(odb: &Odb, repo: &Repository, spec: &str) -> bool {
    let normalized = spec.strip_prefix('^').unwrap_or(spec);
    let Ok(oid) = resolve_revision_without_index_dwim(repo, normalized) else {
        return false;
    };
    peel_to_commit_oid(odb, oid).ok().flatten().is_some()
}

fn ensure_index_knows_path(repo: &Repository, file_path: &str) -> Result<()> {
    let index = repo.load_index().context("loading index")?;
    let path = file_path.as_bytes();
    if index.entries.iter().any(|entry| entry.path == path) {
        return Ok(());
    }
    bail!("file not in index");
}

fn parse_blame_args(
    odb: &Odb,
    repo: &Repository,
    args: &[String],
) -> Result<(Option<String>, String)> {
    match args.len() {
        0 => bail!("usage: grit blame [<rev>] [--] <file>"),
        1 => Ok((None, args[0].clone())),
        2 if args[0] == "--" => Ok((None, args[1].clone())),
        2 => {
            let a0 = &args[0];
            let a1 = &args[1];
            let c0 = spec_resolves_to_commit(odb, repo, a0);
            let c1 = spec_resolves_to_commit(odb, repo, a1);
            match (c0, c1) {
                (true, false) => Ok((Some(a0.clone()), a1.clone())),
                (false, true) => Ok((Some(a1.clone()), a0.clone())),
                (true, true) => Ok((Some(a0.clone()), a1.clone())),
                (false, false) => {
                    // Neither peels to a commit; keep legacy heuristic for odd cases.
                    if resolve_revision_without_index_dwim(repo, a1).is_ok()
                        || a1 == "HEAD"
                        || looks_like_object_id(a1)
                    {
                        Ok((Some(a1.clone()), a0.clone()))
                    } else {
                        Ok((Some(a0.clone()), a1.clone()))
                    }
                }
            }
        }
        3 if args[1] == "--" => Ok((Some(args[0].clone()), args[2].clone())),
        _ => bail!("usage: grit blame [<rev>] [--] <file>"),
    }
}

fn resolve_blame_start_oid(repo: &Repository, rev_spec: &str) -> Result<ObjectId> {
    if let Some(stripped) = rev_spec.strip_prefix('^') {
        return resolve_blame_start_oid(repo, stripped);
    }

    if let Some((lhs, rhs)) = rev_spec.split_once("..") {
        if rhs.is_empty() {
            return resolve_revision(repo, "HEAD").map_err(Into::into);
        }

        if lhs.is_empty() {
            return resolve_revision(repo, rhs).map_err(Into::into);
        }

        // Accept two-dot ranges by resolving the right side (or merge base
        // from the rev parser in cases where that is appropriate).
        return resolve_revision(repo, rhs).map_err(Into::into);
    }
    resolve_revision(repo, rev_spec).map_err(Into::into)
}

fn parse_reverse_range_oids(repo: &Repository, rev_spec: &str) -> Result<(ObjectId, ObjectId)> {
    let (lhs, rhs) = rev_spec
        .split_once("..")
        .ok_or_else(|| anyhow::anyhow!("--reverse requires a <rev1>..<rev2> range"))?;
    if lhs.is_empty() || rhs.is_empty() {
        bail!("--reverse requires a <rev1>..<rev2> range");
    }
    let start = resolve_revision(repo, lhs)?;
    let end = resolve_revision(repo, rhs)?;
    Ok((start, end))
}

struct LineRangeParseCtx<'a> {
    blame_lines: &'a [BlameLine],
    file_path: &'a str,
    textconv: Option<&'a BlameTextconvContext>,
}

fn max_line_no(blame_lines: &[BlameLine]) -> usize {
    blame_lines
        .iter()
        .map(|b| b.final_lineno)
        .max()
        .unwrap_or(0)
}

/// Final file lines in order (1-based line *i* is `lines[i - 1]`), matching `blame_nth_line` indexing.
fn build_final_line_texts(blame_lines: &[BlameLine]) -> Vec<String> {
    let n = max_line_no(blame_lines);
    let mut out = vec![String::new(); n];
    for bl in blame_lines {
        if bl.final_lineno > 0 && bl.final_lineno <= n {
            out[bl.final_lineno - 1] = bl.content.clone();
        }
    }
    out
}

/// One `-L` argument: same rules as git `parse_range_arg` / `line-range.c`.
fn parse_blame_line_range_arg(
    arg: &str,
    lines: &[String],
    anchor: i64,
    ctx: &mut LineRangeParseCtx<'_>,
) -> Result<(usize, usize)> {
    let lno = lines.len() as i64;
    let mut anchor = anchor;
    if anchor < 1 {
        anchor = 1;
    }
    if anchor > lno {
        anchor = lno + 1;
    }

    if arg.starts_with(':') || arg.starts_with("^:") {
        let (begin, end) = parse_range_funcname_blame(arg, lines, anchor, ctx)?;
        return Ok(clamp_range_to_file(begin, end, lno));
    }

    let (mut rest, mut begin): (&str, i64) = parse_loc_git(arg, lines, -anchor)?;
    let mut end: i64 = 0;
    if let Some(after) = rest.strip_prefix(',') {
        let (r, e) = parse_loc_git(after, lines, begin + 1)?;
        rest = r;
        end = e;
    }

    if !rest.is_empty() {
        bail!("invalid -L range: trailing garbage: {rest:?}");
    }

    if begin != 0 && end != 0 && end < begin {
        std::mem::swap(&mut begin, &mut end);
    }

    if (lno == 0 && (begin != 0 || end != 0)) || (lno > 0 && begin > lno) {
        bail!(
            "file has only {} line{}",
            lno,
            if lno == 1 { "" } else { "s" }
        );
    }

    let mut bottom = begin;
    let mut top = end;
    if bottom < 1 {
        bottom = 1;
    }
    // Git `parse_range_arg` leaves `end` at 0 when the range has no second endpoint (`-L N` or
    // `-L N,`); `builtin/blame.c` then sets `top = lno` (annotate through EOF).
    if top < 1 || lno < top {
        top = lno;
    }

    Ok((bottom as usize, top as usize))
}

fn clamp_range_to_file(begin: usize, end: usize, lno: i64) -> (usize, usize) {
    let mut bottom = begin as i64;
    let mut top = end as i64;
    if bottom < 1 {
        bottom = 1;
    }
    if top < 1 || lno < top {
        top = lno;
    }
    (bottom as usize, top as usize)
}

/// Git `parse_loc` for blame: `begin` is negative for the start endpoint (`-anchor`), positive for the end (`start+1`).
fn parse_loc_git<'a>(mut spec: &'a str, lines: &[String], begin: i64) -> Result<(&'a str, i64)> {
    let lines_count = lines.len() as i64;

    // Endpoint `+N` / `-N` line counts (only when `begin >= 1`).
    if begin >= 1 && (spec.starts_with('+') || spec.starts_with('-')) {
        let (num, consumed) = parse_signed_offset_after_sign(spec)?;
        let rest = &spec[consumed..];
        if num == 0 {
            bail!("-L invalid empty range");
        }
        let ret = if num > 0 {
            begin + num - 2
        } else {
            let n = begin + num;
            if n > 0 {
                n
            } else {
                1
            }
        };
        return Ok((rest, ret));
    }

    if let Ok((n, consumed)) = parse_decimal_line_number(spec) {
        let rest = &spec[consumed..];
        if n <= 0 {
            bail!("-L invalid line number: {n}");
        }
        return Ok((rest, n));
    }

    if let Some(rest) = spec.strip_prefix('$') {
        return Ok((rest, lines_count));
    }

    // Regex or remaining forms: resolve search start from `begin`.
    let mut search_1based = begin;
    if search_1based < 0 {
        if !spec.starts_with('^') {
            search_1based = -search_1based;
        } else {
            search_1based = 1;
            spec = &spec[1..];
        }
    }

    if !spec.starts_with('/') {
        return Ok((spec, 0));
    }

    let Some(slash_end) = find_slash_delimited_regex_end(spec) else {
        return Ok((spec, 0));
    };

    let pattern = &spec[1..slash_end];
    let after = &spec[slash_end + 1..];

    let mut idx0 = search_1based - 1;
    if idx0 < 0 {
        idx0 = 0;
    }
    if idx0 >= lines_count {
        bail!(
            "-L parameter '/{pattern}/' starting at line {}: no such line",
            search_1based.max(1)
        );
    }

    let idx0 = idx0 as usize;
    // Git passes a pointer into the full file buffer to `regexec`, so the pattern can match
    // anywhere from the anchor line through EOF (not restricted to the first line).
    let mut hay = String::new();
    let mut line_starts: Vec<usize> = Vec::new();
    for li in idx0..lines.len() {
        line_starts.push(hay.len());
        hay.push_str(&lines[li]);
        if li + 1 < lines.len() {
            hay.push('\n');
        }
    }
    let hay_len = hay.len();

    let re = compile_blame_line_regex(pattern)?;
    let Some(m) = re.find(&hay) else {
        bail!(
            "-L parameter '/{pattern}/' starting at line {}: no match",
            idx0 + 1
        );
    };
    let pos = m.start();
    // Contiguous partitions: line `k` (1-based in full file) covers
    // `[line_starts[i], line_starts[i+1])` in `hay`, where `line_starts` is relative to the
    // sliced tail. `$` at EOF yields `pos == hay.len()`, which belongs to the last line.
    let matched_line = if pos >= hay_len {
        idx0 + line_starts.len()
    } else {
        let rel = line_starts
            .iter()
            .enumerate()
            .rfind(|(_, &s)| s <= pos)
            .map(|(i, _)| i)
            .unwrap_or(0);
        idx0 + rel + 1
    };
    Ok((after, matched_line as i64))
}

fn parse_signed_offset_after_sign(spec: &str) -> Result<(i64, usize)> {
    let bytes = spec.as_bytes();
    if bytes.is_empty() {
        return Ok((0, 0));
    }
    let sign: i64 = if bytes[0] == b'+' {
        1
    } else if bytes[0] == b'-' {
        -1
    } else {
        0
    };
    if sign == 0 {
        return Ok((0, 0));
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 {
        return Ok((0, 0));
    }
    let digits = &spec[1..i];
    let mag: i64 = digits.parse().context("invalid -L offset")?;
    Ok((sign * mag, i))
}

/// Git `strtol(..., 10)` for line numbers (allows a leading `-`, e.g. `-L-1`).
fn parse_decimal_line_number(spec: &str) -> Result<(i64, usize)> {
    let bytes = spec.as_bytes();
    let mut i = 0usize;
    if bytes.first() == Some(&b'+') {
        bail!("not a plain decimal line number");
    }
    if bytes.first() == Some(&b'-') {
        i = 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        bail!("not a line number");
    }
    let n: i64 = spec[..i].parse().context("line number")?;
    Ok((n, i))
}

fn find_slash_delimited_regex_end(spec: &str) -> Option<usize> {
    let bytes = spec.as_bytes();
    if bytes.first() != Some(&b'/') {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            return Some(i);
        }
        if bytes[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    None
}

fn parse_range_funcname_blame(
    arg: &str,
    lines: &[String],
    anchor: i64,
    ctx: &mut LineRangeParseCtx<'_>,
) -> Result<(usize, usize)> {
    let lines_count = lines.len();
    let mut s = arg;
    let mut anchor_1based = anchor;
    if s.starts_with('^') {
        anchor_1based = 1;
        s = &s[1..];
    }
    if !s.starts_with(':') {
        bail!("internal: expected :funcname range");
    }

    let mut term = 1usize;
    let bytes = s.as_bytes();
    while term < bytes.len() && bytes[term] != b':' {
        if bytes[term] == b'\\' && term + 1 < bytes.len() {
            term += 2;
        } else {
            term += 1;
        }
    }
    if term <= 1 {
        bail!("invalid -L :funcname pattern");
    }
    let pattern = &s[1..term];
    let rest = &s[term..];

    let mut idx0 = (anchor_1based - 1).max(0) as usize;
    if idx0 > lines_count {
        idx0 = lines_count;
    }

    let re = compile_blame_line_regex(pattern)?;
    let matcher = funcname_matcher_for_blame(ctx);
    let mut start_line = None;
    for (li, line) in lines.iter().enumerate().skip(idx0) {
        let hay = line.as_str();
        for m in re.find_iter(hay) {
            let bol = hay.as_bytes().get(m.start()).copied();
            let is_fn = matcher
                .as_ref()
                .map(|m2| m2.match_line(hay).is_some())
                .unwrap_or_else(|| {
                    bol.is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
                });
            if is_fn {
                start_line = Some(li + 1);
                break;
            }
        }
        if start_line.is_some() {
            break;
        }
    }

    let Some(begin) = start_line else {
        bail!(
            "-L parameter ':{pattern}' starting at line {}: no match",
            anchor_1based.max(1)
        );
    };

    if begin > lines_count {
        bail!("-L parameter ':{pattern}' matches at EOF");
    }

    let mut end_line = begin + 1;
    while end_line <= lines_count {
        let bol = lines[end_line - 1].as_str();
        let is_boundary = matcher
            .as_ref()
            .map(|m2| m2.match_line(bol).is_some())
            .unwrap_or_else(|| {
                let b = bol.as_bytes().first().copied();
                b.is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
            });
        if is_boundary {
            break;
        }
        end_line += 1;
    }
    end_line -= 1;

    if !rest.is_empty() {
        bail!("invalid -L :funcname: trailing {rest:?}");
    }

    Ok((begin, end_line))
}

fn compile_blame_line_regex(pattern: &str) -> Result<Regex> {
    // Git `line-range.c` uses POSIX `regcomp(..., REG_NEWLINE)` so `^`/`$` match line boundaries.
    let anchored = if pattern.starts_with("(?m)") || pattern.starts_with("(?M)") {
        pattern.to_string()
    } else {
        format!("(?m){pattern}")
    };
    Regex::new(&anchored).with_context(|| format!("invalid regex in -L: {pattern}"))
}

fn funcname_matcher_for_blame(ctx: &LineRangeParseCtx<'_>) -> Option<userdiff::FuncnameMatcher> {
    ctx.textconv
        .and_then(|tc| userdiff::matcher_for_path(&tc.config, &tc.attrs, ctx.file_path).ok())
        .flatten()
}

#[expect(clippy::too_many_arguments)]
fn write_porcelain(
    out: &mut impl Write,
    lines: &[BlameLine],
    commits: &HashMap<ObjectId, CommitData>,
    filename: &str,
    line_porcelain: bool,
    incremental: bool,
    meta_enc: &BlameMetaEncoding,
    mailmap: &grit_lib::mailmap::MailmapTable,
    mark_unblamable: bool,
    mark_ignored: bool,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();

    // Pre-compute group counts: for each position, how many consecutive lines
    // share the same oid starting from the first occurrence in the group.
    let mut group_counts: Vec<Option<usize>> = vec![None; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let oid = lines[i].oid;
        let start = i;
        while i < lines.len() && lines[i].oid == oid {
            i += 1;
        }
        group_counts[start] = Some(i - start);
    }

    let mut order: Vec<usize> = (0..lines.len()).collect();
    if incremental {
        order.sort_by_key(|&idx| std::cmp::Reverse(lines[idx].final_lineno));
    }

    for &idx in &order {
        let bl = &lines[idx];
        let hex = bl.oid.to_hex();
        let source_name = bl.source_file.as_deref().unwrap_or(filename).to_string();
        let first = seen.insert((bl.oid, source_name.clone()));

        // Header line: hash orig_lineno final_lineno [group_count]
        if let Some(count) = group_counts[idx] {
            writeln!(out, "{hex} {} {} {count}", bl.orig_lineno, bl.final_lineno)?;
        } else {
            writeln!(out, "{hex} {} {}", bl.orig_lineno, bl.final_lineno)?;
        }

        if first || line_porcelain {
            let commit = &commits[&bl.oid];
            let author = parse_author_field(&commit.author);
            let committer = parse_author_field(&commit.committer);

            out.write_all(b"author ")?;
            out.write_all(&meta_enc.author_name_bytes_mailmapped(commit, mailmap)?)?;
            out.write_all(b"\n")?;
            out.write_all(b"author-mail ")?;
            out.write_all(&meta_enc.author_mail_bytes_mailmapped(commit, mailmap)?)?;
            out.write_all(b"\n")?;
            writeln!(out, "author-time {}", author.timestamp)?;
            writeln!(out, "author-tz {}", author.tz)?;
            out.write_all(b"committer ")?;
            out.write_all(&meta_enc.committer_name_bytes(commit)?)?;
            out.write_all(b"\n")?;
            out.write_all(b"committer-mail ")?;
            out.write_all(&meta_enc.committer_mail_bytes(commit)?)?;
            out.write_all(b"\n")?;
            writeln!(out, "committer-time {}", committer.timestamp)?;
            writeln!(out, "committer-tz {}", committer.tz)?;
            out.write_all(b"summary ")?;
            out.write_all(&meta_enc.summary_bytes(commit)?)?;
            out.write_all(b"\n")?;
            // Previous commit (parent) if not a root commit
            if !commit.parents.is_empty() {
                let parent_hex = commit.parents[0].to_hex();
                writeln!(out, "previous {parent_hex} {source_name}")?;
            }
            // Boundary: root commit has no parents
            if commit.parents.is_empty() {
                writeln!(out, "boundary")?;
            }
            writeln!(out, "filename {source_name}")?;
        }

        if mark_ignored && bl.ignored {
            writeln!(out, "ignored")?;
        }
        if mark_unblamable && bl.unblamable {
            writeln!(out, "unblamable")?;
        }
        writeln!(out, "\t{}", bl.content)?;
    }

    Ok(())
}

/// `git annotate` output: tab-separated fields, 8-digit hash, parenthetical block padded like git.
fn write_annotate(
    out: &mut impl Write,
    lines: &[BlameLine],
    commits: &HashMap<ObjectId, CommitData>,
    args: &Args,
    mailmap: &grit_lib::mailmap::MailmapTable,
    _file_path: &str,
) -> Result<()> {
    let zero = grit_lib::diff::zero_oid();

    let mut author_field_width: usize = 10;
    for bl in lines {
        let w = annotate_author_field_width(bl, commits, args, mailmap);
        author_field_width = author_field_width.max(w);
    }

    for bl in lines {
        let hash = if bl.oid == zero || bl.external_contents {
            "00000000".to_string()
        } else {
            bl.oid.to_hex()[..8].to_string()
        };

        let (author_display, ts) = annotate_author_and_time(bl, commits, args, mailmap);
        let author_padded = format!("{author_display:>author_field_width$}");

        writeln!(
            out,
            "{hash}\t({author_padded}\t{ts}\t{lineno}){content}",
            lineno = bl.final_lineno,
            content = bl.content,
        )?;
    }
    Ok(())
}

fn annotate_author_field_width(
    bl: &BlameLine,
    commits: &HashMap<ObjectId, CommitData>,
    args: &Args,
    mailmap: &grit_lib::mailmap::MailmapTable,
) -> usize {
    let (name, _ts) = annotate_author_and_time(bl, commits, args, mailmap);
    name.chars().count().max(1)
}

fn annotate_author_and_time(
    bl: &BlameLine,
    commits: &HashMap<ObjectId, CommitData>,
    args: &Args,
    mailmap: &grit_lib::mailmap::MailmapTable,
) -> (String, String) {
    let zero = grit_lib::diff::zero_oid();
    if bl.external_contents {
        return (
            "External file (--contents)".to_string(),
            format_time(0, "+0000"),
        );
    }
    if bl.oid == zero {
        return ("Not Committed Yet".to_string(), format_time(0, "+0000"));
    }
    let commit = &commits[&bl.oid];
    let ai = parse_author_field(&commit.author);
    let (n, e) = if !mailmap.is_empty() {
        mailmap.map_user(ai.name, ai.email)
    } else {
        (ai.name, ai.email)
    };
    let who = if args.email { format!("<{e}>") } else { n };
    let ts = format_time(ai.timestamp, &ai.tz);
    (who, ts)
}

const RESET: &str = "\x1b[0m";

/// Pick the ANSI color for a commit's author timestamp (Git `determine_line_heat`).
fn age_color_for_timestamp(author_time: i64, buckets: &[(i64, String)]) -> &str {
    let mut i = 0usize;
    while i < buckets.len() && author_time > buckets[i].0 {
        i += 1;
    }
    buckets.get(i).map(|(_, c)| c.as_str()).unwrap_or("")
}

#[expect(clippy::too_many_arguments)]
fn write_default(
    out: &mut impl Write,
    lines: &[BlameLine],
    commits: &HashMap<ObjectId, CommitData>,
    args: &Args,
    mailmap: &grit_lib::mailmap::MailmapTable,
    color_style: &BlameColorStyle,
    file_path: &str,
    mark_unblamable: bool,
    mark_ignored: bool,
) -> Result<()> {
    let hash_len = if args.no_abbrev || args.long_hash {
        40
    } else if let Some(n) = args.abbrev {
        n.saturating_add(1).clamp(4, 40)
    } else {
        8
    };
    let max_lineno = lines.iter().map(|b| b.final_lineno).max().unwrap_or(1);
    let lineno_width = format!("{max_lineno}").len();

    let mut prev_oid: Option<ObjectId> = None;

    // Check if any blame line is a boundary (root commit or explicit ^REV start).
    let has_boundary = args.boundary_revision
        || (!args.root
            && lines.iter().any(|l| {
                commits
                    .get(&l.oid)
                    .map(|c| c.parents.is_empty())
                    .unwrap_or(false)
            }));

    for bl in lines {
        let hex = bl.oid.to_hex();
        let is_boundary = args.boundary_revision
            || (!args.root
                && commits
                    .get(&bl.oid)
                    .map(|c| c.parents.is_empty())
                    .unwrap_or(false));
        let short = if has_boundary {
            if is_boundary {
                if args.blank_boundary {
                    " ".repeat(hash_len)
                } else {
                    let boundary_hex_len = match args.abbrev {
                        Some(n) if n > 40 => hash_len,
                        _ => hash_len.saturating_sub(1),
                    };
                    format!("^{}", &hex[..boundary_hex_len.min(hex.len())])
                }
            } else {
                // Extra char width to align with ^ prefix lines
                hex[..hash_len.min(hex.len())].to_string()
            }
        } else {
            hex[..hash_len.min(hex.len())].to_string()
        };

        // Determine color prefix/suffix
        let (color_start, color_end) = if args.color_by_age {
            let commit = &commits[&bl.oid];
            let ai = parse_author_field(&commit.author);
            let c = age_color_for_timestamp(ai.timestamp, &color_style.age_buckets);
            (c, RESET)
        } else if args.color_lines && prev_oid == Some(bl.oid) {
            (color_style.repeated_lines_ansi.as_str(), RESET)
        } else {
            ("", "")
        };

        // Filename field for -f / --show-name
        let fname = if args.show_name {
            let name = bl.source_file.as_deref().unwrap_or(file_path);
            format!("{name} ")
        } else {
            String::new()
        };
        let marker = if mark_unblamable && bl.unblamable {
            "*"
        } else if mark_ignored && bl.ignored {
            "?"
        } else {
            ""
        };

        if args.suppress {
            writeln!(
                out,
                "{color_start}{marker}{short} {fname}{lineno:>w$}) {content}{color_end}",
                lineno = bl.final_lineno,
                w = lineno_width,
                content = bl.content,
            )?;
        } else {
            let commit = &commits[&bl.oid];
            let ai = parse_author_field(&commit.author);
            let (n, e) = if !mailmap.is_empty() {
                mailmap.map_user(ai.name, ai.email)
            } else {
                (ai.name, ai.email)
            };
            let who = if args.email { format!("<{e}>") } else { n };
            let ts = format_time(ai.timestamp, &ai.tz);

            writeln!(
                out,
                "{color_start}{marker}{short} {fname}({who} {ts} {lineno:>w$}) {content}{color_end}",
                lineno = bl.final_lineno,
                w = lineno_width,
                content = bl.content,
            )?;
        }

        prev_oid = Some(bl.oid);
    }

    Ok(())
}
