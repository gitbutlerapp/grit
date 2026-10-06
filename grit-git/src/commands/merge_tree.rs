//! `grit merge-tree` — merge two branches without touching index or working tree.
//!
//! Implements `git merge-tree` including `--write-tree`, `--stdin`, `-z`, and related options.

#![allow(clippy::if_same_then_else)]
use anyhow::{bail, Result};
use grit_lib::config::ConfigSet;
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_tree_trivial::trivial_merge_trees_stdout;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{peel_to_tree, resolve_revision};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};

use crate::explicit_exit::ExplicitExit;

use super::merge::{merge_tree_write_tree_core, MergeDirectoryRenamesMode, MergeRenameOptions};

const USAGE_WRITE_TREE: &str =
    "usage: git merge-tree [--write-tree] [<options>] <branch1> <branch2>";
const USAGE_TRIVIAL: &str = "usage: git merge-tree --trivial-merge <base-tree> <branch1> <branch2>";

#[derive(Debug, Default)]
struct Parsed {
    write_tree: bool,
    trivial_merge: bool,
    stdin: bool,
    merge_base: Option<String>,
    allow_unrelated_histories: bool,
    quiet: bool,
    messages: bool,
    no_messages: bool,
    nul_terminate: bool,
    name_only: bool,
    strategy_option: Vec<String>,
    positionals: Vec<String>,
    git_completion_helper: bool,
    git_completion_helper_all: bool,
    /// True if any option token other than `--trivial-merge` was consumed by the
    /// parser. Mirrors git's `argc < original_argc` check: when `--trivial-merge`
    /// is present together with *any* other option, git dies with
    /// "--trivial-merge is incompatible with all other options".
    other_options_seen: bool,
}

/// Entry from `main`: `rest` is argv after `merge-tree`.
/// Options for shell completion (`git merge-tree --git-completion-helper[-all]`).
///
/// This mirrors git's parse-options `show_gitcomp()` for the merge-tree option
/// table: boolean options are emitted without a trailing `=`, only options that
/// take an argument get `=`, and the negated (`--no-*`) forms (plus a literal
/// `--`) are appended at the end. `-z` has no long name and is therefore not
/// listed. `show_all` does not change the result here because merge-tree has no
/// hidden options.
pub fn completion_helper_options(_show_all: bool) -> Vec<String> {
    vec![
        "--write-tree".to_string(),
        "--trivial-merge".to_string(),
        "--messages".to_string(),
        "--quiet".to_string(),
        "--name-only".to_string(),
        "--allow-unrelated-histories".to_string(),
        "--stdin".to_string(),
        "--merge-base=".to_string(),
        "--strategy-option=".to_string(),
        "--no-messages".to_string(),
        "--".to_string(),
        "--no-merge-base".to_string(),
        "--no-strategy-option".to_string(),
    ]
}

pub fn run_from_argv(rest: &[String]) -> Result<()> {
    let parsed = parse_argv(rest)?;
    if parsed.git_completion_helper || parsed.git_completion_helper_all {
        return print_git_completion_helper(parsed.git_completion_helper_all);
    }
    run_parsed(parsed)
}

