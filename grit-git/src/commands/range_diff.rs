//! `grit range-diff` — compare two commit ranges (Git-compatible).
//!
//! Mirrors Git's `range-diff.c`: `grit log -p` output is normalized into patches,
//! matched with a Hungarian assignment, then printed in RHS order with optional
//! inner diffs.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use crate::commands::upstream_synopsis_help;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{
    abbreviate_object_id, resolve_revision, split_double_dot_range, split_triple_dot_range,
};
use hungarian::minimize;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const COST_MAX: u64 = 1 << 16;
const DEFAULT_CREATION_FACTOR: i32 = 60;

/// Arguments for `grit range-diff`.
#[derive(Debug, Parser)]
#[command(
    name = "grit range-diff",
    about = "Compare two commit ranges",
    disable_help_subcommand = true
)]
pub struct Args {
    #[arg(long = "creation-factor", value_name = "N")]
    pub creation_factor: Option<i32>,

    #[arg(long = "no-dual-color")]
    pub no_dual_color: bool,

    /// Explicit `--dual-color` (Git forces color output on when given).
    #[arg(long = "dual-color")]
    pub dual_color: bool,

    #[arg(long = "left-only")]
    pub left_only: bool,

    #[arg(long = "right-only")]
    pub right_only: bool,

    #[arg(short = 's', long = "no-patch")]
    pub no_patch: bool,

    #[arg(long = "stat")]
    pub stat: bool,

    /// `--submodule[=<format>]` diff option (accepted for compatibility; the inner
    /// patch text already renders submodule changes as ordinary file sections).
    #[arg(long = "submodule", value_name = "FORMAT", num_args = 0..=1, default_missing_value = "log", require_equals = true)]
    pub submodule: Option<String>,

    #[arg(long = "color", default_missing_value = "always", num_args = 0..=1, require_equals = true)]
    pub color: Option<String>,

    #[arg(long = "no-color")]
    pub no_color: bool,

    #[arg(long = "abbrev", value_name = "N", default_missing_value = "7", num_args = 0..=1, require_equals = true)]
    pub abbrev: Option<String>,

    #[arg(long = "no-notes")]
    pub no_notes: bool,

    #[arg(
        long = "notes",
        value_name = "REF",
        action = clap::ArgAction::Append,
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub notes: Vec<String>,

    #[arg(long = "diff-merges", value_name = "STYLE", default_missing_value = "on", num_args = 0..=1, require_equals = true)]
    pub diff_merges: Option<String>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

struct Patch {
    oid: ObjectId,
    full: String,
    diff_offset: usize,
    diffsize: i32,
    matching: i32,
    shown: bool,
}

/// Parse argv after `range-diff` and run (used from `main` so `--` / pathspecs work).
pub fn run_with_rest(rest: &[String]) -> Result<()> {
    upstream_synopsis_help::try_print_upstream_help_and_exit("range-diff", rest);
    // Git's `parse_options` accepts options interspersed with the commit-range
    // operands (e.g. `range-diff A...B --color --dual-color`). Clap's
    // `trailing_var_arg` would swallow any option after the first operand, so
    // move options ahead of operands (preserving everything after a literal `--`).
    let argv = reorder_options_first(rest);
    let args = Args::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("{e}"))?;
    run(args)
}

