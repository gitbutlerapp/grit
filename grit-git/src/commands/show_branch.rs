//! `grit show-branch` — colored branch ancestry display (subset of `git show-branch`).

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::git_date::show::show_date_relative;
use grit_lib::git_date::tm::{atoi_bytes, get_time_sec};
use grit_lib::merge_base::{independent_commits, merge_bases_octopus, resolve_commit_specs};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::refs::{self, list_refs};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};

/// Match `git/color.c` `column_colors_ansi` (excluding trailing `GIT_COLOR_RESET` from rotation).
const COLUMN_COLORS: [&str; 12] = [
    "\x1b[31m",
    "\x1b[32m",
    "\x1b[33m",
    "\x1b[34m",
    "\x1b[35m",
    "\x1b[36m",
    "\x1b[1;31m",
    "\x1b[1;32m",
    "\x1b[1;33m",
    "\x1b[1;34m",
    "\x1b[1;35m",
    "\x1b[1;36m",
];
const COLOR_RESET: &str = "\x1b[m";

const REV_SHIFT: u32 = 2;

/// Raw argv after `show-branch` (no clap: matches Git exit codes for unknown options).
#[derive(Debug, Default)]
pub struct Args {
    pub args: Vec<String>,
}

/// Run `show-branch` from already-split argv (used by `main` without clap).
pub fn run(args: Args) -> Result<()> {
    run_raw(&args.args)
}

/// Entry from `main.rs` with `rest` = argv after the subcommand name.
pub fn run_raw(rest: &[String]) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let parsed = parse_show_branch_args(&repo, rest.to_vec())?;
    run_parsed(&repo, parsed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorWhen {
    Never,
    Always,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    Topo,
    Date,
}

#[derive(Debug, Clone)]
struct Parsed {
    all_heads: bool,
    all_remotes: bool,
    /// `-1` = `--list`, `0` = default, `>0` = `--more=<n>`.
    extra: i32,
    merge_base: bool,
    independent: bool,
    sparse: bool,
    with_current: bool,
    reflog: bool,
    reflog_n: usize,
    reflog_base: Option<String>,
    color: ColorWhen,
    sort_order: SortOrder,
    topics: bool,
    positional: Vec<String>,
    default_from_config: bool,
    /// When true, do not enable `--all` when no revs are given (config defaults only).
    skip_implicit_all: bool,
}

fn parse_show_branch_args(repo: &Repository, raw: Vec<String>) -> Result<Parsed> {
    let mut cfg_defaults: Vec<String> = Vec::new();
    if let Ok(cs) = ConfigSet::load(Some(&repo.git_dir), true) {
        cfg_defaults = cs.get_all("showbranch.default");
    }

    let mut p = Parsed {
        all_heads: false,
        all_remotes: false,
        extra: 0,
        merge_base: false,
        independent: false,
        sparse: false,
        with_current: false,
        reflog: false,
        reflog_n: 4,
        reflog_base: None,
        color: ColorWhen::Auto,
        sort_order: SortOrder::Topo,
        topics: false,
        positional: Vec::new(),
        default_from_config: false,
        skip_implicit_all: false,
    };

    let mut color_from_config: Option<ColorWhen> = None;
    if let Ok(cs) = ConfigSet::load(Some(&repo.git_dir), true) {
        if let Some(v) = cs.get("color.showbranch") {
            color_from_config = Some(parse_color_bool(&v));
        } else if let Some(v) = cs.get("color.ui") {
            color_from_config = Some(parse_color_bool(&v));
        }
    }

    let use_cfg = raw.is_empty() && !cfg_defaults.is_empty();
    let args_iter: Vec<String> = if use_cfg {
        p.default_from_config = true;
        p.skip_implicit_all = true;
        cfg_defaults
    } else {
        raw
    };

    let mut end_opts = false;
    let mut i = 0usize;
    while i < args_iter.len() {
        let arg = &args_iter[i];
        if !end_opts && arg == "--" {
            end_opts = true;
            i += 1;
            continue;
        }
        if !end_opts && arg.starts_with('-') {
            match arg.as_str() {
                "-a" | "--all" => {
                    p.all_heads = true;
                    i += 1;
                }
                "-r" | "--remotes" => {
                    p.all_remotes = true;
                    i += 1;
                }
                "--list" => {
                    p.extra = -1;
                    i += 1;
                }
                "--more" => {
                    i += 1;
                    p.extra = match args_iter.get(i) {
                        Some(v) if !v.starts_with('-') => {
                            let n = if v.is_empty() {
                                1
                            } else {
                                v.parse::<i32>()
                                    .map_err(|_| anyhow::anyhow!("invalid --more value"))?
                            };
                            i += 1;
                            n
                        }
                        _ => 1,
                    };
                }
                s if s.starts_with("--more=") => {
                    let rest = s.strip_prefix("--more=").unwrap_or("");
                    p.extra = if rest.is_empty() {
                        1
                    } else {
                        rest.parse::<i32>()
                            .map_err(|_| anyhow::anyhow!("invalid --more value"))?
                    };
                    i += 1;
                }
                "--merge-base" => {
                    p.merge_base = true;
                    i += 1;
                }
                "--independent" => {
                    p.independent = true;
                    i += 1;
                }
                "--sparse" => {
                    p.sparse = true;
                    i += 1;
                }
                "--no-sparse" => {
                    p.sparse = false;
                    i += 1;
                }
                "--current" => {
                    p.with_current = true;
                    i += 1;
                }
                "--topics" => {
                    p.topics = true;
                    i += 1;
                }
                "--topo-order" => {
                    p.sort_order = SortOrder::Topo;
                    i += 1;
                }
                "--date-order" => {
                    p.sort_order = SortOrder::Date;
                    i += 1;
                }
                "--no-color" => {
                    p.color = ColorWhen::Never;
                    i += 1;
                }
                s if s == "--color" || s.starts_with("--color=") => {
                    let v = if s == "--color" {
                        i += 1;
                        args_iter
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("option `--color` requires a value"))?
                            .as_str()
                    } else {
                        s.strip_prefix("--color=").unwrap_or("")
                    };
                    p.color = match v {
                        "always" => ColorWhen::Always,
                        "never" => ColorWhen::Never,
                        "auto" => ColorWhen::Auto,
                        _ => bail!("unknown --color parameter: {v}"),
                    };
                    i += 1;
                }
                s if s == "-g"
                    || s == "--reflog"
                    || s.starts_with("--reflog=")
                    || s.starts_with("-g") && s.len() > 2 =>
                {
                    let (n, base) = parse_reflog_flag(s, &args_iter, &mut i)?;
                    p.reflog = true;
                    p.reflog_n = n;
                    p.reflog_base = base;
                }
                s if s.starts_with("--no-") => {
                    let tail = s.strip_prefix("--").unwrap_or(s);
                    bail!("unknown option `{tail}`");
                }
                _ => bail!("unknown option `{arg}`"),
            }
            continue;
        }
        p.positional.push(arg.clone());
        i += 1;
    }

    if p.all_heads {
        p.all_remotes = true;
    }

    if p.color == ColorWhen::Auto {
        if let Some(c) = color_from_config {
            p.color = c;
        }
    }

    validate_option_combos(&p)?;
    Ok(p)
}

