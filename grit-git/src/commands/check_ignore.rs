//! `grit check-ignore` - debug gitignore / exclude matching.

use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::ignore::{normalize_repo_relative, submodule_containing_path, IgnoreMatcher};
use grit_lib::index::Index;
use grit_lib::repo::Repository;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

/// Arguments for `grit check-ignore`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Raw command arguments forwarded by the CLI parser.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

/// Run `grit check-ignore`.
pub fn run(args: Args) -> Result<()> {
    match run_inner(args) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("fatal: {e:#}");
            std::process::exit(128);
        }
    }
}

fn run_inner(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    let repo = Repository::discover(None)
        .context("not a git repository (or any of the parent directories)")?;
    let work_tree = repo
        .work_tree
        .as_ref()
        .ok_or_else(|| anyhow!("this operation must be run in a work tree"))?;

    let parsed = parse_args(&args.args)?;
    validate_args(&parsed)?;

    let index = if parsed.no_index {
        None
    } else {
        Some(repo.load_index().context("failed to read index")?)
    };
    let index_ref = index.as_ref();

    let mut matcher =
        IgnoreMatcher::from_repository(&repo).context("failed to load ignore rules")?;

    let mut out = io::stdout().lock();
    let mut matched_count = 0usize;

    if parsed.stdin {
        if parsed.nul_terminated {
            let mut buf = Vec::new();
            io::stdin()
                .read_to_end(&mut buf)
                .context("failed to read stdin")?;
            if buf.is_empty() {
                std::process::exit(1);
            }
            for chunk in buf.split(|b| *b == b'\0') {
                if chunk.is_empty() {
                    continue;
                }
                let raw_path = String::from_utf8_lossy(chunk).to_string();
                matched_count += process_one_path(
                    &parsed,
                    &repo,
                    &cwd,
                    work_tree,
                    index_ref,
                    &mut matcher,
                    &mut out,
                    &raw_path,
                    false,
                )?;
            }
        } else {
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            let mut line = String::new();
            let mut any_line = false;
            loop {
                line.clear();
                let n = reader.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                any_line = true;
                if line.ends_with('\n') {
                    line.pop();
                }
                if line.ends_with('\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                matched_count += process_one_path(
                    &parsed,
                    &repo,
                    &cwd,
                    work_tree,
                    index_ref,
                    &mut matcher,
                    &mut out,
                    &line,
                    true,
                )?;
            }
            if !any_line {
                std::process::exit(1);
            }
        }
    } else {
        for raw_path in &parsed.paths {
            matched_count += process_one_path(
                &parsed,
                &repo,
                &cwd,
                work_tree,
                index_ref,
                &mut matcher,
                &mut out,
                raw_path,
                false,
            )?;
        }
    }

    out.flush().context("failed to flush output")?;

    if matched_count > 0 {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

#[expect(clippy::too_many_arguments)]
fn process_one_path(
    parsed: &ParsedArgs,
    repo: &Repository,
    cwd: &Path,
    work_tree: &Path,
    index_ref: Option<&Index>,
    matcher: &mut IgnoreMatcher,
    out: &mut dyn Write,
    raw_path: &str,
    stdin_text_mode: bool,
) -> Result<usize> {
    let path_for_match = if stdin_text_mode {
        unquote_git_stdin_line(raw_path)
    } else {
        raw_path.to_string()
    };
    let output_path = if stdin_text_mode {
        check_ignore_display_path(&path_for_match)
    } else {
        raw_path.to_string()
    };
    let repo_rel =
        normalize_repo_relative(repo, cwd, &path_for_match).map_err(|e| anyhow!(e.to_string()))?;
    path_beyond_symlink(work_tree, &repo_rel, raw_path)?;
    if let Some(ix) = index_ref {
        if let Some(sm) = submodule_containing_path(&repo_rel, ix) {
            bail!("Pathspec '{raw_path}' is in submodule '{sm}'");
        }
    }
    let abs = work_tree.join(Path::new(&repo_rel));
    let is_dir = fs::metadata(&abs).map(|m| m.is_dir()).unwrap_or(false);

    let (ignored, matched) = matcher
        .check_path(repo, index_ref, &repo_rel, is_dir)
        .map_err(|e| anyhow!(e.to_string()))?;

    for w in matcher.take_warnings() {
        eprintln!("{w}");
    }

    let reportable_match = matched
        .as_ref()
        .map(|rule| parsed.verbose || !rule.negative)
        .unwrap_or(false);
    let mut count = 0usize;
    if reportable_match {
        count = 1;
    }

    if parsed.quiet {
        return Ok(count);
    }

    if parsed.verbose {
        if let Some(matched_rule) = matched {
            write_verbose_record(
                out,
                parsed.nul_terminated,
                &matched_rule.source_display,
                matched_rule.line_number,
                &matched_rule.pattern_text,
                &output_path,
            )?;
        } else if parsed.non_matching {
            write_verbose_non_match(out, parsed.nul_terminated, &output_path)?;
        }
    } else if ignored {
        write_plain_record(out, parsed.nul_terminated, &output_path)?;
    }
    Ok(count)
}

fn unquote_git_stdin_line(line: &str) -> String {
    let t = line.trim();
    if !t.starts_with('"') {
        return t.to_string();
    }
    let bytes = t.as_bytes();
    let mut i = 1usize;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => break,
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    o => out.push(o as char),
                }
                i += 1;
            }
            o => {
                out.push(o as char);
                i += 1;
            }
        }
    }
    out
}

