//! `grit diff-pairs` — compare pairs of blobs or trees.
//!
//! This command consumes NUL-delimited raw diff records on stdin (as produced
//! by `git diff-tree -z -r --raw`) and emits either raw output (`--raw`) or
//! patch output (`-p`, default).

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::diff::{DiffEntry, DiffStatus};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use std::io::{Read, Write};

/// Arguments for `grit diff-pairs`.
#[derive(Debug, ClapArgs)]
#[command(about = "Compare pairs of blobs/trees read from stdin")]
pub struct Args {
    /// Raw command arguments.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Raw,
    Patch,
}

#[derive(Debug, Clone)]
struct Options {
    nul_terminated: bool,
    output_format: OutputFormat,
    pathspecs: Vec<String>,
}

#[derive(Debug, Clone)]
struct ParsedPair {
    old_mode: String,
    new_mode: String,
    old_oid: ObjectId,
    new_oid: ObjectId,
    status: DiffStatus,
    score: Option<u32>,
    old_path: Option<String>,
    new_path: Option<String>,
}

fn usage(msg: &str) -> ! {
    eprintln!("usage: {msg}");
    std::process::exit(129);
}

fn fatal(msg: &str) -> ! {
    eprintln!("fatal: {msg}");
    std::process::exit(128);
}

fn parse_mode(mode: &str) -> Result<u32> {
    u32::from_str_radix(mode, 8).with_context(|| format!("invalid mode: {mode}"))
}

fn parse_options(args: &[String]) -> Options {
    let mut nul_terminated = false;
    let mut output_format = OutputFormat::Patch;
    let mut pathspecs = Vec::new();
    let mut end_of_options = false;

    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            idx += 1;
            continue;
        }
        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "-z" => nul_terminated = true,
                "--raw" => output_format = OutputFormat::Raw,
                "-p" | "--patch" => output_format = OutputFormat::Patch,
                _ if arg.starts_with("--abbrev") || arg == "--no-abbrev" => {
                    // accepted for compatibility; output remains full-width here
                }
                _ => usage(&format!("unsupported option: {arg}")),
            }
            idx += 1;
            continue;
        }
        pathspecs.push(arg.clone());
        idx += 1;
    }

    Options {
        nul_terminated,
        output_format,
        pathspecs,
    }
}

fn parse_meta(meta: &str) -> Result<(String, String, ObjectId, ObjectId, DiffStatus, Option<u32>)> {
    if !meta.starts_with(':') {
        bail!("invalid raw diff input");
    }
    let mut parts = meta[1..].split_whitespace();
    let old_mode = parts.next().context("missing old mode")?.to_owned();
    let new_mode = parts.next().context("missing new mode")?.to_owned();
    let old_oid =
        ObjectId::from_hex(parts.next().context("missing old oid")?).context("invalid old oid")?;
    let new_oid =
        ObjectId::from_hex(parts.next().context("missing new oid")?).context("invalid new oid")?;
    let status_token = parts.next().context("missing status")?;
    if parts.next().is_some() {
        bail!("invalid raw diff input");
    }

    let old_mode_num = parse_mode(&old_mode)?;
    let new_mode_num = parse_mode(&new_mode)?;
    if (old_mode_num & 0o170000) == 0o040000 || (new_mode_num & 0o170000) == 0o040000 {
        fatal("tree objects not supported");
    }

    let mut status_chars = status_token.chars();
    let status_char = status_chars.next().context("missing status character")?;
    let score_suffix = status_chars.as_str();
    let score = if score_suffix.is_empty() {
        None
    } else {
        Some(
            score_suffix
                .parse::<u32>()
                .with_context(|| format!("invalid score: {score_suffix}"))?,
        )
    };

    let status = match status_char {
        'A' => DiffStatus::Added,
        'D' => DiffStatus::Deleted,
        'M' => DiffStatus::Modified,
        'T' => DiffStatus::TypeChanged,
        'R' => DiffStatus::Renamed,
        'C' => DiffStatus::Copied,
        'U' => DiffStatus::Unmerged,
        _ => bail!("unknown diff status: {status_char}"),
    };

    Ok((old_mode, new_mode, old_oid, new_oid, status, score))
}