fn parse_reflog_flag(
    flag: &str,
    _args: &[String],
    i: &mut usize,
) -> Result<(usize, Option<String>)> {
    let mut arg_part: Option<&str> = None;
    if flag == "-g" || flag == "--reflog" {
        *i += 1;
    } else if let Some(rest) = flag.strip_prefix("--reflog=") {
        arg_part = Some(rest);
        *i += 1;
    } else if flag.starts_with("-g") && flag.len() > 2 {
        arg_part = Some(&flag[2..]);
        *i += 1;
    }

    let param = arg_part.unwrap_or("");
    if param.is_empty() {
        return Ok((4, None));
    }
    let (n_str, base) = if let Some(pos) = param.find(',') {
        (&param[..pos], Some(param[pos + 1..].to_string()))
    } else {
        (param, None)
    };
    let n: usize = n_str
        .parse()
        .map_err(|_| anyhow::anyhow!("unrecognized reflog param '{param}'"))?;
    let n = if n == 0 { 4 } else { n };
    Ok((n, base))
}

fn parse_color_bool(v: &str) -> ColorWhen {
    let t = v.trim();
    if t.eq_ignore_ascii_case("always") {
        ColorWhen::Always
    } else if t.eq_ignore_ascii_case("never") {
        ColorWhen::Never
    } else if t.eq_ignore_ascii_case("auto") || parse_bool_loose(t).unwrap_or(false) {
        ColorWhen::Auto
    } else {
        ColorWhen::Never
    }
}

