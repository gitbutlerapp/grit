//! `git rev-parse --parseopt` — parse an option spec from stdin and normalize argv.
//!
//! Matches Git's `cmd_parseopt` in `builtin/rev-parse.c` and `parse_options` in `parse-options.c`.

use anyhow::{bail, Context, Result};
use std::io::{self, BufRead, Write};

const PARSE_OPT_NOARG: u8 = 1 << 1;
const PARSE_OPT_OPTARG: u8 = 1 << 0;
const PARSE_OPT_NONEG: u8 = 1 << 2;
const PARSE_OPT_HIDDEN: u8 = 1 << 3;
const PARSE_OPT_LITERAL_ARGHELP: u8 = 1 << 6;

#[derive(Clone)]
struct OptEntry {
    is_group: bool,
    /// When false, the entry is used only for parsing (e.g. implicit `--no-<name>` form).
    show_in_usage: bool,
    short_name: Option<char>,
    long_name: Option<String>,
    flags: u8,
    argh: Option<String>,
    help: String,
}

#[derive(Clone, Copy, Default)]
struct CliFlags {
    keep_dashdash: bool,
    stop_at_non_option: bool,
    stuck_long: bool,
}

fn env_git_test_bool(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let lower = v.to_ascii_lowercase();
            !(lower.is_empty()
                || lower == "0"
                || lower == "false"
                || lower == "no"
                || lower == "off")
        }
        Err(_) => false,
    }
}

fn find_first_space(s: &str) -> Option<usize> {
    s.find(|c: char| c.is_whitespace())
}

fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

fn usage_argh_piece(entry: &OptEntry) -> String {
    // Matches `usage_argh()` in Git's parse-options.c.
    let literal = (entry.flags & PARSE_OPT_LITERAL_ARGHELP) != 0
        || entry.argh.is_none()
        || entry.argh.as_deref().is_some_and(|a| {
            a.chars()
                .any(|c| matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '|'))
        });
    let template = if (entry.flags & PARSE_OPT_OPTARG) != 0 {
        if entry.long_name.is_some() {
            if literal {
                "[=%s]"
            } else {
                "[=<%s>]"
            }
        } else if literal {
            "[%s]"
        } else {
            "[<%s>]"
        }
    } else if literal {
        " %s"
    } else {
        " <%s>"
    };
    let token = entry.argh.as_deref().unwrap_or("...");
    template.replace("%s", token)
}

const USAGE_OPTS_WIDTH: usize = 26;

fn usage_padding(out: &mut dyn Write, pos: usize) -> io::Result<()> {
    if pos < USAGE_OPTS_WIDTH {
        write!(out, "{:width$}", "", width = USAGE_OPTS_WIDTH - pos)?;
    } else {
        write!(out, "\n{:width$}", "", width = USAGE_OPTS_WIDTH)?;
    }
    Ok(())
}

fn find_option_by_long_name<'a>(opts: &'a [OptEntry], long_name: &str) -> Option<&'a OptEntry> {
    opts.iter()
        .find(|o| o.long_name.as_deref().is_some_and(|n| n == long_name))
}