fn parse_argv(rest: &[String]) -> Result<Parsed> {
    let mut p = Parsed::default();
    let mut i = 0usize;
    while i < rest.len() {
        let tok = rest[i].as_str();
        if tok == "--" || tok == "--end-of-options" {
            // git's parse-options consumes the `--` token, so when combined with
            // `--trivial-merge` the remaining-arg count shrinks and git reports
            // the incompatibility error. Record that an option token was seen.
            p.other_options_seen = true;
            i += 1;
            while i < rest.len() {
                p.positionals.push(rest[i].clone());
                i += 1;
            }
            break;
        }
        if !tok.starts_with('-') {
            p.positionals.push(rest[i].clone());
            i += 1;
            continue;
        }
        if tok == "-z" {
            p.nul_terminate = true;
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        if tok == "-X" {
            i += 1;
            let opt = rest
                .get(i)
                .ok_or_else(|| {
                    anyhow::Error::new(ExplicitExit {
                        code: 129,
                        message: USAGE_WRITE_TREE.to_string(),
                    })
                })?
                .clone();
            if opt.is_empty() {
                return Err(anyhow::Error::new(ExplicitExit {
                    code: 129,
                    message: USAGE_WRITE_TREE.to_string(),
                }));
            }
            p.strategy_option.push(opt);
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        if tok.starts_with("-X") && tok.len() > 2 {
            p.strategy_option.push(tok[2..].to_string());
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        if let Some(v) = tok.strip_prefix("--strategy-option=") {
            p.strategy_option.push(v.to_string());
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        if tok == "--strategy-option" {
            i += 1;
            let v = rest
                .get(i)
                .ok_or_else(|| {
                    anyhow::Error::new(ExplicitExit {
                        code: 129,
                        message: USAGE_WRITE_TREE.to_string(),
                    })
                })?
                .clone();
            p.strategy_option.push(v);
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        // `--no-strategy-option` clears any accumulated strategy options (it does
        // not take a value); it is emitted by the completion helper.
        if tok == "--no-strategy-option" {
            p.strategy_option.clear();
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        if let Some(v) = tok.strip_prefix("--merge-base=") {
            p.merge_base = Some(v.to_string());
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        // `--no-merge-base` unsets the merge base (no value).
        if tok == "--no-merge-base" {
            p.merge_base = None;
            p.other_options_seen = true;
            i += 1;
            continue;
        }
        match tok {
            "--write-tree" => {
                p.write_tree = true;
                p.other_options_seen = true;
            }
            "--trivial-merge" => p.trivial_merge = true,
            "--stdin" => {
                p.stdin = true;
                p.other_options_seen = true;
            }
            "--merge-base" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| {
                        anyhow::Error::new(ExplicitExit {
                            code: 129,
                            message: USAGE_WRITE_TREE.to_string(),
                        })
                    })?
                    .clone();
                p.merge_base = Some(v);
                p.other_options_seen = true;
            }
            "--allow-unrelated-histories" => {
                p.allow_unrelated_histories = true;
                p.other_options_seen = true;
            }
            "--quiet" => {
                p.quiet = true;
                p.other_options_seen = true;
            }
            "--messages" => {
                p.messages = true;
                p.other_options_seen = true;
            }
            "--no-messages" => {
                p.no_messages = true;
                p.other_options_seen = true;
            }
            "--name-only" => {
                p.name_only = true;
                p.other_options_seen = true;
            }
            "--git-completion-helper" => p.git_completion_helper = true,
            "--git-completion-helper-all" => p.git_completion_helper_all = true,
            other => {
                if let Some(long) = other.strip_prefix("--") {
                    if long.is_empty() {
                        return Err(anyhow::Error::new(ExplicitExit {
                            code: 129,
                            message: USAGE_WRITE_TREE.to_string(),
                        }));
                    }
                    return Err(anyhow::Error::new(ExplicitExit {
                        code: 129,
                        message: format!("error: unknown option `{other}`\n{USAGE_WRITE_TREE}"),
                    }));
                }
                return Err(anyhow::Error::new(ExplicitExit {
                    code: 129,
                    message: format!("error: unknown switch `{other}`\n{USAGE_WRITE_TREE}"),
                }));
            }
        }
        i += 1;
    }
    Ok(p)
}

fn print_git_completion_helper(show_all: bool) -> Result<()> {
    println!("{}", completion_helper_options(show_all).join(" "));
    Ok(())
}

fn run_parsed(args: Parsed) -> Result<()> {
    if args.trivial_merge {
        return run_trivial_merge(&args);
    }

    if args.stdin {
        if args.merge_base.is_some() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "fatal: options '--merge-base' and '--stdin' cannot be used together"
                    .to_string(),
            }));
        }
        return run_stdin_merges(&args);
    }

    if args.quiet {
        if args.messages {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "fatal: options '--quiet' and '--messages' cannot be used together"
                    .to_string(),
            }));
        }
        if args.name_only {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "fatal: options '--quiet' and '--name-only' cannot be used together"
                    .to_string(),
            }));
        }
        if args.nul_terminate {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "fatal: options '--quiet' and '-z' cannot be used together".to_string(),
            }));
        }
    }

    let repo = Repository::discover(None)?;

    if args.positionals.len() == 3
        && !args.write_tree
        && !args.stdin
        && !args.trivial_merge
        && args.merge_base.is_none()
        && args.strategy_option.is_empty()
    {
        let base_s = &args.positionals[0];
        let ours_s = &args.positionals[1];
        let theirs_s = &args.positionals[2];
        let base_oid = resolve_merge_tree_revision(&repo, base_s)?;
        let ours_oid = resolve_merge_tree_revision(&repo, ours_s)?;
        let theirs_oid = resolve_merge_tree_revision(&repo, theirs_s)?;
        let base_tree = peel_to_tree(&repo, base_oid)?;
        let ours_tree = peel_to_tree(&repo, ours_oid)?;
        let theirs_tree = peel_to_tree(&repo, theirs_oid)?;
        let text = trivial_merge_trees_stdout(&repo, base_tree, ours_tree, theirs_tree)?;
        if !args.quiet {
            print!("{text}");
        }
        return Ok(());
    }

    let b1 = args.positionals.first().cloned().ok_or_else(|| {
        anyhow::Error::new(ExplicitExit {
            code: 129,
            message: USAGE_WRITE_TREE.to_string(),
        })
    })?;
    let b2 = args.positionals.get(1).cloned().ok_or_else(|| {
        anyhow::Error::new(ExplicitExit {
            code: 129,
            message: USAGE_WRITE_TREE.to_string(),
        })
    })?;
    if args.positionals.len() > 2 {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: USAGE_WRITE_TREE.to_string(),
        }));
    }
    let favor = parse_strategy_options(&args.strategy_option)?;
    let rename_opts = MergeRenameOptions::from_config(&repo);
    let merge_base_oid = args
        .merge_base
        .as_deref()
        .map(|s| resolve_merge_tree_revision(&repo, s))
        .transpose()?;

    let oid1 = resolve_merge_tree_revision(&repo, &b1)?;
    let oid2 = resolve_merge_tree_revision(&repo, &b2)?;

    let out = merge_tree_write_tree_core(
        &repo,
        oid1,
        oid2,
        merge_base_oid,
        &b1,
        &b2,
        args.allow_unrelated_histories,
        favor,
        None,
        merge_renormalize(&repo)?,
        MergeDirectoryRenamesMode::FromConfig,
        rename_opts,
        args.quiet,
        !args.quiet,
    )?;

    let show_messages = resolve_show_messages_after_merge(
        args.messages,
        args.no_messages,
        args.quiet,
        out.has_conflicts,
    );

    write_merge_tree_stdout(
        &repo,
        &out,
        show_messages,
        args.name_only,
        args.nul_terminate,
        false,
        args.quiet,
    )?;

    if out.has_conflicts {
        // Do not use `process::exit`: the test harness often `exec`s grit as `git`, so exiting
        // here would terminate the whole test shell (FATAL: Unexpected exit with code 1).
        return Err(anyhow::Error::new(ExplicitExit {
            code: 1,
            message: String::new(),
        }));
    }
    Ok(())
}