fn parse_bool_loose(s: &str) -> Option<bool> {
    match s {
        "yes" | "true" | "on" | "1" => Some(true),
        "no" | "false" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn validate_option_combos(p: &Parsed) -> Result<()> {
    if (p.extra < 0 || p.reflog) && (p.independent || p.merge_base) {
        eprintln!(
            "usage: git show-branch [-a | --all] [-r | --remotes] [--topo-order | --date-order]"
        );
        eprintln!("                       [--current] [--color[=<when>] | --no-color] [--sparse]");
        eprintln!("                       [--more=<n> | --list | --independent | --merge-base]");
        eprintln!("                       [--no-name | --sha1-name] [--topics]");
        eprintln!("                       [(<rev> | <glob>)...]");
        eprintln!("   or: git show-branch (-g | --reflog)[=<n>[,<base>]] [--list] [<ref>]");
        std::process::exit(129);
    }
    if p.reflog && ((p.extra > 0) || p.all_heads || p.all_remotes) {
        bail!("options '--reflog' and '--all/--remotes/--independent/--merge-base' cannot be used together");
    }
    if p.with_current && p.reflog {
        bail!("options '--reflog' and '--current' cannot be used together");
    }
    Ok(())
}

fn want_color_stdout(w: ColorWhen) -> bool {
    match w {
        ColorWhen::Never => false,
        ColorWhen::Always => true,
        ColorWhen::Auto => io::stdout().is_terminal() && grit_lib::terminal::ansi_supported(),
    }
}

fn run_parsed(repo: &Repository, mut p: Parsed) -> Result<()> {
    if !p.skip_implicit_all
        && p.topics as usize >= p.positional.len()
        && !p.all_heads
        && !p.all_remotes
    {
        p.all_heads = true;
    }

    if p.reflog {
        return run_reflog_mode(repo, &p);
    }

    let mut names: Vec<String> = Vec::new();
    let mut oids: Vec<ObjectId> = Vec::new();

    for spec in &p.positional {
        if let Ok(oid) = resolve_revision(repo, spec) {
            if repo.odb.read(&oid).is_ok() {
                names.push(spec.clone());
                oids.push(oid);
                continue;
            }
        }
        collect_glob_refs(repo, spec, &mut names, &mut oids)?;
    }

    if p.all_heads || p.all_remotes {
        snarf_refs(repo, p.all_heads, p.all_remotes, &mut names, &mut oids)?;
    }

    let head = resolve_head(&repo.git_dir)?;
    let (head_sym, head_oid) = head_ref_info(repo, &head)?;

    if p.with_current {
        let mut has = false;
        for n in &names {
            if rev_is_head(head_sym.as_deref(), n) {
                has = true;
                break;
            }
        }
        if !has {
            if let Some(h) = head_sym.as_deref() {
                let short = h.strip_prefix("refs/heads/").unwrap_or(h);
                if let Ok(oid) = refs::resolve_ref(&repo.git_dir, h) {
                    names.push(short.to_string());
                    oids.push(oid);
                }
            }
        }
    }

    if names.is_empty() {
        eprintln!("No revs to be shown.");
        return Ok(());
    }

    if p.merge_base {
        return run_merge_base_display(repo, &names, &oids);
    }
    if p.independent {
        return run_independent_display(repo, &oids);
    }

    run_graph_mode(repo, &p, &head, head_sym.as_deref(), head_oid, names, oids)
}

fn head_ref_info(
    _repo: &Repository,
    head: &HeadState,
) -> Result<(Option<String>, Option<ObjectId>)> {
    match head {
        HeadState::Branch { refname, oid, .. } => Ok((Some(refname.clone()), *oid)),
        HeadState::Detached { oid } => Ok((None, Some(*oid))),
        HeadState::Invalid => Ok((None, None)),
    }
}

fn rev_is_head(head_full: Option<&str>, display_name: &str) -> bool {
    let Some(h) = head_full else {
        return false;
    };
    let mut head_short = h.strip_prefix("refs/heads/").unwrap_or(h);
    let mut name = display_name;
    if let Some(r) = name.strip_prefix("refs/heads/") {
        name = r;
    } else if let Some(r) = name.strip_prefix("heads/") {
        name = r;
    }
    if let Some(r) = head_short.strip_prefix("refs/heads/") {
        head_short = r;
    }
    head_short == name
}

fn collect_glob_refs(
    repo: &Repository,
    pattern: &str,
    names: &mut Vec<String>,
    oids: &mut Vec<ObjectId>,
) -> Result<()> {
    if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
        bail!("bad sha1 reference {pattern}");
    }
    let mut matched = 0usize;
    for (full, oid) in list_refs(&repo.git_dir, "refs/heads/")? {
        let short = full
            .strip_prefix("refs/heads/")
            .unwrap_or(&full)
            .to_string();
        if wildmatch(pattern, &short) {
            names.push(short);
            oids.push(oid);
            matched += 1;
        }
    }
    for (full, oid) in list_refs(&repo.git_dir, "refs/remotes/")? {
        let short = full
            .strip_prefix("refs/remotes/")
            .unwrap_or(&full)
            .to_string();
        if wildmatch(pattern, &short) {
            names.push(short);
            oids.push(oid);
            matched += 1;
        }
    }
    if matched == 0 {
        eprintln!("error: no matching refs with {pattern}");
    }
    Ok(())
}

fn wildmatch(pattern: &str, text: &str) -> bool {
    wildmatch_bytes(pattern.as_bytes(), text.as_bytes())
}

fn wildmatch_bytes(pat: &[u8], s: &[u8]) -> bool {
    fn rec(pat: &[u8], s: &[u8]) -> bool {
        if pat.is_empty() {
            return s.is_empty();
        }
        match pat[0] {
            b'*' => {
                if rec(&pat[1..], s) {
                    return true;
                }
                !s.is_empty() && rec(pat, &s[1..])
            }
            b'?' => !s.is_empty() && rec(&pat[1..], &s[1..]),
            ch => !s.is_empty() && ch == s[0] && rec(&pat[1..], &s[1..]),
        }
    }
    rec(pat, s)
}

fn snarf_refs(
    repo: &Repository,
    heads: bool,
    remotes: bool,
    names: &mut Vec<String>,
    oids: &mut Vec<ObjectId>,
) -> Result<()> {
    if heads {
        for (full, oid) in list_refs(&repo.git_dir, "refs/heads/")? {
            let short = full
                .strip_prefix("refs/heads/")
                .unwrap_or(&full)
                .to_string();
            names.push(short);
            oids.push(oid);
        }
    }
    if remotes {
        for (full, oid) in list_refs(&repo.git_dir, "refs/remotes/")? {
            let short = full
                .strip_prefix("refs/remotes/")
                .unwrap_or(&full)
                .to_string();
            names.push(short);
            oids.push(oid);
        }
    }
    Ok(())
}

fn run_merge_base_display(repo: &Repository, names: &[String], _oids: &[ObjectId]) -> Result<()> {
    let specs: Vec<String> = names.to_vec();
    let commits = resolve_commit_specs(repo, &specs)?;
    let mut bases = merge_bases_octopus(repo, &commits)?;
    if bases.is_empty() {
        std::process::exit(1);
    }
    bases.sort();
    println!("{}", bases[0]);
    Ok(())
}

fn run_independent_display(repo: &Repository, oids: &[ObjectId]) -> Result<()> {
    let specs: Vec<String> = oids.iter().map(|o| o.to_string()).collect();
    let commits = resolve_commit_specs(repo, &specs)?;
    for oid in independent_commits(repo, &commits)? {
        println!("{oid}");
    }
    Ok(())
}

/// Anchor for `relative` timestamps in `show-branch -g` (matches Git tests using `GIT_TEST_DATE_NOW`).
///
/// Prefer `GIT_TEST_DATE_NOW` when set and valid. Otherwise, if the work tree has a `.test_tick`
/// file (Grit test harness), use it so `GIT_TEST_DATE_NOW=$test_tick` in the shell cannot go stale
/// relative to the on-disk tick file updated by `test_tick`.
fn reflog_display_now_sec(repo: &Repository) -> i64 {
    if let Some(wt) = repo.work_tree.as_deref() {
        let tick_path = wt.join(".test_tick");
        if let Ok(s) = std::fs::read_to_string(&tick_path) {
            if let Ok(v) = s.trim().parse::<i64>() {
                return v;
            }
        }
    }
    if let Ok(s) = std::env::var("GIT_TEST_DATE_NOW") {
        if let Ok(v) = s.trim().parse::<i64>() {
            return v;
        }
    }
    get_time_sec()
}

fn run_reflog_mode(repo: &Repository, p: &Parsed) -> Result<()> {
    let ref_arg = if p.positional.len() == 1 {
        p.positional[0].clone()
    } else if p.positional.is_empty() {
        let head = fs::read_to_string(repo.git_dir.join("HEAD")).unwrap_or_default();
        let t = head.trim();
        if let Some(sym) = t.strip_prefix("ref: ") {
            sym.strip_prefix("refs/heads/").unwrap_or(sym).to_string()
        } else {
            bail!("no branches given, and HEAD is not valid");
        }
    } else {
        bail!("--reflog option needs one branch name");
    };

    let full_ref = if ref_arg.starts_with("refs/") {
        ref_arg.clone()
    } else {
        format!("refs/heads/{ref_arg}")
    };

    let mut entries = grit_lib::reflog::read_reflog(&repo.git_dir, &full_ref)?;
    if entries.is_empty() {
        return Ok(());
    }
    entries.reverse();
    let take = p.reflog_n.min(entries.len());
    let slice: Vec<_> = entries.into_iter().take(take).collect();

    let display_base = full_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&full_ref)
        .to_string();

    let mut names: Vec<String> = Vec::new();
    let mut oids: Vec<ObjectId> = Vec::new();
    for (i, e) in slice.iter().enumerate() {
        names.push(format!("{display_base}@{{{i}}}"));
        oids.push(e.new_oid);
    }

    let now_sec = reflog_display_now_sec(repo);
    let mut reflog_lines: Vec<String> = Vec::new();
    for e in &slice {
        let (ts, _tz) = parse_identity_ts_tz(&e.identity);
        let when = show_date_relative(ts, now_sec);
        let msg = if e.message.is_empty() {
            "(none)"
        } else {
            e.message.trim_end_matches('\n')
        };
        reflog_lines.push(format!("({when}) {msg}"));
    }

    let head = resolve_head(&repo.git_dir)?;
    let (_, head_oid) = head_ref_info(repo, &head)?;
    // Reflog graph stops at the merge base of the shown entries; do not force `--more=1` (that
    // pulls unrelated tips such as other test branches into the graph; see t3202).
    let graph_extra = if p.extra < 0 { -1 } else { p.extra.max(0) };
    let mut p2 = p.clone();
    p2.reflog = false;
    p2.skip_implicit_all = true;
    run_graph_from_seeds(
        repo,
        &p2,
        &head,
        head_oid,
        names,
        oids,
        graph_extra,
        Some(reflog_lines),
    )
}