fn print_parseopt_usage(
    out: &mut dyn Write,
    usage_lines: &[String],
    opts: &[OptEntry],
    full: bool,
    shell_eval: bool,
) -> io::Result<()> {
    const USAGE_PREFIX: &str = "usage: %s";
    const OR_PREFIX: &str = "   or: %s";
    let usage_len = USAGE_PREFIX.len() - "%s".len();
    let mut saw_empty_line = false;
    let mut prefix = USAGE_PREFIX;

    if shell_eval {
        writeln!(out, "cat <<\\EOF")?;
    }

    for u in usage_lines {
        if !saw_empty_line && u.is_empty() {
            saw_empty_line = true;
        }
        if u.contains('\n') {
            for (j, line) in u.split('\n').enumerate() {
                if saw_empty_line && !line.is_empty() {
                    writeln!(out, "    {line}")?;
                } else if saw_empty_line && line.is_empty() {
                    writeln!(out)?;
                } else if j == 0 {
                    writeln!(out, "{}", prefix.replace("%s", line))?;
                } else {
                    writeln!(out, "{:usage_len$}{line}", "", usage_len = usage_len)?;
                }
            }
        } else if saw_empty_line && !u.is_empty() {
            writeln!(out, "    {u}")?;
        } else if saw_empty_line && u.is_empty() {
            writeln!(out)?;
        } else {
            writeln!(out, "{}", prefix.replace("%s", u))?;
        }
        prefix = OR_PREFIX;
    }

    let mut need_newline = true;
    for entry in opts {
        if !entry.show_in_usage {
            continue;
        }
        if entry.is_group {
            writeln!(out)?;
            need_newline = false;
            if !entry.help.is_empty() {
                writeln!(out, "{}", entry.help)?;
            }
            continue;
        }
        if (entry.flags & PARSE_OPT_HIDDEN) != 0 && !full {
            continue;
        }
        if need_newline {
            writeln!(out)?;
            need_newline = false;
        }
        let mut pos = 4usize;
        write!(out, "    ")?;
        if let Some(c) = entry.short_name {
            write!(out, "-{c}")?;
            pos += 2;
        }
        if entry.long_name.is_some() && entry.short_name.is_some() {
            write!(out, ", ")?;
            pos += 2;
        }
        let mut positive_name: Option<&str> = None;
        if let Some(ln) = entry.long_name.as_deref() {
            if (entry.flags & PARSE_OPT_NONEG) != 0 {
                write!(out, "--{ln}")?;
                pos += 2 + ln.len();
            } else if let Some(rest) = ln.strip_prefix("no-") {
                positive_name = Some(rest);
                write!(out, "--{ln}")?;
                pos += 2 + ln.len();
            } else {
                write!(out, "--[no-]{ln}")?;
                pos += 7 + ln.len();
            }
        }
        if (entry.flags & PARSE_OPT_LITERAL_ARGHELP) != 0 || (entry.flags & PARSE_OPT_NOARG) == 0 {
            let piece = usage_argh_piece(entry);
            write!(out, "{piece}")?;
            pos += piece.len();
        }
        for (chunk_i, chunk) in entry.help.split('\n').enumerate() {
            if chunk_i > 0 {
                pos = 0;
            }
            usage_padding(out, pos)?;
            pos = 0;
            writeln!(out, "{chunk}")?;
        }
        if let Some(pn) = positive_name {
            if find_option_by_long_name(opts, pn).is_none() {
                pos = 4;
                write!(out, "    --{pn}")?;
                pos += 2 + pn.len();
                usage_padding(out, pos)?;
                writeln!(out, "opposite of --no-{pn}")?;
            }
        }
    }
    writeln!(out)?;
    if shell_eval {
        writeln!(out, "EOF")?;
    }
    Ok(())
}

fn exit_with_usage(usage_lines: &[String], opts: &[OptEntry], full: bool) -> ! {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = print_parseopt_usage(&mut out, usage_lines, opts, full, true);
    std::process::exit(129);
}

fn exit_unknown_option(usage_lines: &[String], opts: &[OptEntry], opt_display: &str) -> ! {
    // Matches gettext `unknown option \`%s\''` in Git's parse-options.c.
    eprintln!("error: unknown option `{opt_display}'");
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let _ = print_parseopt_usage(&mut err, usage_lines, opts, false, false);
    std::process::exit(129);
}

fn exit_with_usage_stderr(usage_lines: &[String], opts: &[OptEntry], full: bool) -> ! {
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let _ = print_parseopt_usage(&mut err, usage_lines, opts, full, false);
    std::process::exit(129);
}

fn append_parsed(
    parsed: &mut String,
    entry: &OptEntry,
    arg: Option<&str>,
    unset: bool,
    stuck_long: bool,
) {
    if unset {
        if let Some(ln) = entry.long_name.as_deref() {
            parsed.push_str(&format!(" --no-{ln}"));
        }
    } else if let Some(sn) = entry
        .short_name
        .filter(|_| entry.long_name.is_none() || !stuck_long)
    {
        parsed.push(' ');
        parsed.push('-');
        parsed.push(sn);
    } else if let Some(ln) = entry.long_name.as_deref() {
        parsed.push_str(&format!(" --{ln}"));
    }
    if let Some(a) = arg {
        if !stuck_long {
            parsed.push(' ');
        } else if entry.long_name.is_some() {
            parsed.push('=');
        }
        // Short-only + stuck-long: value is glued to the switch (`-C'Z'`), no separator.
        parsed.push('\'');
        parsed.push_str(&shell_escape(a));
        parsed.push('\'');
    }
}

#[derive(Clone, Copy)]
struct MatchedLong {
    idx: usize,
    /// Third argument to Git's parseopt callback (`flags ^ opt_flags`).
    callback_unset: bool,
}

#[derive(Clone, Copy)]
struct AbbrevEntry {
    idx: usize,
    /// Git stores `flags ^ opt_flags` for abbreviation disambiguation.
    combined_unset: bool,
}