fn run_stdin_merges(args: &Parsed) -> Result<()> {
    let repo = Repository::discover(None)?;
    let favor = parse_strategy_options(&args.strategy_option)?;
    let rename_opts = MergeRenameOptions::from_config(&repo);
    let renormalize = merge_renormalize(&repo)?;
    let stdin = io::stdin();
    let mut line = String::new();
    let mut stdout = io::stdout().lock();
    while stdin.read_line(&mut line)? > 0 {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let (mb, left, right) = if parts.len() >= 4 && parts[1] == "--" {
            (
                Some(parts[0]),
                parts[2],
                parts
                    .get(3)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("malformed input line: '{trimmed}'."))?,
            )
        } else if parts.len() == 2 {
            (None, parts[0], parts[1])
        } else {
            bail!("malformed input line: '{trimmed}'.");
        };

        let merge_base_oid = mb
            .map(|s| resolve_merge_tree_revision(&repo, s))
            .transpose()?;
        let oid1 = resolve_merge_tree_revision(&repo, left)?;
        let oid2 = resolve_merge_tree_revision(&repo, right)?;

        let out = merge_tree_write_tree_core(
            &repo,
            oid1,
            oid2,
            merge_base_oid,
            left,
            right,
            args.allow_unrelated_histories,
            favor,
            None,
            renormalize,
            MergeDirectoryRenamesMode::FromConfig,
            rename_opts,
            args.quiet,
            !args.quiet,
        )?;

        let show_messages = resolve_show_messages_after_merge(
            args.messages,
            args.no_messages,
            args.quiet,
            out.has_conflicts,
        );

        let clean = !out.has_conflicts;
        write!(stdout, "{}\0", u8::from(clean))?;
        write_merge_tree_stdout_to(
            &mut stdout,
            &repo,
            &out,
            show_messages,
            args.name_only,
            true,
            true,
            args.quiet,
        )?;
        write!(stdout, "\0")?;
        line.clear();
    }
    Ok(())
}