fn check_ignore_display_path(path: &str) -> String {
    if path.contains('"') || path.contains('\\') {
        let mut s = String::new();
        s.push('"');
        for c in path.chars() {
            match c {
                '"' => s.push_str("\\\""),
                '\\' => s.push_str("\\\\"),
                c => s.push(c),
            }
        }
        s.push('"');
        s
    } else {
        path.to_string()
    }
}

fn path_beyond_symlink(work_tree: &Path, repo_rel: &str, raw_path: &str) -> Result<()> {
    if repo_rel.is_empty() {
        return Ok(());
    }
    let mut cur = work_tree.to_path_buf();
    let parts: Vec<&str> = repo_rel.split('/').filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        cur.push(part);
        let md = match fs::symlink_metadata(&cur) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        if md.file_type().is_symlink() && i + 1 < parts.len() {
            bail!("pathspec '{raw_path}' is beyond a symbolic link");
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ParsedArgs {
    quiet: bool,
    verbose: bool,
    stdin: bool,
    nul_terminated: bool,
    non_matching: bool,
    no_index: bool,
    paths: Vec<String>,
}

fn parse_args(raw: &[String]) -> Result<ParsedArgs> {
    let mut parsed = ParsedArgs::default();
    let mut i = 0usize;
    while i < raw.len() {
        let arg = &raw[i];
        match arg.as_str() {
            "--" => {
                parsed.paths.extend(raw.iter().skip(i + 1).cloned());
                break;
            }
            "-q" | "--quiet" => parsed.quiet = true,
            "-v" | "--verbose" => parsed.verbose = true,
            "--stdin" => parsed.stdin = true,
            "-z" => parsed.nul_terminated = true,
            "-n" | "--non-matching" => parsed.non_matching = true,
            "--no-index" => parsed.no_index = true,
            _ if arg.starts_with('-') => bail!("unsupported option: {arg}"),
            _ => parsed.paths.push(arg.clone()),
        }
        i += 1;
    }
    Ok(parsed)
}

fn validate_args(args: &ParsedArgs) -> Result<()> {
    if args.stdin && !args.paths.is_empty() {
        bail!("cannot specify pathnames with --stdin");
    }
    if !args.stdin {
        if args.nul_terminated {
            bail!("-z only makes sense with --stdin");
        }
        if args.paths.is_empty() {
            bail!("no path specified");
        }
    }
    if args.quiet {
        // Match git: multiple paths with --quiet is reported before --quiet/--verbose conflict.
        if !args.stdin && args.paths.len() != 1 {
            bail!("--quiet is only valid with a single pathname");
        }
        if args.verbose {
            bail!("cannot have both --quiet and --verbose");
        }
    }
    if args.non_matching && !args.verbose {
        bail!("--non-matching is only valid with --verbose");
    }
    if args
        .paths
        .iter()
        .any(|path| path.starts_with(":(attr:") || path.contains(",attr:"))
    {
        bail!("pathspec magic not supported by this command: 'attr'");
    }
    Ok(())
}

fn write_plain_record(out: &mut dyn Write, nul_terminated: bool, path: &str) -> Result<()> {
    if nul_terminated {
        out.write_all(path.as_bytes())?;
        out.write_all(b"\0")?;
    } else {
        writeln!(out, "{path}")?;
    }
    Ok(())
}

fn write_verbose_record(
    out: &mut dyn Write,
    nul_terminated: bool,
    source: &str,
    line_number: usize,
    pattern: &str,
    path: &str,
) -> Result<()> {
    if nul_terminated {
        out.write_all(source.as_bytes())?;
        out.write_all(b"\0")?;
        out.write_all(line_number.to_string().as_bytes())?;
        out.write_all(b"\0")?;
        out.write_all(pattern.as_bytes())?;
        out.write_all(b"\0")?;
        out.write_all(path.as_bytes())?;
        out.write_all(b"\0")?;
    } else {
        writeln!(out, "{source}:{line_number}:{pattern}\t{path}")?;
    }
    Ok(())
}

fn write_verbose_non_match(out: &mut dyn Write, nul_terminated: bool, path: &str) -> Result<()> {
    if nul_terminated {
        out.write_all(b"\0\0\0")?;
        out.write_all(path.as_bytes())?;
        out.write_all(b"\0")?;
    } else {
        writeln!(out, "::\t{path}")?;
    }
    Ok(())
}