/// Reorder `range-diff` argv so options precede operands (stopping at `--`),
/// returning a fresh argv with the program name prepended.
fn reorder_options_first(rest: &[String]) -> Vec<String> {
    let mut opts: Vec<String> = Vec::new();
    let mut operands: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut seen_dashdash = false;
    while i < rest.len() {
        let tok = &rest[i];
        if seen_dashdash {
            tail.push(tok.clone());
            i += 1;
            continue;
        }
        if tok == "--" {
            seen_dashdash = true;
            tail.push(tok.clone());
            i += 1;
            continue;
        }
        if tok.starts_with('-') && tok.len() > 1 {
            opts.push(tok.clone());
            // `--creation-factor N` / `-s` etc.: only `--creation-factor` (and its
            // alias forms) consumes a following separate value token.
            if (tok == "--creation-factor") && i + 1 < rest.len() {
                opts.push(rest[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        operands.push(tok.clone());
        i += 1;
    }
    let mut argv = vec!["grit range-diff".to_string()];
    argv.extend(opts);
    argv.extend(operands);
    argv.extend(tail);
    argv
}

fn run(args: Args) -> Result<()> {
    if args.left_only && args.right_only {
        bail!("options '--left-only' and '--right-only' cannot be used together");
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let (rev1, rev2, log_extra) = parse_invocation(&repo, &args.rest)?;

    let creation = args
        .creation_factor
        .unwrap_or(DEFAULT_CREATION_FACTOR)
        .max(0) as u64;

    let mut branch1 = read_patches_from_log(&repo, &rev1, &args, &log_extra)?;
    let mut branch2 = read_patches_from_log(&repo, &rev2, &args, &log_extra)?;

    find_exact_matches(&mut branch1, &mut branch2);
    get_correspondences(&mut branch1, &mut branch2, creation);
    match_adjacent_unmatched_prefix_subjects(&repo, &mut branch1, &mut branch2);

    // Git uses one `--[no-]dual-color` toggle (`simple_color`, default -1). `--dual-color`
    // sets it to 0 and forces color on; `--no-dual-color` sets it to 1 (simple colors).
    let dual_color = !args.no_dual_color;
    let force_color = args.dual_color && !args.no_dual_color;

    let use_color = !args.no_color
        && (force_color
            || args
                .color
                .as_deref()
                .map(|c| c == "always" || c.is_empty())
                .unwrap_or_else(|| {
                    (std::io::stdout().is_terminal() && grit_lib::terminal::ansi_supported())
                        || std::env::var_os("GIT_PAGER_IN_USE").is_some()
                }));
    let use_color = use_color
        && !matches!(
            args.color.as_deref(),
            Some("never") | Some("false") | Some("no")
        );

    let abbrev_len = args
        .abbrev
        .as_deref()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(7)
        .clamp(4, 40);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    output(
        &mut out,
        &repo,
        &mut branch1,
        &mut branch2,
        args.left_only,
        args.right_only,
        args.no_patch,
        args.stat,
        use_color,
        dual_color,
        abbrev_len,
    )?;

    Ok(())
}

/// Compute a `range-diff` body for `format-patch --range-diff`, comparing the previous
/// range (`old_range`, e.g. `boop~2..boop~1`) against the current series range
/// (`new_range`, e.g. `<series base>..<series tip>`), and return it as a plain (no-color)
/// string. Equivalent to `git range-diff [--creation-factor=N] <old_range> <new_range>`.
pub fn compute_range_diff_body(
    repo: &Repository,
    old_range: &str,
    new_range: &str,
    creation_factor: Option<usize>,
) -> Result<String> {
    compute_range_diff_body_with_notes(repo, old_range, new_range, creation_factor, true, &[])
}

/// Compute a range-diff body with explicit note-display controls.
///
/// `notes_refs` accepts full notes ref names. When `no_notes` is true, notes are suppressed even
/// if refs are supplied.
pub fn compute_range_diff_body_with_notes(
    repo: &Repository,
    old_range: &str,
    new_range: &str,
    creation_factor: Option<usize>,
    no_notes: bool,
    notes_refs: &[String],
) -> Result<String> {
    let args = Args::parse_from(["grit range-diff"]);
    let args = Args {
        no_notes,
        notes: notes_refs.to_vec(),
        ..args
    };
    let creation = creation_factor
        .map(|c| c as u64)
        .unwrap_or(DEFAULT_CREATION_FACTOR as u64);

    let rev1 = log_range_vec(old_range);
    let rev2 = log_range_vec(new_range);
    let log_extra: Vec<String> = Vec::new();

    let mut branch1 = read_patches_from_log(repo, &rev1, &args, &log_extra)?;
    let mut branch2 = read_patches_from_log(repo, &rev2, &args, &log_extra)?;

    find_exact_matches(&mut branch1, &mut branch2);
    get_correspondences(&mut branch1, &mut branch2, creation);
    match_adjacent_unmatched_prefix_subjects(repo, &mut branch1, &mut branch2);

    let mut buf: Vec<u8> = Vec::new();
    output(
        &mut buf,
        repo,
        &mut branch1,
        &mut branch2,
        false,
        false,
        false,
        false,
        false,
        false,
        7,
    )?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_invocation(
    repo: &Repository,
    rest: &[String],
) -> Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let (rpart, pspecs) = if let Some(i) = rest.iter().position(|s| s == "--") {
        (&rest[..i], &rest[i + 1..])
    } else {
        (rest, &[][..])
    };

    let mut log_extra: Vec<String> = Vec::new();
    if !pspecs.is_empty() {
        log_extra.push("--".to_string());
        log_extra.extend(pspecs.iter().cloned());
    }

    let argc = rpart.len();

    if argc >= 3 && all_committish(repo, &rpart[..3]) {
        for s in &rpart[..3] {
            resolve_revision(repo, s).map_err(|_| anyhow::anyhow!("not a revision: '{s}'"))?;
        }
        let s1 = log_range_vec(&format!("{}..{}", rpart[0], rpart[1]));
        let s2 = log_range_vec(&format!("{}..{}", rpart[0], rpart[2]));
        return Ok((s1, s2, log_extra));
    }

    if argc >= 2 && is_range(repo, &rpart[0]) && is_range(repo, &rpart[1]) {
        return Ok((
            log_range_vec(&rpart[0]),
            log_range_vec(&rpart[1]),
            log_extra,
        ));
    }

    if argc == 2 {
        let a = expand_parent_only_syntax(repo, &rpart[0])?;
        let b = expand_parent_only_syntax(repo, &rpart[1])?;
        if let (Some(va), Some(vb)) = (a, b) {
            return Ok((va, vb, log_extra));
        }
        if rpart[0].contains("..") || rpart[1].contains("..") {
            bail!("not a commit range");
        }
    }

    if argc >= 1 {
        let arg0 = rpart[0].as_str();
        if let Some((a, b)) = split_triple_dot_range(arg0) {
            if !a.is_empty() || !b.is_empty() {
                let a_oid = resolve_revision(repo, if a.is_empty() { "HEAD" } else { a })?;
                let b_oid = resolve_revision(repo, if b.is_empty() { "HEAD" } else { b })?;
                // Match `git range-diff` / `builtin/range-diff.c`: `B..A` then `A..B`.
                let range1 = format!("{}..{}", b_oid.to_hex(), a_oid.to_hex());
                let range2 = format!("{}..{}", a_oid.to_hex(), b_oid.to_hex());
                return Ok((log_range_vec(&range1), log_range_vec(&range2), log_extra));
            }
        }
    }

    bail!("need two commit ranges");
}

/// `topic^!` → `^p1 ^p2 … tip`; `topic^-N` → `^pN tip` (Git parent-only revision syntax).
fn expand_parent_only_syntax(repo: &Repository, spec: &str) -> Result<Option<Vec<String>>> {
    if let Some(base) = spec.strip_suffix("^!") {
        if base.is_empty() {
            return Ok(None);
        }
        let oid = resolve_revision(repo, base)?;
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let mut out: Vec<String> = commit
            .parents
            .iter()
            .map(|p| format!("^{}", p.to_hex()))
            .collect();
        out.push(oid.to_hex());
        return Ok(Some(out));
    }
    if let Some(pos) = spec.rfind("^-") {
        let base = &spec[..pos];
        let rest = &spec[pos + 2..];
        if base.is_empty() {
            return Ok(None);
        }
        let n: usize = if rest.is_empty() {
            1
        } else {
            rest.parse()
                .map_err(|_| anyhow::anyhow!("invalid parent spec"))?
        };
        if n < 1 {
            return Ok(None);
        }
        let oid = resolve_revision(repo, base)?;
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let parent = commit
            .parents
            .get(n - 1)
            .ok_or_else(|| anyhow::anyhow!("revision '{spec}' has no parent {n}"))?;
        return Ok(Some(vec![format!("^{}", parent.to_hex()), oid.to_hex()]));
    }
    Ok(None)
}

fn log_range_vec(range: &str) -> Vec<String> {
    log_range_args(range)
}

fn all_committish(repo: &Repository, specs: &[String]) -> bool {
    specs.iter().all(|s| resolve_revision(repo, s).is_ok())
}

fn is_range(repo: &Repository, spec: &str) -> bool {
    if split_triple_dot_range(spec).is_some() {
        return true;
    }
    let Some((left, right)) = split_double_dot_range(spec) else {
        return false;
    };
    if left.is_empty() || right.is_empty() {
        return false;
    }
    resolve_revision(repo, left).is_ok() && resolve_revision(repo, right).is_ok()
}

fn read_patches_from_log(
    repo: &Repository,
    rev_args: &[String],
    args: &Args,
    log_extra: &[String],
) -> Result<Vec<Patch>> {
    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("-C")
        .arg(repo_work_dir(repo))
        .arg("-c")
        .arg("diff.submodule=short")
        .arg("log")
        .arg("--no-color")
        .arg("--no-abbrev")
        .arg("--reverse")
        .arg("--decorate=no")
        .arg("--find-renames")
        .arg("--no-prefix")
        .arg("--output-indicator-new=>")
        .arg("--output-indicator-old=<")
        .arg("--output-indicator-context=#")
        .arg("--pretty=medium")
        .arg("-p");
    if args.diff_merges.is_none() {
        cmd.arg("--no-merges");
    }
    if args.no_notes {
        cmd.arg("--no-notes");
    }
    for n in &args.notes {
        if n.is_empty() {
            cmd.arg("--notes");
        } else {
            cmd.arg(format!("--notes={n}"));
        }
    }
    if let Some(dm) = &args.diff_merges {
        cmd.arg(format!("--diff-merges={dm}"));
    }
    for arg in rev_args {
        cmd.arg(arg);
    }
    for e in log_extra {
        cmd.arg(e);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().context("spawn grit log")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("log failed: {stderr}");
    }
    parse_log_into_patches(&String::from_utf8_lossy(&out.stdout))
}

fn log_range_args(range: &str) -> Vec<String> {
    if let Some((left, right)) = split_double_dot_range(range) {
        if !left.is_empty() && !right.is_empty() && split_triple_dot_range(range).is_none() {
            return vec![format!("^{left}"), right.to_string()];
        }
    }
    vec![range.to_string()]
}

fn repo_work_dir(repo: &Repository) -> &Path {
    repo.work_tree.as_deref().unwrap_or(repo.git_dir.as_path())
}

fn parse_log_into_patches(contents: &str) -> Result<Vec<Patch>> {
    let mut list: Vec<Patch> = Vec::new();
    let mut buf = String::new();
    let mut current: Option<Patch> = None;
    let mut in_header = true;
    let mut current_filename: Option<String> = None;
    let mut pending: Option<DiffHeader> = None;

    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("commit ") {
            // Flush a trailing hunk-less diff header from the previous commit. The
            // file name is irrelevant here since the new commit resets it below.
            if let Some(h) = pending.take() {
                let summary = h.summary();
                if let Some(p) = current.as_mut() {
                    p.diffsize += summary.lines().count() as i32;
                }
                buf.push_str(&summary);
            }
            if let Some(p) = current.take() {
                list.push(finish_patch(p, &buf)?);
                buf.clear();
            }
            let oid_str = rest
                .split(" (from ")
                .next()
                .unwrap_or(rest)
                .split_whitespace()
                .next()
                .unwrap_or(rest)
                .trim();
            let oid: ObjectId = oid_str
                .parse()
                .map_err(|_| anyhow::anyhow!("could not parse commit '{oid_str}'"))?;
            current = Some(Patch {
                oid,
                full: String::new(),
                diff_offset: 0,
                diffsize: 0,
                matching: -1,
                shown: false,
            });
            in_header = true;
            current_filename = None;
            continue;
        }

        let util = current.as_mut().ok_or_else(|| {
            anyhow::anyhow!("could not parse log output (expected commit line first)")
        })?;

        if line.starts_with("diff --git ") {
            // Flush any previous (hunk-less, e.g. pure mode change) diff header first.
            if let Some(h) = pending.take() {
                let summary = h.summary();
                util.diffsize += summary.lines().count() as i32;
                buf.push_str(&summary);
                current_filename = Some(h.context_filename());
            }
            in_header = false;
            buf.push('\n');
            if util.diff_offset == 0 {
                util.diff_offset = buf.len();
            }
            pending = Some(DiffHeader::from_diff_git_line(line));
            continue;
        }

        // While accumulating a `diff --git` header, capture mode/new/delete/rename
        // metadata and consume the header lines until the first hunk or content.
        if let Some(h) = pending.as_mut() {
            if line.starts_with("old mode ") {
                h.old_mode = line["old mode ".len()..].trim().to_string();
                continue;
            }
            if line.starts_with("new mode ") {
                h.new_mode = line["new mode ".len()..].trim().to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("new file mode ") {
                h.is_new = true;
                h.new_mode = rest.trim().to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("deleted file mode ") {
                h.is_delete = true;
                h.old_mode = rest.trim().to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("rename from ") {
                h.is_rename = true;
                h.old_name = rest.to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("rename to ") {
                h.is_rename = true;
                h.new_name = rest.to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("copy from ") {
                h.old_name = rest.to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("copy to ") {
                h.new_name = rest.to_string();
                continue;
            }
            if line.starts_with("similarity index ")
                || line.starts_with("dissimilarity index ")
                || line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("Binary files ")
                || line.starts_with("GIT binary patch")
            {
                continue;
            }
            // First real content line (a hunk or otherwise): flush the summary now.
            let summary = h.summary();
            util.diffsize += summary.lines().count() as i32;
            buf.push_str(&summary);
            current_filename = Some(h.context_filename());
            pending = None;
        }

        if in_header {
            if line.starts_with("Author: ") {
                buf.push_str(" ## Metadata ##\n");
                buf.push_str(line);
                buf.push_str("\n\n");
                buf.push_str(" ## Commit message ##\n");
            } else if line.starts_with("Notes") && line.ends_with(':') {
                buf.push_str("\n\n");
                let name = line.trim_end_matches(':');
                buf.push_str(&format!(" ## {name} ##\n"));
            } else if line.starts_with("    ") {
                // Keep the full message line (including its 4-space indent), like
                // Git's range-diff; only trailing whitespace is stripped.
                let trimmed = line.trim_end_matches(char::is_whitespace);
                buf.push_str(trimmed);
                buf.push('\n');
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("@@ ") {
            let mut h = String::from("@@");
            if let Some(pos) = rest.find("@@") {
                let after = &rest[pos + 2..];
                if let Some(ref fname) = current_filename {
                    if !after.is_empty() {
                        h.push_str(&format!(" {fname}:"));
                    }
                }
                h.push_str(after);
            }
            buf.push_str(&h);
            buf.push('\n');
            util.diffsize += 1;
            continue;
        }

        if line.is_empty() {
            continue;
        }

        let first = line.as_bytes().first().copied();
        if first == Some(b'>') {
            buf.push('+');
            buf.push_str(&line[1..]);
        } else if first == Some(b'<') {
            buf.push('-');
            buf.push_str(&line[1..]);
        } else if first == Some(b'#') {
            buf.push(' ');
            buf.push_str(&line[1..]);
        } else {
            buf.push(' ');
            buf.push_str(line);
        }
        buf.push('\n');
        util.diffsize += 1;
    }

    if let Some(h) = pending.take() {
        let summary = h.summary();
        if let Some(p) = current.as_mut() {
            p.diffsize += summary.lines().count() as i32;
        }
        buf.push_str(&summary);
    }

    if let Some(p) = current {
        list.push(finish_patch(p, &buf)?);
    }

    Ok(list)
}

fn finish_patch(mut p: Patch, buf: &str) -> Result<Patch> {
    p.full = buf.to_string();
    if p.diff_offset > p.full.len() {
        p.diff_offset = p.full.len();
    }
    Ok(p)
}

/// Accumulated state for one `diff --git` section, used to render the range-diff
/// `## ... ##` file summary line (mirrors Git `range-diff.c`'s use of
/// `parse_git_diff_header`).
struct DiffHeader {
    old_name: String,
    new_name: String,
    is_new: bool,
    is_delete: bool,
    is_rename: bool,
    old_mode: String,
    new_mode: String,
}

impl DiffHeader {
    fn from_diff_git_line(line: &str) -> Self {
        let rest = line.strip_prefix("diff --git ").unwrap_or("");
        let mut parts = rest.split_whitespace();
        let a_raw = parts.next().unwrap_or("");
        let b_raw = parts.next().unwrap_or("");
        let a = a_raw.strip_prefix("a/").unwrap_or(a_raw);
        let b = b_raw.strip_prefix("b/").unwrap_or(b_raw);
        DiffHeader {
            old_name: a.to_string(),
            new_name: b.to_string(),
            is_new: false,
            is_delete: false,
            is_rename: false,
            old_mode: String::new(),
            new_mode: String::new(),
        }
    }

    /// The `## ... ##` summary line (with trailing newline).
    fn summary(&self) -> String {
        let mut s = String::from(" ## ");
        if self.is_new {
            s.push_str(&format!("{} (new)", self.new_name));
        } else if self.is_delete {
            s.push_str(&format!("{} (deleted)", self.old_name));
        } else if self.is_rename {
            s.push_str(&format!("{} => {}", self.old_name, self.new_name));
        } else {
            s.push_str(&self.new_name);
        }
        if !self.old_mode.is_empty()
            && !self.new_mode.is_empty()
            && self.old_mode != self.new_mode
            && !self.is_new
            && !self.is_delete
        {
            s.push_str(&format!(
                " (mode change {} => {})",
                self.old_mode, self.new_mode
            ));
        }
        s.push_str(" ##\n");
        s
    }

    /// File name used for `@@ <file>: <func>` inner-hunk headers.
    fn context_filename(&self) -> String {
        if self.is_delete {
            self.old_name.clone()
        } else {
            self.new_name.clone()
        }
    }
}

fn find_exact_matches(a: &mut [Patch], b: &mut [Patch]) {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in a.iter().enumerate() {
        let d = p.full[p.diff_offset..].to_string();
        map.entry(d).or_default().push(i);
    }
    for (j, p) in b.iter_mut().enumerate() {
        let d = &p.full[p.diff_offset..];
        if let Some(indices) = map.get(d) {
            for &i in indices {
                if a[i].matching < 0 && p.matching < 0 {
                    a[i].matching = j as i32;
                    p.matching = i as i32;
                    break;
                }
            }
        }
    }
}

/// Cost of pairing two patch texts: the number of lines a real unified diff
/// (Myers, 3 context lines) of them would emit, mirroring Git `range-diff.c`'s
/// `diffsize()` (each hunk header + each emitted body line counts once).
fn diffsize_lines(x: &str, y: &str) -> i32 {
    use grit_lib::diff::unified_diff_with_prefix_and_funcname;
    if x == y {
        return 0;
    }
    let raw = unified_diff_with_prefix_and_funcname(
        x, y, "a", "b", 3, 0, "", "", None, /* indent_heuristic */ true,
        /* quote_path_fully */ false,
    );
    let mut count = 0i32;
    for line in raw.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        count += 1;
    }
    count
}

fn get_correspondences(a: &mut [Patch], b: &mut [Patch], creation_factor: u64) {
    let n = a.len() + b.len();
    if n == 0 {
        return;
    }
    let mut cost = vec![0u64; n * n];
    for i in 0..a.len() {
        for j in 0..b.len() {
            let ai = &a[i];
            let bj = &b[j];
            let c = if ai.matching == j as i32 {
                0
            } else if ai.matching < 0 && bj.matching < 0 {
                let da = &ai.full[ai.diff_offset..];
                let db = &bj.full[bj.diff_offset..];
                diffsize_lines(da, db) as u64
            } else {
                COST_MAX
            };
            cost[j + n * i] = c;
        }
        let ai = &a[i];
        let c_pad = if ai.matching < 0 {
            (ai.diffsize as u64).saturating_mul(creation_factor) / 100
        } else {
            COST_MAX
        };
        for j in b.len()..n {
            cost[j + n * i] = c_pad;
        }
    }
    for j in 0..b.len() {
        let bj = &b[j];
        let c_pad = if bj.matching < 0 {
            (bj.diffsize as u64).saturating_mul(creation_factor) / 100
        } else {
            COST_MAX
        };
        for i in a.len()..n {
            cost[j + n * i] = c_pad;
        }
    }
    for i in a.len()..n {
        for j in b.len()..n {
            cost[j + n * i] = 0;
        }
    }

    let assign = minimize(&cost, n, n);
    for i in 0..a.len() {
        if let Some(Some(j)) = assign.get(i) {
            if *j < b.len() {
                a[i].matching = *j as i32;
                b[*j].matching = i as i32;
            }
        }
    }
}

fn commit_subject_for_match(repo: &Repository, oid: ObjectId) -> Option<String> {
    let obj = repo.odb.read(&oid).ok()?;
    if obj.kind != grit_lib::objects::ObjectKind::Commit {
        return None;
    }
    let commit = parse_commit(&obj.data).ok()?;
    commit
        .message
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn strict_subject_prefix_match(a: &str, b: &str) -> bool {
    (a.len() >= 5 && b.starts_with(a) && b.len() > a.len())
        || (b.len() >= 5 && a.starts_with(b) && a.len() > b.len())
}

fn match_adjacent_unmatched_prefix_subjects(repo: &Repository, a: &mut [Patch], b: &mut [Patch]) {
    let limit = a.len().min(b.len());
    for idx in 0..limit {
        if a[idx].matching >= 0 || b[idx].matching >= 0 {
            continue;
        }
        let Some(a_subject) = commit_subject_for_match(repo, a[idx].oid) else {
            continue;
        };
        let Some(b_subject) = commit_subject_for_match(repo, b[idx].oid) else {
            continue;
        };
        if strict_subject_prefix_match(&a_subject, &b_subject) {
            a[idx].matching = idx as i32;
            b[idx].matching = idx as i32;
        }
    }
}

fn output(
    out: &mut impl Write,
    repo: &Repository,
    a: &mut [Patch],
    b: &mut [Patch],
    left_only: bool,
    right_only: bool,
    no_patch: bool,
    stat_mode: bool,
    use_color: bool,
    dual_color: bool,
    abbrev_len: usize,
) -> Result<()> {
    let dual = use_color && dual_color;
    let max_n = 1 + a.len().max(b.len());
    let w = decimal_width(max_n);
    let reset = if use_color { "\x1b[m" } else { "" };
    let color_old = if use_color { "\x1b[31m" } else { "" };
    let color_new = if use_color { "\x1b[32m" } else { "" };
    let color_commit = if use_color { "\x1b[33m" } else { "" };

    let dash_str: String = std::iter::repeat_n('-', abbrev_len).collect();

    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.len() || j < b.len() {
        while i < a.len() && a[i].shown {
            i += 1;
        }
        if i < a.len() && a[i].matching < 0 {
            if !right_only {
                write_pair_header(
                    &mut *out,
                    repo,
                    w,
                    Some((i, &a[i])),
                    None,
                    abbrev_len,
                    &dash_str,
                    reset,
                    color_old,
                    color_new,
                    color_commit,
                )?;
            }
            i += 1;
            continue;
        }
        while j < b.len() && b[j].matching < 0 {
            if !left_only {
                write_pair_header(
                    &mut *out,
                    repo,
                    w,
                    None,
                    Some((j, &b[j])),
                    abbrev_len,
                    &dash_str,
                    reset,
                    color_old,
                    color_new,
                    color_commit,
                )?;
                if !no_patch {
                    write_unmatched_new_notes(&mut *out, &b[j])?;
                }
            }
            j += 1;
        }
        if j < b.len() {
            let bj = &b[j];
            let ai_idx = bj.matching as usize;
            let ai = &a[ai_idx];
            write_pair_header(
                &mut *out,
                repo,
                w,
                Some((ai_idx, ai)),
                Some((j, bj)),
                abbrev_len,
                &dash_str,
                reset,
                color_old,
                color_new,
                color_commit,
            )?;
            if !no_patch && ai.full != bj.full {
                let inner = diff_patches(&ai.full, &bj.full);
                if stat_mode {
                    write_stat_summary(&mut *out, &inner)?;
                } else if dual {
                    for line in inner.lines() {
                        writeln!(out, "    {}", render_inner_dual_color(line))?;
                    }
                } else {
                    for line in inner.lines() {
                        writeln!(out, "    {line}")?;
                    }
                }
            }
            a[ai_idx].shown = true;
            j += 1;
        }
    }

    Ok(())
}

fn write_unmatched_new_notes(out: &mut impl Write, patch: &Patch) -> Result<()> {
    let mut in_notes = false;
    for line in patch.full.lines() {
        if line.starts_with(" ## Notes") {
            in_notes = true;
        } else if in_notes && line.starts_with(" ## ") {
            break;
        }
        if in_notes {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

fn write_stat_summary(out: &mut impl Write, diff: &str) -> Result<()> {
    let mut ins = 0usize;
    let mut dels = 0usize;
    let mut files = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            ins += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            dels += 1;
        }
    }
    if diff.contains("diff --git") {
        files = diff.matches("diff --git").count();
    } else if ins + dels > 0 {
        files = 1;
    }
    writeln!(out, "     a => b | {} +-", ins.saturating_add(dels))?;
    writeln!(
        out,
        "     {} file{} changed, {} insertion{}(+), {} deletion{}(-)",
        files,
        if files == 1 { "" } else { "s" },
        ins,
        if ins == 1 { "" } else { "s" },
        dels,
        if dels == 1 { "" } else { "s" },
    )?;
    Ok(())
}

/// Inner diff between two normalized patch texts, formatted like Git `range-diff`:
/// a real unified diff (Myers, 3 context lines) with `--- /+++ ` headers suppressed,
/// hunk-header line counts suppressed (`@@ <funcname>` instead of `@@ -a,b +c,d @@`),
/// and the `range-diff` section-header function-name driver.
fn diff_patches(x: &str, y: &str) -> String {
    use grit_lib::diff::unified_diff_with_prefix_and_funcname;

    // Full unified diff with empty prefixes; we strip the file headers and rebuild
    // each hunk header ourselves (Git suppresses line counts and uses a custom funcname).
    let raw = unified_diff_with_prefix_and_funcname(
        x, y, "a", "b", 3, 0, "", "", None, /* indent_heuristic */ true,
        /* quote_path_fully */ false,
    );

    let old_lines: Vec<&str> = x.lines().collect();
    let mut out = String::new();
    for line in raw.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if line.starts_with("@@ ") || line == "@@" {
            // Parse the old start line from "@@ -<start>[,<count>] +..." and rebuild the
            // header with line counts suppressed plus the section-header funcname.
            let start = parse_hunk_old_start(line);
            out.push_str("@@");
            if let Some(s) = start {
                if let Some(func) = section_funcname(&old_lines, s) {
                    out.push(' ');
                    out.push_str(&func);
                }
            }
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// Git diff color codes (defaults), used for `--dual-color` inner-diff rendering.
const C_RESET: &str = "\x1b[m";
const C_REVERSE: &str = "\x1b[7m";
const C_OLD: &str = "\x1b[31m"; // red
const C_NEW: &str = "\x1b[32m"; // green
const C_FRAG: &str = "\x1b[36m"; // cyan
const C_CONTEXT: &str = ""; // no color
const C_FUNC: &str = ""; // no color
const C_OLD_BOLD: &str = "\x1b[1;31m";
const C_NEW_BOLD: &str = "\x1b[1;32m";
const C_CONTEXT_BOLD: &str = "\x1b[1m";
const C_OLD_DIM: &str = "\x1b[2;31m";
const C_NEW_DIM: &str = "\x1b[2;32m";
const C_CONTEXT_DIM: &str = "\x1b[2m";

/// Render one inner-diff line in Git `--dual-color` style (mirrors `emit_diff_symbol`
/// with `dual_color_diffed_diffs`). `line` carries the outer diff prefix
/// (` `/`+`/`-`) or is a rebuilt hunk header beginning with `@@`.
fn render_inner_dual_color(line: &str) -> String {
    // Rebuilt inner hunk header: "@@" or "@@ <funcname>".
    if let Some(rest) = line.strip_prefix("@@") {
        let mut s = String::new();
        s.push_str(C_REVERSE);
        s.push_str(C_FRAG);
        s.push_str("@@");
        s.push_str(C_RESET);
        // Anything after "@@" is " <funcname>": a context-colored leading blank,
        // then the function name in funcinfo color.
        let trimmed = rest.trim_start_matches(' ');
        let blanks = &rest[..rest.len() - trimmed.len()];
        if !blanks.is_empty() {
            s.push_str(C_CONTEXT);
            s.push_str(blanks);
            s.push_str(C_RESET);
        }
        if !trimmed.is_empty() {
            s.push_str(C_FUNC);
            s.push_str(trimmed);
            s.push_str(C_RESET);
        }
        return s;
    }

    let (outer, content) = line.split_at(line.len().min(1));
    let inner_first = content.as_bytes().first().copied();
    match outer {
        "+" => {
            // set_sign = NEW; content color depends on the inner first char.
            let set = match inner_first {
                Some(b'-') => C_OLD_BOLD,
                Some(b'@') => C_FRAG,
                Some(b'+') => C_NEW_BOLD,
                _ => C_CONTEXT_BOLD,
            };
            emit_line0(C_REVERSE, C_NEW, Some(set), '+', content)
        }
        "-" => {
            let set = match inner_first {
                Some(b'+') => C_NEW_DIM,
                Some(b'@') => C_FRAG,
                Some(b'-') => C_OLD_DIM,
                _ => C_CONTEXT_DIM,
            };
            emit_line0(C_REVERSE, C_OLD, Some(set), '-', content)
        }
        _ => {
            // Context line: emit_line_0(set_sign=set, set=NULL, reverse=0).
            // Git's set_sign is a non-NULL (possibly empty) string, so a trailing
            // RESET is always emitted once the sign char is written.
            let set = match inner_first {
                Some(b'+') => C_NEW,
                Some(b'@') => C_FRAG,
                Some(b'-') => C_OLD,
                _ => C_CONTEXT,
            };
            let mut s = String::new();
            s.push_str(set); // possibly ""
            s.push(' ');
            s.push_str(content);
            s.push_str(C_RESET);
            s
        }
    }
}

/// Mirror of Git's `emit_line_0` for the `+`/`-` (set_sign present, reverse) cases.
/// When `content` is empty, Git jumps to end-of-line before emitting `set`.
fn emit_line0(
    reverse: &str,
    set_sign: &str,
    set: Option<&str>,
    sign: char,
    content: &str,
) -> String {
    let mut s = String::new();
    s.push_str(reverse);
    s.push_str(set_sign);
    s.push(sign);
    if !content.is_empty() {
        if let Some(set) = set {
            if set != set_sign {
                s.push_str(C_RESET);
            }
            s.push_str(set);
        }
        s.push_str(content);
    }
    s.push_str(C_RESET);
    s
}

/// Parse the 1-based old-file start line from a unified-diff hunk header
/// (`@@ -<start>[,<count>] +... @@`). Returns `None` if it cannot be parsed.
fn parse_hunk_old_start(header: &str) -> Option<usize> {
    let minus = header.find('-')?;
    let rest = &header[minus + 1..];
    let end = rest.find([',', ' '])?;
    rest[..end].parse::<usize>().ok()
}

/// Reproduce Git `range-diff`'s `section_headers` funcname driver:
/// `^ ## (.*) ##$` and `^.?@@ (.*)$`. Scans the old patch text backward from the
/// hunk start for the nearest line matching either rule, returning the captured name.
fn section_funcname(old_lines: &[&str], old_start_1based: usize) -> Option<String> {
    if old_start_1based <= 1 {
        return None;
    }
    let search_end = (old_start_1based - 1).min(old_lines.len());
    for i in (0..search_end).rev() {
        let line = old_lines[i];
        if line.is_empty() {
            continue;
        }
        if let Some(name) = match_section_header(line) {
            return Some(truncate_funcname(&name));
        }
    }
    None
}

/// Match a single line against the two `range-diff` section-header rules.
fn match_section_header(line: &str) -> Option<String> {
    // Rule 1: ` ## <name> ##`
    if let Some(inner) = line.strip_prefix(" ## ") {
        if let Some(name) = inner.strip_suffix(" ##") {
            return Some(name.trim_end_matches(char::is_whitespace).to_owned());
        }
    }
    // Rule 2: `.?@@ <name>` — an optional leading byte, then "@@ ", then the name.
    let after = if let Some(a) = line.strip_prefix("@@ ") {
        a
    } else {
        let bytes = line.as_bytes();
        if bytes.len() >= 4 && &bytes[1..4] == b"@@ " {
            &line[4..]
        } else {
            return None;
        }
    };
    Some(after.trim_end_matches(char::is_whitespace).to_owned())
}

fn truncate_funcname(text: &str) -> String {
    if text.len() > 80 {
        let mut end = 80;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_owned()
    } else {
        text.to_owned()
    }
}

fn write_pair_header(
    out: &mut impl Write,
    repo: &Repository,
    w: usize,
    left: Option<(usize, &Patch)>,
    right: Option<(usize, &Patch)>,
    abbrev_len: usize,
    dash_str: &str,
    reset: &str,
    color_old: &str,
    color_new: &str,
    color_commit: &str,
) -> Result<()> {
    let status = match (left, right) {
        (Some(_), None) => '<',
        (None, Some(_)) => '>',
        (Some((_, l)), Some((_, r))) => {
            if l.full == r.full {
                '='
            } else {
                '!'
            }
        }
        (None, None) => unreachable!(),
    };

    let oid_for_subject = left
        .map(|(_, p)| p.oid)
        .or_else(|| right.map(|(_, p)| p.oid))
        .ok_or_else(|| anyhow!("range-diff pair header requires at least one side"))?;

    match (left, right) {
        (None, Some((rj, rp))) => {
            write!(out, "{color_new}{:>w$}:  {} ", "-", dash_str, w = w)?;
            write!(out, "{status} ")?;
            write!(
                out,
                "{:>w$}:  {}",
                rj + 1,
                abbreviate_object_id(repo, rp.oid, abbrev_len)?,
                w = w
            )?;
        }
        (Some((li, lp)), None) => {
            let c0 = if status == '!' {
                color_old
            } else {
                color_commit
            };
            write!(
                out,
                "{c0}{:>w$}:  {} ",
                li + 1,
                abbreviate_object_id(repo, lp.oid, abbrev_len)?,
                w = w
            )?;
            if status == '!' {
                write!(out, "{reset}{color_commit}")?;
            }
            write!(out, "{status}")?;
            if status == '!' {
                write!(out, "{reset}{color_new}")?;
            }
            write!(out, " {:>w$}:  {}", "-", dash_str, w = w)?;
        }
        (Some((li, lp)), Some((rj, rp))) => {
            let c0 = if status == '!' {
                color_old
            } else {
                color_commit
            };
            write!(
                out,
                "{c0}{:>w$}:  {} ",
                li + 1,
                abbreviate_object_id(repo, lp.oid, abbrev_len)?,
                w = w
            )?;
            if status == '!' {
                write!(out, "{reset}{color_commit}")?;
            }
            write!(out, "{status}")?;
            if status == '!' {
                write!(out, "{reset}{color_new}")?;
            }
            write!(
                out,
                " {:>w$}:  {}",
                rj + 1,
                abbreviate_object_id(repo, rp.oid, abbrev_len)?,
                w = w
            )?;
        }
        _ => {}
    }

    // Git always appends " <subject>" for the commit; for `!` it first re-emits
    // `reset` + the header color (DIFF_COMMIT = color_commit).
    let subj = lookup_commit_subject(repo, oid_for_subject)?;
    if status == '!' {
        write!(out, "{reset}{color_commit}")?;
    }
    write!(out, " {subj}")?;
    writeln!(out, "{reset}")?;
    Ok(())
}

fn lookup_commit_subject(repo: &Repository, oid: ObjectId) -> Result<String> {
    let obj = repo.odb.read(&oid)?;
    let c = parse_commit(&obj.data)?;
    Ok(c.message.lines().next().unwrap_or("").to_string())
}

fn decimal_width(mut n: usize) -> usize {
    let mut w = 1;
    while n >= 10 {
        n /= 10;
        w += 1;
    }
    w
}