fn run_trivial_merge(args: &Parsed) -> Result<()> {
    // git: `--trivial-merge` is incompatible with *any* other option token,
    // including ones that were unset (`--no-merge-base`) or `--` itself. The
    // `other_options_seen` flag captures git's `argc < original_argc` behavior.
    if args.other_options_seen {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: --trivial-merge is incompatible with all other options".to_string(),
        }));
    }
    let _base = args.positionals.first().ok_or_else(|| {
        anyhow::Error::new(ExplicitExit {
            code: 129,
            message: USAGE_TRIVIAL.to_string(),
        })
    })?;
    Err(anyhow::Error::new(ExplicitExit {
        code: 128,
        message: "fatal: merge-tree --trivial-merge is not implemented yet".to_string(),
    }))
}

fn resolve_merge_tree_revision(repo: &Repository, spec: &str) -> Result<ObjectId> {
    if spec == "AUTO_MERGE" {
        let raw = fs::read_to_string(repo.git_dir.join("AUTO_MERGE"))
            .map_err(|e| anyhow::anyhow!("failed to read AUTO_MERGE: {e}"))?;
        let line = raw.lines().next().unwrap_or("").trim();
        return line
            .parse::<ObjectId>()
            .map_err(|_| anyhow::anyhow!("AUTO_MERGE did not contain a valid object id"));
    }
    resolve_revision(repo, spec).map_err(|e| anyhow::anyhow!("{e}"))
}

fn merge_renormalize(repo: &Repository) -> Result<bool> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    Ok(cfg
        .get_bool("merge.renormalize")
        .and_then(|r| r.ok())
        .unwrap_or(false))
}

fn parse_strategy_options(opts: &[String]) -> Result<MergeFavor> {
    let mut favor = MergeFavor::None;
    for o in opts {
        let o = o.trim();
        if o.is_empty() {
            continue;
        }
        let (k, v) = o
            .split_once('=')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((o, None));
        match k {
            "ours" if v.is_none() || v == Some("") => favor = MergeFavor::Ours,
            "theirs" if v.is_none() || v == Some("") => favor = MergeFavor::Theirs,
            _ => {
                return Err(anyhow::Error::new(ExplicitExit {
                    code: 128,
                    message: format!("fatal: unknown strategy option: -X{k}"),
                }));
            }
        }
    }
    Ok(favor)
}