fn render_raw_entry(pair: &ParsedPair, out: &mut impl std::io::Write) -> Result<()> {
    let status = match (pair.status, pair.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{s:03}"),
        (DiffStatus::Copied, Some(s)) => format!("C{s:03}"),
        _ => pair.status.letter().to_string(),
    };

    write!(
        out,
        ":{} {} {} {} {}\0",
        pair.old_mode,
        pair.new_mode,
        pair.old_oid.to_hex(),
        pair.new_oid.to_hex(),
        status
    )?;

    match pair.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            write!(
                out,
                "{}\0{}\0",
                pair.old_path.as_deref().unwrap_or_default(),
                pair.new_path.as_deref().unwrap_or_default()
            )?;
        }
        _ => {
            let path = pair
                .new_path
                .as_deref()
                .or(pair.old_path.as_deref())
                .unwrap_or_default();
            write!(out, "{path}\0")?;
        }
    }

    Ok(())
}

fn to_diff_entry(pair: &ParsedPair) -> DiffEntry {
    DiffEntry {
        status: pair.status,
        // TODO(byte-paths): ParsedPair stores lossy String paths; migrate its source to bytes (Phase 2/4).
        old_path: pair
            .old_path
            .clone()
            .map(grit_lib::repo_path::RepoPathBuf::from_string),
        new_path: pair
            .new_path
            .clone()
            .map(grit_lib::repo_path::RepoPathBuf::from_string),
        old_mode: pair.old_mode.clone(),
        new_mode: pair.new_mode.clone(),
        old_oid: pair.old_oid,
        new_oid: pair.new_oid,
        score: pair.score,
    }
}

fn flush_batch(
    pairs: &mut Vec<ParsedPair>,
    out: &mut impl std::io::Write,
    options: &Options,
    repo: &Repository,
) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    match options.output_format {
        OutputFormat::Raw => {
            for pair in pairs.iter() {
                render_raw_entry(pair, out)?;
            }
        }
        OutputFormat::Patch => {
            let entries: Vec<DiffEntry> = pairs.iter().map(to_diff_entry).collect();
            crate::commands::diff::write_patch_from_pairs(out, &entries, repo)?;
        }
    }
    pairs.clear();
    Ok(())
}

fn parse_path_token(token: &[u8]) -> String {
    String::from_utf8_lossy(token).into_owned()
}

fn parse_pairs(input: &[u8], options: &Options, repo: &Repository) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut batch = Vec::new();

    let mut pos = 0usize;
    let len = input.len();

    while pos <= len {
        let Some(next_nul) = input[pos..].iter().position(|b| *b == 0) else {
            break;
        };
        let token_end = pos + next_nul;
        let token = &input[pos..token_end];
        pos = token_end + 1;

        if token.is_empty() {
            if pos < len {
                flush_batch(&mut batch, &mut out, options, repo)?;
                out.push(0);
            }
            continue;
        }

        let meta = String::from_utf8_lossy(token).into_owned();
        let (old_mode, new_mode, old_oid, new_oid, status, score) = parse_meta(&meta)?;

        let read_path = |start: usize| -> Result<(String, usize)> {
            let rel = input[start..]
                .iter()
                .position(|b| *b == 0)
                .context("got EOF while reading path")?;
            let end = start + rel;
            Ok((parse_path_token(&input[start..end]), end + 1))
        };

        let (old_path, new_path, new_pos) = match status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let (src, after_src) = read_path(pos)?;
                let (dst, after_dst) = read_path(after_src)?;
                (Some(src), Some(dst), after_dst)
            }
            DiffStatus::Added => {
                let (path, after) = read_path(pos)?;
                (None, Some(path), after)
            }
            DiffStatus::Deleted => {
                let (path, after) = read_path(pos)?;
                (Some(path), None, after)
            }
            _ => {
                let (path, after) = read_path(pos)?;
                (Some(path.clone()), Some(path), after)
            }
        };

        pos = new_pos;
        batch.push(ParsedPair {
            old_mode,
            new_mode,
            old_oid,
            new_oid,
            status,
            score,
            old_path,
            new_path,
        });
    }

    flush_batch(&mut batch, &mut out, options, repo)?;
    Ok(out)
}

/// Run `grit diff-pairs`.
pub fn run(args: Args) -> Result<()> {
    let options = parse_options(&args.args);
    if !options.nul_terminated {
        usage("working without -z is not supported");
    }
    if !options.pathspecs.is_empty() {
        usage("pathspec arguments not supported");
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;

    let rendered = parse_pairs(&input, &options, &repo).inspect_err(|err| {
        let msg = err.to_string();
        if msg.contains("tree objects not supported") {
            fatal("tree objects not supported");
        }
        if msg.contains("got EOF while reading path") {
            fatal("got EOF while reading path");
        }
        if msg.contains("unknown diff status") {
            fatal(&msg);
        }
        if msg.contains("invalid raw diff input") {
            fatal("invalid raw diff input");
        }
    })?;

    std::io::stdout().write_all(&rendered)?;
    Ok(())
}