fn parse_long_option(
    arg: &str,
    options: &[OptEntry],
    disallow_abbrev: bool,
) -> Result<Option<MatchedLong>, ()> {
    let eq = arg.find('=');
    let arg_end = eq.unwrap_or(arg.len());
    let mut arg_start = arg;
    let mut user_unset = false;
    let mut arg_starts_with_no_no = false;

    if let Some(rest) = arg_start.strip_prefix("no-") {
        arg_start = rest;
        if let Some(rest2) = arg_start.strip_prefix("no-") {
            arg_start = rest2;
            arg_starts_with_no_no = true;
        } else {
            user_unset = true;
        }
    }

    let mut exact: Option<MatchedLong> = None;
    let mut abbrev: Option<AbbrevEntry> = None;
    let mut ambig: Option<(AbbrevEntry, AbbrevEntry)> = None;

    for (idx, opt) in options.iter().enumerate() {
        if opt.is_group {
            continue;
        }
        let Some(long_raw) = opt.long_name.as_deref() else {
            continue;
        };
        let mut long_name = long_raw;
        let mut opt_unset_form = false;
        let allow_unset = (opt.flags & PARSE_OPT_NONEG) == 0;

        if let Some(rest) = long_name.strip_prefix("no-") {
            long_name = rest;
            opt_unset_form = true;
        } else if arg_starts_with_no_no {
            continue;
        }

        if user_unset != opt_unset_form && !allow_unset {
            continue;
        }

        if arg_start.starts_with(long_name)
            && (arg_start.len() == long_name.len()
                || arg_start.as_bytes().get(long_name.len()) == Some(&b'='))
        {
            exact = Some(MatchedLong {
                idx,
                callback_unset: user_unset ^ opt_unset_form,
            });
            break;
        }

        if disallow_abbrev {
            continue;
        }

        let prefix_len = arg_end.min(arg_start.len()).min(long_name.len());
        if long_name.as_bytes().get(..prefix_len) == arg_start.as_bytes().get(..prefix_len) {
            let combined = user_unset ^ opt_unset_form;
            let m = AbbrevEntry {
                idx,
                combined_unset: combined,
            };
            if let Some(prev) = abbrev {
                if prev.idx != m.idx || prev.combined_unset != m.combined_unset {
                    ambig = Some((prev, m));
                }
            } else {
                abbrev = Some(m);
            }
        }

        if allow_unset && arg.starts_with("no-") {
            let neg_arg = arg.strip_prefix("no-").unwrap_or(arg);
            let prefix_len = arg_end.min(neg_arg.len()).min(long_name.len());
            if long_name.as_bytes().get(..prefix_len) == neg_arg.as_bytes().get(..prefix_len) {
                let combined = true ^ opt_unset_form;
                let m = AbbrevEntry {
                    idx,
                    combined_unset: combined,
                };
                if let Some(prev) = abbrev {
                    if prev.idx != m.idx || prev.combined_unset != m.combined_unset {
                        ambig = Some((prev, m));
                    }
                } else {
                    abbrev = Some(m);
                }
            }
        }
    }

    if let Some(e) = exact {
        return Ok(Some(e));
    }

    if disallow_abbrev && abbrev.is_some() {
        return Err(());
    }

    if let Some((a, b)) = ambig {
        let oa = &options[a.idx];
        let ob = &options[b.idx];
        let fmt = |o: &OptEntry, neg: bool| -> String {
            let raw = o.long_name.as_deref().unwrap_or("");
            let positive = raw.strip_prefix("no-").unwrap_or(raw);
            if neg {
                format!("no-{positive}")
            } else {
                positive.to_string()
            }
        };
        let an = fmt(oa, a.combined_unset);
        let bn = fmt(ob, b.combined_unset);
        eprintln!(
            "error: ambiguous option: {} (could be --{} or --{})",
            &arg[..arg_end],
            an,
            bn
        );
        return Err(());
    }

    Ok(abbrev.map(|a| MatchedLong {
        idx: a.idx,
        callback_unset: a.combined_unset,
    }))
}