fn resolve_show_messages_after_merge(
    messages: bool,
    no_messages: bool,
    quiet: bool,
    has_conflicts: bool,
) -> bool {
    if quiet {
        return false;
    }
    if no_messages {
        return false;
    }
    if messages {
        return true;
    }
    has_conflicts
}

fn write_merge_tree_stdout(
    repo: &Repository,
    out: &super::merge::MergeTreeWriteOutput,
    show_messages: bool,
    name_only: bool,
    nul: bool,
    stdin_batch_inner: bool,
    quiet: bool,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write_merge_tree_stdout_to(
        &mut stdout,
        repo,
        out,
        show_messages,
        name_only,
        nul,
        stdin_batch_inner,
        quiet,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_merge_tree_stdout_to(
    w: &mut dyn Write,
    repo: &Repository,
    out: &super::merge::MergeTreeWriteOutput,
    show_messages: bool,
    name_only: bool,
    nul: bool,
    stdin_batch_inner: bool,
    quiet: bool,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    let line_term = if nul { b'\0' } else { b'\n' };
    let tree_line = out
        .tree_oid
        .map(|o| o.to_hex())
        .unwrap_or_else(|| "0".repeat(40));
    write!(w, "{tree_line}")?;
    w.write_all(&[line_term])?;

    if out.has_conflicts {
        // With NUL line termination (`-z`/`--stdin`), git never C-quotes paths — the NUL
        // delimiter already makes any byte safe, so non-ASCII names appear verbatim.
        let quote = !nul && config_quote_path(repo);
        let mut seen_name_only: Option<String> = None;
        for e in &out.index.entries {
            if e.stage() == 0 || e.mode == grit_lib::index::MODE_TREE {
                continue;
            }
            let path_str = String::from_utf8_lossy(&e.path).into_owned();
            if name_only {
                if seen_name_only.as_deref() == Some(&path_str) {
                    continue;
                }
                seen_name_only = Some(path_str.clone());
                if nul {
                    write!(w, "{}", format_path_maybe_quote(&path_str, quote))?;
                    w.write_all(b"\0")?;
                } else {
                    writeln!(w, "{}", format_path_maybe_quote(&path_str, quote))?;
                }
                continue;
            }
            let line = format!(
                "{:06o} {} {}\t{}",
                e.mode,
                e.oid.to_hex(),
                e.stage(),
                format_path_maybe_quote(&path_str, quote)
            );
            if nul {
                write!(w, "{line}")?;
                w.write_all(b"\0")?;
            } else {
                writeln!(w, "{line}")?;
            }
        }
    }

    if show_messages {
        if !stdin_batch_inner
            && !nul
            && (!out.auto_merge_paths.is_empty() || !out.conflict_descriptions.is_empty())
        {
            writeln!(w)?;
        }

        if nul {
            #[derive(Clone)]
            enum ZMsg<'a> {
                Auto(&'a str),
                DirRename {
                    new: &'a str,
                    old: &'a str,
                    body: &'a str,
                },
                OtherConflict {
                    paths: Vec<&'a str>,
                    short: String,
                    long: String,
                },
            }

            // `(subject_path, tier, tie_breaker)` — `tie_breaker` disambiguates multiple
            // `rename/delete` rows that share the same destination (t4301 rrdd).
            let mut zrows: Vec<(String, u8, String, ZMsg<'_>)> = Vec::new();
            let distinct_mode_conflict_paths: HashSet<String> = out
                .conflict_descriptions
                .iter()
                .filter(|d| d.kind == "distinct modes")
                .filter_map(|d| d.remerge_anchor_path.clone())
                .collect();
            let mut auto_paths: Vec<String> = out.auto_merge_paths.clone();
            auto_paths.sort();
            auto_paths.dedup();
            let mut auto_seen: HashSet<String> = HashSet::new();
            for p in &auto_paths {
                if distinct_mode_conflict_paths.contains(p) {
                    continue;
                }
                if auto_seen.insert(p.clone()) {
                    zrows.push((p.clone(), 1, String::new(), ZMsg::Auto(p.as_str())));
                }
            }
            for d in &out.conflict_descriptions {
                if d.kind == "distinct modes" {
                    if let Some(h) = d.auto_merge_hint_path.as_deref() {
                        if auto_seen.insert(h.to_string()) {
                            zrows.push((h.to_string(), 1, String::new(), ZMsg::Auto(h)));
                        }
                    }
                }
            }
            for d in &out.conflict_descriptions {
                if d.kind == "rename/rename" {
                    if let Some(src) = d.auto_merge_hint_path.as_deref() {
                        if auto_seen.insert(src.to_string()) {
                            // Tier 0 so this sorts before the conflict row (subject_path = theirs'
                            // destination), matching Git's merge-tree -z ordering.
                            zrows.push((src.to_string(), 0, String::new(), ZMsg::Auto(src)));
                        }
                    }
                }
            }
            for d in &out.conflict_descriptions {
                if d.kind == "rename/add" {
                    if let Some(src) = d.remerge_anchor_path.as_deref() {
                        if src != d.subject_path.as_str() {
                            let paired_rename_delete = out.conflict_descriptions.iter().any(|x| {
                                x.kind == "rename/delete"
                                    && x.remerge_anchor_path.as_deref() == Some(src)
                                    && x.subject_path == d.subject_path
                            });
                            if !paired_rename_delete && auto_seen.insert(src.to_string()) {
                                zrows.push((src.to_string(), 0, String::new(), ZMsg::Auto(src)));
                            }
                        }
                    }
                }
                if d.kind == "binary" {
                    let subj = d.subject_path.as_str();
                    zrows.push((
                        subj.to_string(),
                        2,
                        String::new(),
                        ZMsg::OtherConflict {
                            paths: vec![subj],
                            short: "CONFLICT (binary)".to_string(),
                            long: format!("CONFLICT (binary): {}", d.body),
                        },
                    ));
                    continue;
                }
                if d.kind == "directory rename suggested" {
                    if let (Some(old), new) =
                        (d.remerge_anchor_path.as_deref(), d.subject_path.as_str())
                    {
                        zrows.push((
                            new.to_string(),
                            0,
                            String::new(),
                            ZMsg::DirRename {
                                new,
                                old,
                                body: d.body.as_str(),
                            },
                        ));
                    }
                    continue;
                }
                if d.kind == "distinct modes" {
                    if let (Some(conflict_path), side_path) =
                        (d.remerge_anchor_path.as_deref(), d.subject_path.as_str())
                    {
                        let long = format!("CONFLICT (distinct types): {}", d.body);
                        zrows.push((
                            conflict_path.to_string(),
                            0,
                            String::new(),
                            ZMsg::OtherConflict {
                                paths: vec![conflict_path, side_path],
                                short: "CONFLICT (distinct modes)".to_string(),
                                long,
                            },
                        ));
                    }
                    continue;
                }
                if d.kind == "rename/rename" {
                    if let (Some(base), Some(ours_d), Some(theirs_d)) = (
                        d.remerge_anchor_path.as_deref(),
                        d.rename_rr_ours_dest.as_deref(),
                        d.rename_rr_theirs_dest.as_deref(),
                    ) {
                        let short = "CONFLICT (rename/rename)".to_string();
                        let long = format!("{short}: {}", d.body);
                        zrows.push((
                            base.to_string(),
                            3,
                            String::new(),
                            ZMsg::OtherConflict {
                                paths: vec![base, ours_d, theirs_d],
                                short,
                                long,
                            },
                        ));
                    }
                    continue;
                }
                if d.kind == "file/directory" {
                    // git lists the relocated path and the original directory path, and orders the
                    // `file/directory` row before the companion `modify/delete` row at the same
                    // relocated path.
                    let new = d.subject_path.as_str();
                    let short = "CONFLICT (file/directory)".to_string();
                    let long = format!("{short}: {}", d.body);
                    if let Some(old) = d.remerge_anchor_path.as_deref() {
                        zrows.push((
                            new.to_string(),
                            0,
                            String::new(),
                            ZMsg::OtherConflict {
                                paths: vec![new, old],
                                short,
                                long,
                            },
                        ));
                    } else {
                        zrows.push((
                            new.to_string(),
                            0,
                            String::new(),
                            ZMsg::OtherConflict {
                                paths: vec![new],
                                short,
                                long,
                            },
                        ));
                    }
                    continue;
                }
                let tier = match d.kind {
                    "rename/delete" => 0u8,
                    "modify/delete" => 1,
                    "rename/add" => 2,
                    _ => 3,
                };
                let (short, long, paths): (String, String, Vec<&str>) = if d.kind == "content" {
                    (
                        "CONFLICT (contents)".to_string(),
                        format!("CONFLICT (content): {}", d.body),
                        vec![d
                            .remerge_anchor_path
                            .as_deref()
                            .unwrap_or(d.subject_path.as_str())],
                    )
                } else if d.kind == "rename/add" || d.kind == "add/add" {
                    (
                        "CONFLICT (contents)".to_string(),
                        format!("CONFLICT (add/add): {}", d.body),
                        vec![d.subject_path.as_str()],
                    )
                } else if d.kind == "rename/delete" {
                    let s = format!("CONFLICT ({})", d.kind);
                    let l = format!("{s}: {}", d.body);
                    if let Some(src) = d.remerge_anchor_path.as_deref() {
                        (s, l, vec![d.subject_path.as_str(), src])
                    } else {
                        (s, l, vec![d.subject_path.as_str()])
                    }
                } else if d.kind == "modify/delete" {
                    // git always lists the single (possibly relocated) subject path for
                    // modify/delete, even when an anchor to the original path is recorded.
                    let s = format!("CONFLICT ({})", d.kind);
                    let l = format!("{s}: {}", d.body);
                    (s, l, vec![d.subject_path.as_str()])
                } else {
                    let s = format!("CONFLICT ({})", d.kind);
                    let l = format!("{s}: {}", d.body);
                    let anchor = d
                        .remerge_anchor_path
                        .as_deref()
                        .unwrap_or(d.subject_path.as_str());
                    (s, l, vec![anchor])
                };
                let sort_key = d.subject_path.clone();
                let tie = if d.kind == "rename/delete" {
                    d.remerge_anchor_path.clone().unwrap_or_default()
                } else {
                    String::new()
                };
                zrows.push((
                    sort_key,
                    tier,
                    tie,
                    ZMsg::OtherConflict { paths, short, long },
                ));
            }
            zrows.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.cmp(&b.1))
                    .then_with(|| a.2.cmp(&b.2))
            });

            for (_, _, _, m) in zrows {
                match m {
                    ZMsg::Auto(p) => {
                        write_z_message_record(
                            w,
                            &[p],
                            "Auto-merging",
                            &format!("Auto-merging {p}"),
                        )?;
                    }
                    ZMsg::DirRename { new, old, body } => {
                        write_z_message_record(
                            w,
                            &[new, old],
                            "CONFLICT (directory rename suggested)",
                            body,
                        )?;
                    }
                    ZMsg::OtherConflict { paths, short, long } => {
                        write_z_message_record(w, &paths, &short, &long)?;
                    }
                }
                w.write_all(b"\n")?;
            }
            // Trailing NUL matches the final `Q` line in upstream t4301 heredocs.
            w.write_all(b"\0")?;
        } else {
            let mut auto_paths: Vec<String> = out.auto_merge_paths.clone();
            auto_paths.sort();
            auto_paths.dedup();

            #[derive(Clone)]
            enum Msg {
                Auto(String),
                BinaryWarn(String),
                BinaryErr(String),
                Conflict(String),
            }

            let mut rows: Vec<(String, u8, Msg)> = Vec::new();
            let mut auto_seen_nl: HashSet<String> = HashSet::new();
            for p in &auto_paths {
                if auto_seen_nl.insert(p.clone()) {
                    rows.push((p.clone(), 1, Msg::Auto(p.clone())));
                }
            }
            for d in &out.conflict_descriptions {
                if d.kind == "rename/rename" {
                    if let Some(h) = d.auto_merge_hint_path.as_deref() {
                        if auto_seen_nl.insert(h.to_string()) {
                            rows.push((h.to_string(), 1, Msg::Auto(h.to_string())));
                        }
                    }
                }
            }
            for d in &out.conflict_descriptions {
                if d.kind == "binary" {
                    rows.push((
                        d.subject_path.clone(),
                        2,
                        Msg::BinaryWarn(d.subject_path.clone()),
                    ));
                    rows.push((
                        d.subject_path.clone(),
                        3,
                        Msg::BinaryErr(d.subject_path.clone()),
                    ));
                    continue;
                }
                let (sort_path, tier) = if d.kind == "directory rename suggested" {
                    (d.subject_path.clone(), 0u8)
                } else if d.kind == "rename/delete" {
                    (d.subject_path.clone(), 0u8)
                } else if d.kind == "modify/delete" {
                    (d.subject_path.clone(), 1u8)
                } else {
                    (
                        d.remerge_anchor_path
                            .clone()
                            .unwrap_or_else(|| d.subject_path.clone()),
                        2u8,
                    )
                };
                let short = format!("CONFLICT ({})", d.kind);
                let long = format!("{short}: {}", d.body);
                rows.push((sort_path, tier, Msg::Conflict(long)));
            }
            rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

            for (_, _, m) in rows {
                match m {
                    Msg::Auto(p) => writeln!(w, "Auto-merging {p}")?,
                    Msg::BinaryWarn(s) => writeln!(w, "warning: Cannot merge binary files: {s}")?,
                    Msg::BinaryErr(s) => writeln!(w, "Cannot merge binary files: {s}")?,
                    Msg::Conflict(line) => writeln!(w, "{line}")?,
                }
            }
        }
    }

    Ok(())
}