fn parse_identity_ts_tz(identity: &str) -> (u64, i32) {
    let after_gt = identity
        .rfind('>')
        .map(|p| &identity[p + 1..])
        .unwrap_or("");
    let mut parts = after_gt.split_whitespace();
    let ts: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let tz_s = parts.next().unwrap_or("+0000");
    let tz = atoi_bytes(tz_s.as_bytes());
    (ts, tz)
}

fn run_graph_mode(
    repo: &Repository,
    p: &Parsed,
    head: &HeadState,
    _head_sym: Option<&str>,
    head_oid: Option<ObjectId>,
    names: Vec<String>,
    oids: Vec<ObjectId>,
) -> Result<()> {
    let graph_extra = if p.extra < 0 { -1 } else { p.extra.max(1) };
    run_graph_from_seeds(repo, p, head, head_oid, names, oids, graph_extra, None)
}

#[expect(clippy::too_many_arguments)]
fn run_graph_from_seeds(
    repo: &Repository,
    p: &Parsed,
    head: &HeadState,
    head_oid: Option<ObjectId>,
    names: Vec<String>,
    oids: Vec<ObjectId>,
    merge_extra: i32,
    reflog_subject: Option<Vec<String>>,
) -> Result<()> {
    let num = names.len();
    if num == 0 {
        return Ok(());
    }

    let list_only = merge_extra < 0;
    let use_color = want_color_stdout(p.color);
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let head_sym = match head {
        HeadState::Branch { refname, .. } => Some(refname.as_str()),
        _ => None,
    };

    let mut head_at: Option<usize> = None;
    for i in 0..num {
        let is_head = head_oid == Some(oids[i]) && rev_is_head(head_sym, &names[i]);
        if is_head {
            head_at = Some(i);
        }
        if list_only || reflog_subject.is_some() || num > 1 {
            if list_only {
                let mark = if is_head { "* " } else { "  " };
                write!(out, "{mark}[{}] ", names[i])?;
            } else {
                for _ in 0..i {
                    write!(out, " ")?;
                }
                let mark = if reflog_subject.is_some() {
                    '!'
                } else if is_head {
                    '*'
                } else {
                    '!'
                };
                if use_color {
                    let c = COLUMN_COLORS[i % COLUMN_COLORS.len()];
                    write!(out, "{c}{mark}{COLOR_RESET} [{}] ", names[i])?;
                } else {
                    write!(out, "{mark} [{}] ", names[i])?;
                }
            }
            if let Some(ref lines) = reflog_subject {
                writeln!(out, "{}", lines[i])?;
            } else {
                writeln!(out, "{}", commit_oneline(repo, &oids[i])?)?;
            }
        }
    }

    if merge_extra < 0 {
        return Ok(());
    }

    if num > 1 || reflog_subject.is_some() {
        for _ in 0..num {
            write!(out, "-")?;
        }
        writeln!(out)?;
    }

    let mut rev_mask = vec![0u32; num];
    let mut queue: BinaryHeap<CommitWork> = BinaryHeap::new();
    let mut seen: Vec<ObjectId> = Vec::new();
    let mut seen_set: HashSet<ObjectId> = HashSet::new();
    let mut flags: HashMap<ObjectId, u32> = HashMap::new();

    for i in 0..num {
        let bit = 1u32 << (i as u32 + REV_SHIFT);
        rev_mask[i] = bit;
        let oid = oids[i];
        if let Some(e) = flags.get_mut(&oid) {
            *e |= bit;
        } else {
            flags.insert(oid, bit);
        }
        if !seen_set.contains(&oid) {
            seen_set.insert(oid);
            seen.push(oid);
        }
        let t = commit_committer_time(repo, &oid)?;
        queue.push(CommitWork { oid, time: t });
    }

    join_revs(
        repo,
        &mut queue,
        &mut seen,
        &mut seen_set,
        &mut flags,
        num,
        merge_extra,
    )?;

    sort_seen_by_date(repo, &mut seen)?;

    let order = topo_sort_seen(repo, &seen, p.sort_order)?;

    let mut commit_name: HashMap<ObjectId, String> = HashMap::new();
    let mut commit_gen: HashMap<ObjectId, u32> = HashMap::new();
    name_commits(
        repo,
        &order,
        &oids,
        &names,
        num,
        &mut commit_name,
        &mut commit_gen,
    )?;

    let all_mask = ((1u32 << (REV_SHIFT + num as u32)) - 1) & !((1u32 << REV_SHIFT) - 1);
    let mut shown_merge_point = false;
    let mut extra_left = merge_extra;

    for oid in order {
        let this_flag = *flags.get(&oid).unwrap_or(&0);
        let is_merge_point = (this_flag & all_mask) == all_mask;
        if is_merge_point {
            shown_merge_point = true;
        }

        if num > 1 || reflog_subject.is_some() {
            let parents = parents_of(repo, oid)?;
            let is_merge = parents.len() > 1;
            if p.topics && !is_merge_point && (this_flag & (1u32 << REV_SHIFT)) != 0 {
                continue;
            }
            if !p.sparse && is_merge && omit_in_dense(oid, &oids, num, &flags) {
                continue;
            }
            for i in 0..num {
                let bit = 1u32 << (i as u32 + REV_SHIFT);
                let mark = if (this_flag & bit) == 0 {
                    ' '
                } else if is_merge {
                    '-'
                } else if head_at == Some(i) {
                    '*'
                } else {
                    '+'
                };
                if mark == ' ' {
                    write!(out, " ")?;
                } else if use_color {
                    let c = COLUMN_COLORS[i % COLUMN_COLORS.len()];
                    write!(out, "{c}{mark}{COLOR_RESET}")?;
                } else {
                    write!(out, "{mark}")?;
                }
            }
            write!(out, " ")?;
        }

        let label = commit_name.get(&oid).map(String::as_str).unwrap_or("");
        if label.is_empty() {
            let hex = oid.to_hex();
            let short = &hex[..7.min(hex.len())];
            writeln!(out, "[{short}] {}", commit_oneline(repo, &oid)?)?;
        } else {
            writeln!(out, "[{label}] {}", commit_oneline(repo, &oid)?)?;
        }

        if shown_merge_point {
            extra_left -= 1;
            if extra_left < 0 {
                break;
            }
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
struct CommitWork {
    oid: ObjectId,
    time: i64,
}

impl Eq for CommitWork {}

impl PartialEq for CommitWork {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.oid == other.oid
    }
}

impl Ord for CommitWork {
    fn cmp(&self, other: &Self) -> Ordering {
        self.time
            .cmp(&other.time)
            .then_with(|| other.oid.cmp(&self.oid))
    }
}

impl PartialOrd for CommitWork {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const UNINTERESTING: u32 = 1;

fn heap_max_idx(heap: &[CommitWork]) -> usize {
    let mut best = 0usize;
    for i in 1..heap.len() {
        if heap[i].cmp(&heap[best]) == Ordering::Greater {
            best = i;
        }
    }
    best
}

fn join_revs(
    repo: &Repository,
    queue: &mut BinaryHeap<CommitWork>,
    seen: &mut Vec<ObjectId>,
    seen_set: &mut HashSet<ObjectId>,
    flags: &mut HashMap<ObjectId, u32>,
    num_rev: usize,
    mut extra: i32,
) -> Result<()> {
    let all_mask = (1u32 << (REV_SHIFT + num_rev as u32)) - 1;
    let all_revs = all_mask & !((1u32 << REV_SHIFT) - 1);

    let mut heap: Vec<CommitWork> = std::mem::take(queue).into_iter().collect();

    while !heap.is_empty() {
        let still_interesting = heap
            .iter()
            .any(|w| (*flags.get(&w.oid).unwrap_or(&0) & UNINTERESTING) == 0);
        if !still_interesting && extra <= 0 {
            break;
        }

        let tip_idx = heap_max_idx(&heap);
        let commit = heap[tip_idx].oid;

        let _ = mark_seen_oid(commit, seen_set, seen);

        let commit_stored = *flags.get(&commit).unwrap_or(&0);
        let mut f = commit_stored & all_mask;
        if (f & all_revs) == all_revs {
            f |= UNINTERESTING;
        }

        let parents = parents_of(repo, commit)?;
        let mut get_pending = true;
        for p in parents {
            let this_flag = *flags.get(&p).unwrap_or(&0);
            if (this_flag & f) == f {
                continue;
            }
            let newly_seen = mark_seen_oid(p, seen_set, seen);
            if newly_seen && !still_interesting {
                extra -= 1;
            }
            flags.insert(p, this_flag | f);

            let t = commit_committer_time(repo, &p)?;
            if get_pending {
                heap[tip_idx] = CommitWork { oid: p, time: t };
            } else {
                heap.push(CommitWork { oid: p, time: t });
            }
            get_pending = false;
        }

        flags.insert(commit, (commit_stored & !all_mask) | f);

        if get_pending {
            heap.swap_remove(tip_idx);
        }
    }

    loop {
        let mut changed = false;
        let snapshot: Vec<ObjectId> = seen.clone();
        for c in snapshot {
            let cf = *flags.get(&c).unwrap_or(&0);
            if ((cf & all_revs) != all_revs) && (cf & UNINTERESTING) == 0 {
                continue;
            }
            for p in parents_of(repo, c)? {
                let pf = flags.entry(p).or_insert(0);
                if (*pf & UNINTERESTING) == 0 {
                    *pf |= UNINTERESTING;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    Ok(())
}

fn mark_seen_oid(
    oid: ObjectId,
    seen_set: &mut HashSet<ObjectId>,
    seen: &mut Vec<ObjectId>,
) -> bool {
    if seen_set.contains(&oid) {
        return false;
    }
    seen_set.insert(oid);
    seen.push(oid);
    true
}

fn sort_seen_by_date(repo: &Repository, seen: &mut [ObjectId]) -> Result<()> {
    let mut times = HashMap::new();
    for oid in seen.iter() {
        times.insert(*oid, commit_committer_time(repo, oid)?);
    }
    seen.sort_by(|a, b| {
        times
            .get(b)
            .unwrap_or(&0)
            .cmp(times.get(a).unwrap_or(&0))
            .then_with(|| b.cmp(a))
    });
    Ok(())
}

fn topo_sort_seen(repo: &Repository, seen: &[ObjectId], order: SortOrder) -> Result<Vec<ObjectId>> {
    let in_set: HashSet<ObjectId> = seen.iter().copied().collect();
    let mut indegree: HashMap<ObjectId, i32> = HashMap::new();
    for oid in seen {
        indegree.insert(*oid, 1);
    }
    for oid in seen {
        for p in parents_of(repo, *oid)? {
            if in_set.contains(&p) {
                *indegree.entry(p).or_insert(0) += 1;
            }
        }
    }

    let mut tips: Vec<ObjectId> = Vec::new();
    for oid in seen {
        if *indegree.get(oid).unwrap_or(&0) == 1 {
            tips.push(*oid);
        }
    }

    if order == SortOrder::Topo {
        tips.reverse();
    }

    let mut queue: BinaryHeap<CommitWork> = BinaryHeap::new();
    for oid in tips {
        let t = commit_committer_time(repo, &oid)?;
        queue.push(CommitWork { oid, time: t });
    }

    let mut out = Vec::new();
    while let Some(w) = queue.pop() {
        let commit = w.oid;
        out.push(commit);
        for p in parents_of(repo, commit)? {
            let Some(pi) = indegree.get_mut(&p) else {
                continue;
            };
            if *pi == 0 {
                continue;
            }
            *pi -= 1;
            if *pi == 1 {
                let t = commit_committer_time(repo, &p)?;
                queue.push(CommitWork { oid: p, time: t });
            }
        }
        indegree.insert(commit, 0);
    }
    Ok(out)
}

fn omit_in_dense(
    commit: ObjectId,
    rev: &[ObjectId],
    num: usize,
    flags: &HashMap<ObjectId, u32>,
) -> bool {
    for tip in rev {
        if *tip == commit {
            return false;
        }
    }
    let f = *flags.get(&commit).unwrap_or(&0);
    let mut count = 0u32;
    for i in 0..num {
        if (f & (1u32 << (i as u32 + REV_SHIFT))) != 0 {
            count += 1;
        }
    }
    count == 1
}

fn name_commits(
    repo: &Repository,
    list: &[ObjectId],
    rev: &[ObjectId],
    ref_name: &[String],
    num_rev: usize,
    commit_name: &mut HashMap<ObjectId, String>,
    commit_gen: &mut HashMap<ObjectId, u32>,
) -> Result<()> {
    for oid in list {
        if commit_name.contains_key(oid) {
            continue;
        }
        for i in 0..num_rev {
            if rev[i] == *oid {
                commit_name.insert(*oid, ref_name[i].clone());
                commit_gen.insert(*oid, 0);
                break;
            }
        }
    }

    loop {
        let mut progress = 0u32;
        for oid in list {
            if !commit_name.contains_key(oid) {
                continue;
            }
            progress += name_first_parent_chain(repo, *oid, commit_name, commit_gen)?;
        }
        if progress == 0 {
            break;
        }
    }

    loop {
        let mut i = 0u32;
        for oid in list {
            if !commit_name.contains_key(oid) {
                continue;
            }
            let parents = parents_of(repo, *oid)?;
            let n = *commit_gen.get(oid).unwrap_or(&0);
            let head_label = commit_name.get(oid).cloned().unwrap_or_default();
            let mut nth = 0u32;
            for p in parents {
                nth += 1;
                if commit_name.contains_key(&p) {
                    continue;
                }
                let base = match n {
                    0 => head_label.clone(),
                    1 => format!("{head_label}^"),
                    _ => format!("{head_label}~{n}"),
                };
                let pname = if nth == 1 {
                    format!("{base}^")
                } else {
                    format!("{base}^{nth}")
                };
                commit_name.insert(p, pname);
                commit_gen.insert(p, 0);
                i += 1;
                i += name_first_parent_chain(repo, p, commit_name, commit_gen)?;
            }
        }
        if i == 0 {
            break;
        }
    }
    Ok(())
}

fn name_first_parent_chain(
    repo: &Repository,
    mut c: ObjectId,
    commit_name: &mut HashMap<ObjectId, String>,
    commit_gen: &mut HashMap<ObjectId, u32>,
) -> Result<u32> {
    let mut count = 0u32;
    while let Some(cname) = commit_name.get(&c).cloned() {
        let cg = *commit_gen.get(&c).unwrap_or(&0);
        let parents = parents_of(repo, c)?;
        let Some(p) = parents.first().copied() else {
            break;
        };
        let new_gen = cg + 1;
        let update = match commit_gen.get(&p) {
            None => true,
            Some(&pg) => new_gen < pg,
        };
        if !update {
            break;
        }
        commit_gen.insert(p, new_gen);
        let pname = match new_gen {
            1 => format!("{cname}^"),
            _ => format!("{cname}~{}", new_gen - 1),
        };
        commit_name.insert(p, pname);
        count += 1;
        c = p;
    }
    Ok(count)
}

fn parents_of(repo: &Repository, oid: ObjectId) -> Result<Vec<ObjectId>> {
    let obj = repo.odb.read(&oid)?;
    let c = parse_commit(&obj.data)?;
    Ok(c.parents)
}

fn commit_committer_time(repo: &Repository, oid: &ObjectId) -> Result<i64> {
    let obj = repo.odb.read(oid)?;
    let c = parse_commit(&obj.data)?;
    parse_sig_time(&c.committer)
}

fn parse_sig_time(sig: &str) -> Result<i64> {
    let after = sig.rfind('>').map(|p| &sig[p + 1..]).unwrap_or("");
    let mut it = after.split_whitespace();
    let sec: i64 = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("bad signature"))?
        .parse()?;
    Ok(sec)
}

fn commit_oneline(repo: &Repository, oid: &ObjectId) -> Result<String> {
    let obj = repo.odb.read(oid)?;
    let c = parse_commit(&obj.data)?;
    let line = c.message.lines().next().unwrap_or("").to_string();
    let line = line.strip_prefix("[PATCH] ").unwrap_or(&line).to_string();
    Ok(line)
}