/// Returns `(value_for_callback, extra_argv_advance)` where `extra_argv_advance` is 0 or 1
/// (additional argv slots consumed after the current option token).
fn consume_argument(
    entry: &OptEntry,
    argv: &[String],
    opt_index: usize,
    attached: Option<String>,
    unset: bool,
) -> Result<(Option<String>, usize), &'static str> {
    if unset {
        if attached.is_some() {
            return Err("takes no value");
        }
        return Ok((None, 0));
    }
    if (entry.flags & PARSE_OPT_NOARG) != 0 {
        if attached.is_some() {
            return Err("takes no value");
        }
        return Ok((None, 0));
    }
    if let Some(a) = attached {
        return Ok((Some(a), 0));
    }
    if (entry.flags & PARSE_OPT_OPTARG) != 0 {
        return Ok((None, 0));
    }
    if opt_index + 1 < argv.len() {
        Ok((Some(argv[opt_index + 1].clone()), 1))
    } else {
        Err("requires a value")
    }
}

fn parse_spec(stdin_lines: &[String]) -> Result<(Vec<String>, Vec<OptEntry>)> {
    let sep = match stdin_lines.iter().position(|l| l == "--") {
        Some(i) => i,
        None => bail!("fatal: premature end of input"),
    };
    let usage_lines: Vec<String> = stdin_lines[..sep].to_vec();
    if usage_lines.is_empty() {
        bail!("fatal: no usage string given before the `--' separator");
    }
    let mut options = Vec::new();
    for raw in &stdin_lines[sep + 1..] {
        if raw.is_empty() {
            continue;
        }
        let Some(sp) = find_first_space(raw) else {
            // No whitespace: Git treats as OPTION_GROUP (entire line is header text).
            options.push(OptEntry {
                is_group: true,
                show_in_usage: true,
                short_name: None,
                long_name: None,
                flags: 0,
                argh: None,
                help: raw.trim().to_string(),
            });
            continue;
        };
        if sp == 0 {
            // Leading whitespace before any non-space: OPTION_GROUP (see parse-options.c).
            options.push(OptEntry {
                is_group: true,
                show_in_usage: true,
                short_name: None,
                long_name: None,
                flags: 0,
                argh: None,
                help: raw.trim().to_string(),
            });
            continue;
        }
        let name_field = &raw[..sp];
        let help = raw[sp + 1..].trim().to_string();
        let flag_start = name_field
            .find(['*', '=', '?', '!'])
            .unwrap_or(name_field.len());
        if flag_start == 0 {
            bail!("fatal: missing opt-spec before option flags");
        }
        let (short_name, long_name) = if flag_start == 1 {
            let sc = name_field
                .chars()
                .next()
                .context("empty opt-spec short name")?;
            (Some(sc), None::<String>)
        } else if name_field.as_bytes().get(1) != Some(&b',') {
            (None, Some(name_field[..flag_start].to_string()))
        } else {
            let sc = name_field
                .chars()
                .next()
                .context("empty opt-spec short name")?;
            (Some(sc), Some(name_field[2..flag_start].to_string()))
        };
        let mut flags: u8 = PARSE_OPT_NOARG;
        let mut j = flag_start;
        while j < name_field.len() {
            match name_field.as_bytes()[j] {
                b'=' => {
                    flags &= !PARSE_OPT_NOARG;
                    j += 1;
                }
                b'?' => {
                    flags &= !PARSE_OPT_NOARG;
                    flags |= PARSE_OPT_OPTARG;
                    j += 1;
                }
                b'!' => {
                    flags |= PARSE_OPT_NONEG;
                    j += 1;
                }
                b'*' => {
                    flags |= PARSE_OPT_HIDDEN;
                    j += 1;
                }
                _ => break,
            }
        }
        let argh = if j < name_field.len() {
            Some(name_field[j..].to_string())
        } else {
            None
        };
        options.push(OptEntry {
            is_group: false,
            show_in_usage: true,
            short_name,
            long_name,
            flags,
            argh,
            help,
        });
    }

    let mut synthetic: Vec<OptEntry> = Vec::new();
    for o in &options {
        if o.is_group {
            continue;
        }
        let Some(ln) = o.long_name.as_deref() else {
            continue;
        };
        if ln.starts_with("no-") || (o.flags & PARSE_OPT_NONEG) != 0 {
            continue;
        }
        let mut syn = o.clone();
        syn.show_in_usage = false;
        syn.long_name = Some(format!("no-{ln}"));
        syn.help.clear();
        synthetic.push(syn);
    }
    options.extend(synthetic);

    Ok((usage_lines, options))
}