fn config_quote_path(repo: &Repository) -> bool {
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get_bool("core.quotePath"))
        .and_then(|r| r.ok())
        .unwrap_or(true)
}

fn format_path_maybe_quote(path: &str, quote: bool) -> String {
    if !quote {
        return path.to_string();
    }
    maybe_quote_c_path(path)
}

fn maybe_quote_c_path(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    let mut needs_quotes = false;
    for ch in name.chars() {
        match ch {
            '"' => {
                out.push_str("\\\"");
                needs_quotes = true;
            }
            '\\' => {
                out.push_str("\\\\");
                needs_quotes = true;
            }
            '\t' => {
                out.push_str("\\t");
                needs_quotes = true;
            }
            '\n' => {
                out.push_str("\\n");
                needs_quotes = true;
            }
            '\r' => {
                out.push_str("\\r");
                needs_quotes = true;
            }
            c if c.is_control() || (c as u32) >= 0x80 => {
                for b in c.to_string().bytes() {
                    out.push_str(&format!("\\{:03o}", b));
                }
                needs_quotes = true;
            }
            c => out.push(c),
        }
    }
    if needs_quotes {
        format!("\"{out}\"")
    } else {
        out
    }
}

fn write_z_message_record(
    w: &mut dyn Write,
    paths: &[&str],
    short_type: &str,
    long_message: &str,
) -> Result<()> {
    // Match Git's `merge-tree -z` framing: each logical line in the heredoc begins with `Q`
    // (NUL) before the path-count digit.
    w.write_all(b"\0")?;
    write!(w, "{}", paths.len())?;
    w.write_all(b"\0")?;
    for p in paths {
        write!(w, "{p}")?;
        w.write_all(b"\0")?;
    }
    write!(w, "{short_type}")?;
    w.write_all(b"\0")?;
    write!(w, "{long_message}")?;
    Ok(())
}