/// Entry point for `rev-parse --parseopt` (args after `--parseopt`).
pub fn run_parseopt(extra_args: &[String]) -> Result<()> {
    let mut cli = CliFlags::default();
    let mut pos = 0;
    while pos < extra_args.len() && extra_args[pos] != "--" {
        match extra_args[pos].as_str() {
            "--keep-dashdash" => cli.keep_dashdash = true,
            "--stop-at-non-option" => cli.stop_at_non_option = true,
            "--stuck-long" => cli.stuck_long = true,
            _ => bail!("usage: git rev-parse --parseopt -- [<args>...]"),
        }
        pos += 1;
    }
    if pos >= extra_args.len() || extra_args[pos] != "--" {
        bail!("usage: git rev-parse --parseopt -- [<args>...]");
    }
    let argv: Vec<String> = extra_args[pos + 1..].to_vec();

    let stdin = io::stdin();
    let mut stdin_lines: Vec<String> = Vec::new();
    for line in stdin.lock().lines() {
        stdin_lines.push(line?);
    }
    let (usage_lines, options) = parse_spec(&stdin_lines)?;

    let disallow_abbrev = env_git_test_bool("GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS");

    if argv.len() == 1 && argv[0] == "-h" {
        exit_with_usage(&usage_lines, &options, false);
    }

    let mut parsed = String::from("set --");
    let mut out_args: Vec<String> = Vec::new();
    let mut i = 0usize;

    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with('-') || arg == "-" {
            if cli.stop_at_non_option {
                out_args.extend(argv[i..].iter().cloned());
                break;
            }
            out_args.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            if cli.keep_dashdash {
                out_args.push(arg.clone());
            }
            out_args.extend(argv[i + 1..].iter().cloned());
            break;
        }
        if arg == "--help-all" {
            exit_with_usage(&usage_lines, &options, true);
        }
        if arg == "--help" {
            exit_with_usage(&usage_lines, &options, false);
        }

        if let Some(inner) = arg.strip_prefix("--") {
            let attached_from_eq = inner.find('=').map(|p| inner[p + 1..].to_string());
            let name_part = inner.find('=').map(|p| &inner[..p]).unwrap_or(inner);
            let matched = parse_long_option(name_part, &options, disallow_abbrev);
            let Ok(Some(m)) = matched else {
                if matched.is_err() {
                    exit_with_usage_stderr(&usage_lines, &options, false);
                }
                exit_unknown_option(&usage_lines, &options, inner);
            };
            let entry = &options[m.idx];
            let (val, extra) =
                match consume_argument(entry, &argv, i, attached_from_eq, m.callback_unset) {
                    Ok(v) => v,
                    Err(msg) => {
                        eprintln!("error: option `{inner}`: {msg}");
                        exit_with_usage_stderr(&usage_lines, &options, false);
                    }
                };
            append_parsed(
                &mut parsed,
                entry,
                val.as_deref(),
                m.callback_unset,
                cli.stuck_long,
            );
            i += 1 + extra;
            continue;
        }

        let cluster = arg[1..].to_string();
        let mut rest = cluster;
        let mut cluster_argv_extra: usize = 0;
        while !rest.is_empty() {
            let c = rest.remove(0);
            let mut idx_opt = None;
            for (idx, o) in options.iter().enumerate() {
                if !o.is_group && o.show_in_usage && o.short_name == Some(c) {
                    idx_opt = Some(idx);
                    break;
                }
            }
            let Some(idx) = idx_opt else {
                if c == 'h' {
                    exit_with_usage(&usage_lines, &options, false);
                }
                eprintln!("error: unknown switch `{c}'");
                exit_with_usage_stderr(&usage_lines, &options, false);
            };
            let entry = &options[idx];
            let attached = if !rest.is_empty() {
                Some(rest.clone())
            } else {
                None
            };
            if attached.is_some() {
                rest.clear();
            }
            let (val, extra) = match consume_argument(entry, &argv, i, attached, false) {
                Ok(v) => v,
                Err(msg) => {
                    eprintln!("error: switch `{c}` {msg}");
                    exit_with_usage_stderr(&usage_lines, &options, false);
                }
            };
            if extra > 0 {
                if cluster_argv_extra > 0 {
                    eprintln!("error: switch `{c}` requires a value");
                    exit_with_usage(&usage_lines, &options, false);
                }
                cluster_argv_extra = extra;
            }
            append_parsed(&mut parsed, entry, val.as_deref(), false, cli.stuck_long);
            if extra == 1 {
                break;
            }
        }
        i += 1 + cluster_argv_extra;
    }

    parsed.push_str(" --");
    for a in &out_args {
        parsed.push(' ');
        parsed.push('\'');
        parsed.push_str(&shell_escape(a));
        parsed.push('\'');
    }
    println!("{parsed}");
    Ok(())
}
