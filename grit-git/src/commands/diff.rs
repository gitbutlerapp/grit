//! `grit diff` — show changes between commits, commit and working tree, etc.
//!
//! Modes:
//! - No revisions: working tree vs index (unstaged changes)
//! - `--cached [<commit>]`: index vs HEAD (or specified commit) — staged changes
//! - `<commit>`: commit's tree vs working tree (combined view)
//! - `<commit> <commit>`: commit-to-commit diff
//!
//! Output formats: unified patch (default), `--stat`, `--shortstat`,
//! `--numstat`, `--name-only`, `--name-status`.
//!
//! Exit codes: `--exit-code` / `--quiet` return exit code 1 if there are
//! differences.

use crate::explicit_exit::ExplicitExit;
use crate::pathspec::resolve_pathspec;
use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::attributes::{
    collect_attrs_for_path, load_gitattributes_for_diff, load_gitattributes_from_index, AttrValue,
    ParsedGitAttributes,
};
use grit_lib::combined_diff_patch::CombinedDiffWsOptions;
use grit_lib::combined_tree_diff::{
    combined_diff_paths_trees, format_combined_raw_line, CombinedTreeDiffOptions,
};
use grit_lib::config::ConfigSet;
use grit_lib::crlf::{get_file_attrs, parse_gitattributes_content, DiffAttr};
use grit_lib::diff::{
    anchored_unified_diff, count_changes, count_changes_with_algorithm, count_git_lines,
    detect_copies, detect_renames, diff_index_to_tree, diff_index_to_worktree,
    diff_slice_ops_compacted, diff_tree_to_worktree, diff_trees, diffcore_count_changes,
    empty_blob_oid, format_stat_line, resolve_indent_heuristic,
    rewrite_dissimilarity_index_percent, should_break_rewrite_for_stat,
    unified_diff_histogram_hunks_only, unified_diff_with_prefix,
    unified_diff_with_prefix_and_funcname_and_algorithm, word_diff_ops_imara, zero_oid, DiffEntry,
    DiffStatus,
};
use grit_lib::diffstat::{terminal_columns, write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::repo_path::{RepoPath, RepoPathBuf};
use grit_lib::error::Error;
use grit_lib::index::{Index, MODE_GITLINK};
use grit_lib::merge_base::{
    merge_base_for_diff_index, merge_base_for_diff_two_commits, MergeBaseForDiffError,
};
use grit_lib::merge_diff::{
    blob_text_for_diff_with_oid, combined_diff_paths, combined_merge_parent_blob_paths,
    diff_forced_binary_by_driver, diff_textconv_active, format_combined_textconv_patch,
    format_gitlink_unmerged_conflict_combined, format_worktree_conflict_combined, run_textconv,
};
use grit_lib::objects::{parse_commit, parse_tag, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::quote_path::{format_diff_path_with_prefix, quote_c_style};
use grit_lib::repo::{resolve_dot_git, Repository};
use grit_lib::rev_list::{is_symmetric_diff, rev_list, RevListOptions};
use grit_lib::rev_parse::{
    abbreviate_object_id, expand_rev_token_circ_bang, resolve_revision, resolve_revision_as_commit,
    resolve_treeish_blob_at_path, show_prefix, split_treeish_colon, TreeishBlobAtPath,
};
use grit_lib::userdiff::{matcher_for_path_parsed, word_regex_pattern_for_path_parsed};
use grit_lib::ws::{self, WhitespaceGitAttr, WS_BLANK_AT_EOF, WS_INCOMPLETE_LINE};
use regex::Regex;
use regex::RegexBuilder;
use similar::{group_diff_ops, udiff::UnifiedDiffHunk, ChangeTag, TextDiff};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::sync::Arc;

use crate::commands::diff_index::{write_patch_entry, SubmoduleIgnoreFlags, SubmodulePatchFormat};
use crate::commands::promisor_hydrate::{prefetch_promisor_for_diff_entries, PromisorDiffPrefetch};
use grit_lib::config::{canonical_key, ConfigFile, ConfigScope};

/// Shared gitattributes + config for per-path diff algorithm selection (`diff.<driver>.algorithm`).
#[derive(Clone)]
struct DiffAlgoContext {
    attrs: Arc<grit_lib::attributes::ParsedGitAttributes>,
    config: Arc<grit_lib::config::ConfigSet>,
    ignore_case_attrs: bool,
}

/// Map a Git `diff.algorithm` / driver algorithm string to a line diff backend.
///
/// Histogram uses `imara-diff` for Git-compatible hunks; patience/minimal/myers use `similar`.
fn match_grit_diff_algorithm_name(name: &str) -> Option<(similar::Algorithm, bool)> {
    match name.to_ascii_lowercase().as_str() {
        "myers" | "default" => Some((similar::Algorithm::Myers, false)),
        "histogram" => Some((similar::Algorithm::Patience, true)),
        "patience" => Some((similar::Algorithm::Patience, false)),
        "minimal" => Some((similar::Algorithm::Lcs, false)),
        _ => None,
    }
}

fn diff_algo_from_config_default(cfg: &grit_lib::config::ConfigSet) -> (similar::Algorithm, bool) {
    cfg.get("diff.algorithm")
        .as_deref()
        .and_then(match_grit_diff_algorithm_name)
        .unwrap_or((similar::Algorithm::Myers, false))
}

/// Last algorithm-related flag on the argv wins (matches Git).
#[derive(Clone, Copy, Debug, Default)]
struct CliDiffAlgo {
    similar: similar::Algorithm,
    histogram_git: bool,
}

fn parse_cli_diff_algorithm_from_argv() -> Option<CliDiffAlgo> {
    let argv: Vec<String> = std::env::args().collect();
    let mut last: Option<CliDiffAlgo> = None;
    let mut i = 0usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--histogram" => {
                last = Some(CliDiffAlgo {
                    similar: similar::Algorithm::Patience,
                    histogram_git: true,
                });
            }
            "--patience" => {
                last = Some(CliDiffAlgo {
                    similar: similar::Algorithm::Patience,
                    histogram_git: false,
                });
            }
            "--minimal" => {
                last = Some(CliDiffAlgo {
                    similar: similar::Algorithm::Lcs,
                    histogram_git: false,
                });
            }
            s if s.starts_with("--diff-algorithm=") => {
                let v = s.strip_prefix("--diff-algorithm=").unwrap_or("");
                if let Some((algo, hg)) = match_grit_diff_algorithm_name(v) {
                    last = Some(CliDiffAlgo {
                        similar: algo,
                        histogram_git: hg,
                    });
                }
            }
            "--diff-algorithm" => {
                if let Some(v) = argv.get(i + 1) {
                    if let Some((algo, hg)) = match_grit_diff_algorithm_name(v) {
                        last = Some(CliDiffAlgo {
                            similar: algo,
                            histogram_git: hg,
                        });
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    last
}

fn diff_algorithm_for_path(
    rel_path: &str,
    cli_override: Option<CliDiffAlgo>,
    ctx: &DiffAlgoContext,
) -> (similar::Algorithm, bool) {
    if let Some(c) = cli_override {
        return (c.similar, c.histogram_git);
    }
    let map = collect_attrs_for_path(
        &ctx.attrs.rules,
        &ctx.attrs.macros,
        rel_path,
        ctx.ignore_case_attrs,
    );
    if let Some(AttrValue::Value(driver)) = map.get("diff") {
        if let Some(algo_key) = ctx.config.get(&format!("diff.{driver}.algorithm")) {
            if let Some((a, hg)) = match_grit_diff_algorithm_name(&algo_key) {
                return (a, hg);
            }
        }
    }
    diff_algo_from_config_default(&ctx.config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordDiffModeCli {
    None,
    Plain,
    Color,
    Porcelain,
}

#[derive(Clone, Debug)]
struct EffectiveWordDiff {
    mode: WordDiffModeCli,
    /// When set, overrides `diff.wordRegex` / driver word regex for this invocation.
    regex_override: Option<String>,
    /// Force word-diff coloring (`--word-diff=color`, `--color-words`, or `--color`).
    force_color: bool,
}

fn parse_word_diff_from_argv() -> (WordDiffModeCli, Option<String>) {
    let argv: Vec<String> = std::env::args().collect();
    let mut mode = WordDiffModeCli::None;
    let mut regex_override: Option<String> = None;
    let mut i = 0usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--word-diff" {
            if i + 1 < argv.len() {
                let n = argv[i + 1].as_str();
                if !n.starts_with('-') {
                    match n {
                        "plain" => mode = WordDiffModeCli::Plain,
                        "color" => mode = WordDiffModeCli::Color,
                        "porcelain" => mode = WordDiffModeCli::Porcelain,
                        "none" => mode = WordDiffModeCli::None,
                        _ => {}
                    }
                    i += 2;
                    continue;
                }
            }
            if mode == WordDiffModeCli::None {
                mode = WordDiffModeCli::Plain;
            }
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("--word-diff=") {
            match v {
                "plain" => mode = WordDiffModeCli::Plain,
                "color" => mode = WordDiffModeCli::Color,
                "porcelain" => mode = WordDiffModeCli::Porcelain,
                "none" => mode = WordDiffModeCli::None,
                _ => {}
            }
            i += 1;
            continue;
        }
        if a == "--word-diff-regex" {
            if i + 1 < argv.len() {
                regex_override = Some(argv[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(v) = a.strip_prefix("--word-diff-regex=") {
            regex_override = Some(v.to_owned());
            i += 1;
            continue;
        }
        // Git only takes a regex from `--color-words=<pat>`; a bare `--color-words` does not
        // consume the next argv (which is usually a path). See t4034-diff-words.
        if a == "--color-words" {
            mode = WordDiffModeCli::Color;
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("--color-words=") {
            if !v.is_empty() {
                regex_override = Some(v.to_owned());
            }
            mode = WordDiffModeCli::Color;
            i += 1;
            continue;
        }
        i += 1;
    }
    (mode, regex_override)
}

fn effective_word_diff(args: &Args, stdout_is_tty: bool) -> Option<EffectiveWordDiff> {
    let (mut mode, regex_override) = parse_word_diff_from_argv();
    // git: `--word-diff-regex=<re>` implies word-diff in plain mode unless a mode is
    // given explicitly (see Documentation/diff-options.txt and t4034 subtest 15).
    if args.word_diff_regex.is_some() && mode == WordDiffModeCli::None && args.word_diff.is_none() {
        mode = WordDiffModeCli::Plain;
    }
    if args.color_words.is_some() {
        mode = WordDiffModeCli::Color;
    }
    if args.word_diff.is_some() {
        match args.word_diff.as_deref() {
            Some("plain") => mode = WordDiffModeCli::Plain,
            Some("color") => mode = WordDiffModeCli::Color,
            Some("porcelain") => mode = WordDiffModeCli::Porcelain,
            Some("none") => mode = WordDiffModeCli::None,
            Some(_) => mode = WordDiffModeCli::Plain,
            None => {}
        }
    }
    if mode == WordDiffModeCli::None {
        return None;
    }
    let regex_override = args
        .word_diff_regex
        .clone()
        .or(regex_override)
        .or_else(|| args.color_words.as_ref().filter(|s| !s.is_empty()).cloned());
    let color_cli = args.color.as_deref();
    let force_color = matches!(mode, WordDiffModeCli::Color)
        || args.color_words.is_some()
        || color_cli == Some("always")
        || (matches!(color_cli, Some("auto") | None)
            && stdout_is_tty
            && mode != WordDiffModeCli::Plain);
    Some(EffectiveWordDiff {
        mode,
        regex_override,
        force_color,
    })
}

fn ansi_diff_color(config: &ConfigSet, key: &str, default_esc: &str) -> String {
    let raw = config.get(key).unwrap_or_default();
    if raw == "normal" {
        return String::new();
    }
    if let Some(rest) = raw.strip_prefix("ansi") {
        let codes: String = rest
            .split_whitespace()
            .filter_map(|t| t.parse::<u8>().ok())
            .map(|n| format!("\x1b[{n}m"))
            .collect();
        return codes;
    }
    default_esc.to_owned()
}

fn submodule_ignore_flags_from_diff_arg(ignore_sm: &str) -> SubmoduleIgnoreFlags {
    match ignore_sm {
        "all" => SubmoduleIgnoreFlags {
            ignore_all: true,
            ignore_untracked: false,
            ignore_dirty: false,
        },
        "untracked" => SubmoduleIgnoreFlags {
            ignore_all: false,
            ignore_untracked: true,
            ignore_dirty: false,
        },
        "dirty" => SubmoduleIgnoreFlags {
            ignore_all: false,
            ignore_untracked: false,
            ignore_dirty: true,
        },
        _ => SubmoduleIgnoreFlags {
            ignore_all: false,
            ignore_untracked: false,
            ignore_dirty: false,
        },
    }
}

/// Submodule HEAD OID for patch output when raw diff used a null new side (`git diff <tree>`).
fn submodule_head_oid_for_gitlink_patch(entry: &DiffEntry, work_tree: Option<&Path>) -> ObjectId {
    if entry.old_mode != "160000" || entry.new_mode != "160000" {
        return entry.new_oid;
    }
    if entry.new_oid != zero_oid() {
        return entry.new_oid;
    }
    work_tree
        .and_then(|wt| grit_lib::diff::read_submodule_head_oid(&entry.path().to_fs_path(wt)))
        .unwrap_or_else(zero_oid)
}

/// Suffix for `+Subproject commit …` when the submodule checkout has local changes.
fn submodule_path_to_name_map(work_tree: &Path, cfg: &ConfigSet) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let gm = work_tree.join(".gitmodules");
    if let Ok(text) = fs::read_to_string(&gm) {
        if let Ok(file) = ConfigFile::parse(&gm, &text, ConfigScope::Local) {
            for e in &file.entries {
                let Some(rest) = e.key.strip_prefix("submodule.") else {
                    continue;
                };
                let Some(name) = rest.strip_suffix(".path") else {
                    continue;
                };
                if let Some(p) = e.value.as_deref() {
                    let key = p.trim().replace('\\', "/");
                    if !key.is_empty() {
                        out.insert(key, name.to_string());
                    }
                }
            }
        }
    }
    for e in cfg.entries() {
        let Some(rest) = e.key.strip_prefix("submodule.") else {
            continue;
        };
        let Some(name) = rest.strip_suffix(".path") else {
            continue;
        };
        if let Some(p) = e.value.as_deref() {
            let key = p.trim().replace('\\', "/");
            if !key.is_empty() {
                out.insert(key, name.to_string());
            }
        }
    }
    out
}

/// `submodule.<name>.ignore` from `.gitmodules` (not part of merged [`ConfigSet`]).
fn gitmodules_submodule_name_to_ignore(work_tree: &Path) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let gm = work_tree.join(".gitmodules");
    let Ok(text) = fs::read_to_string(&gm) else {
        return out;
    };
    let Ok(file) = ConfigFile::parse(&gm, &text, ConfigScope::Local) else {
        return out;
    };
    for e in &file.entries {
        let Some(rest) = e.key.strip_prefix("submodule.") else {
            continue;
        };
        let Some(name) = rest.strip_suffix(".ignore") else {
            continue;
        };
        if let Some(v) = e.value.as_deref() {
            out.insert(name.to_string(), v.trim().to_ascii_lowercase());
        }
    }
    out
}

fn submodule_named_ignore_from_repo_config(
    submodule_name: &str,
    cfg: &ConfigSet,
) -> Option<String> {
    let key = format!("submodule.{submodule_name}.ignore");
    let key_canon = canonical_key(&key).ok()?;
    cfg.entries()
        .iter()
        .rev()
        .find(|e| e.key == key_canon)
        .and_then(|e| e.value.as_deref())
        .map(|s| s.trim().to_ascii_lowercase())
}

/// Per-path submodule ignore: repo `submodule.<name>.ignore`, then `.gitmodules`, then `diff.ignoreSubmodules`.
///
/// When the path is not registered in `.gitmodules` / config (embedded submodule), only
/// `diff.ignoreSubmodules` applies — matches Git (t4027 `test_config diff.ignoreSubmodules none`).
fn submodule_effective_ignore_token(
    path: &str,
    path_to_name: &HashMap<String, String>,
    gm_name_ignore: &HashMap<String, String>,
    cfg: &ConfigSet,
) -> Option<String> {
    if let Some(name) = path_to_name.get(path) {
        if let Some(v) = submodule_named_ignore_from_repo_config(name, cfg)
            .or_else(|| gm_name_ignore.get(name).cloned())
        {
            return Some(v);
        }
    }
    cfg.get("diff.ignoreSubmodules")
        .map(|s| s.trim().to_ascii_lowercase())
}

fn ignore_submodules_cli_all(cli: Option<&str>) -> bool {
    cli.is_some_and(|c| c.eq_ignore_ascii_case("all"))
}

fn ignore_submodules_cli_dirty(cli: Option<&str>) -> bool {
    cli.is_some_and(|c| c.eq_ignore_ascii_case("dirty"))
}

/// Drop gitlink entries entirely (`--ignore-submodules=all` or effective policy `all`).
fn gitlink_suppressed_ignore_all(
    entry: &DiffEntry,
    cli: Option<&str>,
    path_to_name: &HashMap<String, String>,
    gm_name_ignore: &HashMap<String, String>,
    cfg: &ConfigSet,
) -> bool {
    if entry.old_mode != "160000" && entry.new_mode != "160000" {
        return false;
    }
    if ignore_submodules_cli_all(cli) {
        return true;
    }
    if cli.is_some() {
        return false;
    }
    submodule_effective_ignore_token(
        &entry.path().to_string_lossy(),
        path_to_name,
        gm_name_ignore,
        cfg,
    )
    .as_deref()
        == Some("all")
}

/// Drop same-OID gitlink noise (dirty worktree and/or policy `dirty`, or default untracked-only).
fn gitlink_same_oid_suppressed(
    entry: &DiffEntry,
    cli: Option<&str>,
    path_to_name: &HashMap<String, String>,
    gm_name_ignore: &HashMap<String, String>,
    cfg: &ConfigSet,
    work_tree: Option<&Path>,
) -> bool {
    if entry.old_mode != "160000" || entry.new_mode != "160000" || entry.old_oid != entry.new_oid {
        return false;
    }
    if ignore_submodules_cli_dirty(cli) {
        return true;
    }
    if cli.is_some() {
        return false;
    }
    let path = entry.path().to_string_lossy();
    let token = submodule_effective_ignore_token(&path, path_to_name, gm_name_ignore, cfg);
    if token.as_deref() == Some("none") {
        return false;
    }
    if token.as_deref() == Some("dirty") {
        return true;
    }
    let Some(wt) = work_tree else {
        return false;
    };
    let flags = grit_lib::diff::submodule_porcelain_flags(wt, &path, entry.old_oid);
    if token.as_deref() == Some("untracked") {
        return !flags.modified && flags.untracked;
    }
    !flags.modified && flags.untracked
}

fn submodule_gitlink_patch_plus_suffix(
    entry: &DiffEntry,
    work_tree: Option<&Path>,
    submodule_ignore: SubmoduleIgnoreFlags,
    ignore_submodules_cli: Option<&str>,
    diff_cfg: &grit_lib::config::ConfigSet,
    path_to_name: &HashMap<String, String>,
    gm_name_ignore: &HashMap<String, String>,
) -> String {
    if entry.old_mode != "160000" || entry.new_mode != "160000" {
        return String::new();
    }
    let Some(wt) = work_tree else {
        return String::new();
    };
    let path = entry.path().to_string_lossy();
    let flags = grit_lib::diff::submodule_porcelain_flags(wt, &path, entry.old_oid);
    let eff = submodule_effective_ignore_token(&path, path_to_name, gm_name_ignore, diff_cfg);
    let cli_none = ignore_submodules_cli.is_some_and(|s| s.eq_ignore_ascii_case("none"));
    let count_untracked =
        !submodule_ignore.ignore_untracked && (cli_none || eff.as_deref() == Some("none"));
    let dirty_for_suffix =
        (flags.modified && !submodule_ignore.ignore_dirty) || (flags.untracked && count_untracked);
    if dirty_for_suffix {
        "-dirty".to_owned()
    } else {
        String::new()
    }
}

/// Open the submodule repository for `--submodule=log` only when it is checked out.
fn open_submodule_repo_for_log(
    super_git_dir: &Path,
    work_tree: Option<&Path>,
    path: &str,
) -> Option<Repository> {
    if let Some(wt) = work_tree {
        let sub_wt = wt.join(path);
        let dot_git = sub_wt.join(".git");
        if dot_git.exists() {
            if let Ok(gd) = resolve_dot_git(&dot_git) {
                if let Ok(repo) = Repository::open(&gd, Some(&sub_wt)) {
                    return Some(repo);
                }
            }
        }
    }
    let modules_dir = super_git_dir.join("modules").join(path);
    if modules_dir.is_dir() {
        Repository::open(&modules_dir, None).ok()
    } else {
        None
    }
}

fn write_submodule_log_lines(
    out: &mut impl Write,
    repo: &Repository,
    entry: &DiffEntry,
    work_tree: Option<&Path>,
) -> Result<()> {
    let z = zero_oid();
    if entry.old_oid == z {
        return Ok(());
    }
    let new_oid =
        if entry.new_oid == z && (entry.old_mode == "160000" || entry.new_mode == "160000") {
            submodule_head_oid_for_gitlink_patch(entry, work_tree)
        } else {
            entry.new_oid
        };
    if new_oid == z {
        return Ok(());
    }
    let old_a = abbreviate_object_id(repo, entry.old_oid, 7)?;
    let new_a = abbreviate_object_id(repo, new_oid, 7)?;
    let Some(wt) = work_tree.or(repo.work_tree.as_deref()) else {
        writeln!(out, "Submodule {} {}..{}:", entry.path(), old_a, new_a)?;
        return Ok(());
    };
    let Some(sub_repo) =
        open_submodule_repo_for_log(&repo.git_dir, Some(wt), &entry.path().to_string_lossy())
    else {
        writeln!(
            out,
            "Submodule {} {}...{} (commits not present)",
            entry.path(),
            old_a,
            new_a
        )?;
        return Ok(());
    };
    writeln!(out, "Submodule {} {}..{}:", entry.path(), old_a, new_a)?;
    let mut opts = RevListOptions::default();
    opts.first_parent = true;
    let (_, negative_specs) =
        grit_lib::rev_list::split_revision_token(&format!("^{}", entry.old_oid.to_hex()));
    let Ok(res) = rev_list(&sub_repo, &[new_oid.to_hex()], &negative_specs, &opts) else {
        return Ok(());
    };
    for oid in res.commits.iter().rev() {
        let Ok(obj) = sub_repo.odb.read(oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let Ok(c) = parse_commit(&obj.data) else {
            continue;
        };
        let subject = submodule_commit_subject_line(&c);
        writeln!(out, "  > {subject}")?;
    }
    Ok(())
}

fn submodule_commit_subject_line(c: &grit_lib::objects::CommitData) -> String {
    let enc = c.encoding.as_deref().unwrap_or("UTF-8");
    let is_latin1 = enc.eq_ignore_ascii_case("ISO8859-1")
        || enc.eq_ignore_ascii_case("ISO-8859-1")
        || enc.eq_ignore_ascii_case("LATIN1")
        || enc.eq_ignore_ascii_case("ISO-8859-15");
    if let Some(raw) = c.raw_message.as_deref() {
        let line = raw.split(|b| *b == b'\n').next().unwrap_or(raw);
        if is_latin1 {
            return line
                .iter()
                .map(|&b| b as char)
                .collect::<String>()
                .trim()
                .to_owned();
        }
        return String::from_utf8_lossy(line).trim().to_string();
    }
    c.message.lines().next().unwrap_or("").trim().to_owned()
}

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
/// ANSI color codes for diff output.
const RESET: &str = "\x1b[m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const BG_RED: &str = "\x1b[41m";
const MAGENTA: &str = "\x1b[35m";

/// `ws_error_highlight` bits (Git `WSEH_NEW`/`WSEH_OLD`/`WSEH_CONTEXT`).
const WSEH_NEW: u8 = 1;
const WSEH_OLD: u8 = 2;
const WSEH_CONTEXT: u8 = 4;

/// Resolve a diff color slot, honoring both `color.diff.<slot>` and `diff.color.<slot>`
/// (Git aliases), parsing the value with `parse_color`. `default_name` is a Git color *name*
/// (e.g. `green`, `bold red`, `normal`, or empty) used when neither config key is set.
fn resolve_diff_color(config: &ConfigSet, slot: &str, default_name: &str) -> String {
    let raw = config
        .get(&format!("color.diff.{slot}"))
        .or_else(|| config.get(&format!("diff.color.{slot}")));
    let name = match raw {
        Some(v) if !v.is_empty() => v,
        _ => default_name.to_owned(),
    };
    grit_lib::config::parse_color(&name).unwrap_or_default()
}

/// Parse one `--ws-error-highlight` / `diff.wsErrorHighlight` value (Git `parse_ws_error_highlight`).
/// Returns `None` on an unknown token (Git reports an error / leaves default in that case).
fn parse_ws_error_highlight(arg: &str) -> Option<u8> {
    let mut val: u8 = 0;
    for tok in arg.split(',') {
        match tok {
            "" => {}
            "none" => val = 0,
            "default" => val = WSEH_NEW,
            "all" => val = WSEH_NEW | WSEH_OLD | WSEH_CONTEXT,
            "new" => val |= WSEH_NEW,
            "old" => val |= WSEH_OLD,
            "context" => val |= WSEH_CONTEXT,
            _ => return None,
        }
    }
    Some(val)
}

/// Last `--ws-error-highlight=<value>` on the command line, if any (Git: later flag wins).
fn ws_error_highlight_from_argv() -> Option<u8> {
    let argv: Vec<String> = std::env::args().collect();
    let mut result: Option<u8> = None;
    let mut i = 0usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        if let Some(v) = a.strip_prefix("--ws-error-highlight=") {
            if let Some(b) = parse_ws_error_highlight(v) {
                result = Some(b);
            }
        } else if a == "--ws-error-highlight" {
            if let Some(n) = argv.get(i + 1) {
                if let Some(b) = parse_ws_error_highlight(n) {
                    result = Some(b);
                }
                i += 1;
            }
        }
        i += 1;
    }
    result
}

/// Effective `ws_error_highlight` bitmask: CLI flag overrides `diff.wsErrorHighlight` config,
/// which overrides the built-in default of `WSEH_NEW`.
fn effective_ws_error_highlight(config: &ConfigSet) -> u8 {
    if let Some(v) = ws_error_highlight_from_argv() {
        return v;
    }
    if let Some(cfg) = config.get("diff.wsErrorHighlight") {
        if let Some(v) = parse_ws_error_highlight(&cfg) {
            return v;
        }
    }
    WSEH_NEW
}

/// Context for whitespace-error highlighting while writing a colored patch.
struct WsColorCtx {
    /// Whitespace rule bits for the file being diffed.
    ws_rule: u32,
    /// `ws_error_highlight` bitmask (which line kinds get highlighted).
    highlight: u8,
    /// Resolved colors.
    new_color: String,
    old_color: String,
    context_color: String,
    ws_color: String,
    reset: String,
    /// Git `blank_at_eof_in_preimage` / `blank_at_eof_in_postimage`: a `+` blank line is
    /// flagged only when `pre_threshold <= lno_in_preimage && post_threshold <= lno_in_postimage`.
    /// Both `0` disables blank-at-eof highlighting.
    blank_at_eof_pre: usize,
    blank_at_eof_post: usize,
}

/// Port of Git `ws_check_emit_1` (emit side): paint whitespace errors in `body` using
/// `set` (the line's base color), `reset`, and `ws` (the whitespace-error color).
fn ws_check_emit(out: &mut String, body: &str, ws_rule: u32, set: &str, reset: &str, ws: &str) {
    let bytes = body.as_bytes();
    let mut len = bytes.len();

    let mut trailing_newline = false;
    if len > 0 && bytes[len - 1] == b'\n' {
        trailing_newline = true;
        len -= 1;
    }
    let mut trailing_carriage_return = false;
    if (ws_rule & ws::WS_CR_AT_EOL) != 0 && len > 0 && bytes[len - 1] == b'\r' {
        trailing_carriage_return = true;
        len -= 1;
    }

    let mut trailing_whitespace: isize = -1;
    if (ws_rule & ws::WS_BLANK_AT_EOL) != 0 {
        let mut i = len as isize - 1;
        while i >= 0 {
            // Git's `isspace` (sane_ctype) counts only space/tab/newline/CR — NOT
            // form-feed (0x0c) or vertical-tab (0x0b). So a leading `\f`/`\v` is
            // not treated as trailing whitespace (t4015 #134).
            if matches!(bytes[i as usize], b' ' | b'\t' | b'\n' | b'\r') {
                trailing_whitespace = i;
            } else {
                break;
            }
            i -= 1;
        }
    }
    let trailing_whitespace = if trailing_whitespace == -1 {
        len
    } else {
        trailing_whitespace as usize
    };

    let push = |out: &mut String, range: &[u8]| {
        out.push_str(&String::from_utf8_lossy(range));
    };

    // Check indentation.
    let mut written = 0usize;
    let mut i = 0usize;
    while i < trailing_whitespace {
        let c = bytes[i];
        if c == b' ' {
            i += 1;
            continue;
        }
        if c != b'\t' {
            break;
        }
        if (ws_rule & ws::WS_SPACE_BEFORE_TAB) != 0 && written < i {
            out.push_str(ws);
            push(out, &bytes[written..i]);
            out.push_str(reset);
            push(out, &bytes[i..i + 1]);
        } else if (ws_rule & ws::WS_TAB_IN_INDENT) != 0 {
            push(out, &bytes[written..i]);
            out.push_str(ws);
            push(out, &bytes[i..i + 1]);
            out.push_str(reset);
        } else {
            push(out, &bytes[written..i + 1]);
        }
        written = i + 1;
        i += 1;
    }

    // Check for indent using non-tab.
    if (ws_rule & ws::WS_INDENT_WITH_NON_TAB) != 0 && i - written >= ws::ws_tab_width(ws_rule) {
        out.push_str(ws);
        push(out, &bytes[written..i]);
        out.push_str(reset);
        written = i;
    }

    // Emit non-highlighted (middle) segment.
    if trailing_whitespace > written {
        out.push_str(set);
        push(out, &bytes[written..trailing_whitespace]);
        out.push_str(reset);
    }
    // Highlight trailing whitespace errors.
    if trailing_whitespace != len {
        out.push_str(ws);
        push(out, &bytes[trailing_whitespace..len]);
        out.push_str(reset);
    }
    if trailing_carriage_return {
        out.push('\r');
    }
    if trailing_newline {
        out.push('\n');
    }
}

/// Emit one content line (`+`/`-`/` `) with whitespace-error markup, mirroring Git
/// `emit_line_ws_markup` for the non-dual-color case (`set_sign == NULL`).
fn emit_ws_markup_line(
    out: &mut String,
    sign: char,
    body: &str,
    set: &str,
    reset: &str,
    ws_color: Option<&str>,
    ws_rule: u32,
    blank_at_eof: bool,
) {
    let (body_no_nl, nl) = match body.strip_suffix('\n') {
        Some(b) => (b, "\n"),
        None => (body, ""),
    };
    match ws_color {
        None => {
            // No highlighting (Git `emit_line_0`): a trailing carriage return is
            // emitted *after* the reset, so it never lands inside the line color.
            let (body_core, cr) = match body_no_nl.strip_suffix('\r') {
                Some(b) => (b, "\r"),
                None => (body_no_nl, ""),
            };
            out.push_str(set);
            out.push(sign);
            out.push_str(body_core);
            out.push_str(reset);
            out.push_str(cr);
            out.push_str(nl);
        }
        Some(ws) if blank_at_eof => {
            // Blank line at EOF: paint the whole `+` line (including sign) with ws color.
            out.push_str(ws);
            out.push(sign);
            out.push_str(body_no_nl);
            out.push_str(reset);
            out.push_str(nl);
        }
        Some(ws) => {
            // Prefix `{set}{sign}{reset}`, then the body via ws_check_emit.
            out.push_str(set);
            out.push(sign);
            out.push_str(reset);
            ws_check_emit(out, body, ws_rule, set, reset, ws);
        }
    }
}

/// Whitespace-ignore options bundled together.
#[derive(Debug, Default)]
struct WhitespaceMode {
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_blank_lines: bool,
    ignore_cr_at_eol: bool,
}

/// Whether to emit ANSI colors for unified diff output (CLI `--color` + `diff.color` / `color.diff`).
fn diff_use_color(cli_color: Option<&str>, config: &ConfigSet, output_path: Option<&Path>) -> bool {
    let cfg_val = config
        .get("diff.color")
        .or_else(|| config.get("color.diff"))
        .map(|s| s.to_ascii_lowercase());
    // `auto` colors only when writing to a terminal that can render ANSI: not a
    // file/pipe, and on Windows only when the console understands VT sequences.
    let auto = output_path.is_none()
        && io::stdout().is_terminal()
        && grit_lib::terminal::ansi_supported();
    match cli_color.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("always") => true,
        Some("never") => false,
        Some("auto") => auto,
        Some(_) => false,
        None => match cfg_val.as_deref() {
            Some("always") => true,
            Some("never") | Some("false") => false,
            Some("auto") | None => auto,
            _ => auto,
        },
    }
}

impl WhitespaceMode {
    /// Returns true if any whitespace-ignore option is active.
    fn any(&self) -> bool {
        self.ignore_all_space
            || self.ignore_space_change
            || self.ignore_space_at_eol
            || self.ignore_blank_lines
            || self.ignore_cr_at_eol
    }

    /// Normalize a string according to the active whitespace modes.
    fn normalize(&self, s: &str) -> String {
        let mut lines: Vec<String> = s.lines().map(|l| self.normalize_line(l)).collect();
        if self.ignore_blank_lines {
            lines.retain(|l| !l.trim().is_empty());
        }
        lines.join("\n")
    }

    /// True when only `--ignore-blank-lines` is active among the whitespace modes (so blank-line
    /// ignoring can be handled by xdiff-style hunk suppression rather than line deletion).
    fn only_blank_lines(&self) -> bool {
        self.ignore_blank_lines
            && !self.ignore_all_space
            && !self.ignore_space_change
            && !self.ignore_space_at_eol
            && !self.ignore_cr_at_eol
    }

    /// Normalize a single line according to the active whitespace modes.
    fn normalize_line(&self, line: &str) -> String {
        let mut s = line.to_owned();

        // --ignore-cr-at-eol: strip trailing CR
        if self.ignore_cr_at_eol && s.ends_with('\r') {
            s.truncate(s.len() - 1);
        }

        // -w / --ignore-all-space: remove all whitespace
        if self.ignore_all_space {
            s = s.chars().filter(|c| !c.is_whitespace()).collect();
            return s;
        }

        // -b / --ignore-space-change: collapse runs of whitespace to single space
        if self.ignore_space_change {
            let mut result = String::with_capacity(s.len());
            let mut in_space = false;
            for c in s.chars() {
                if c.is_whitespace() {
                    if !in_space {
                        result.push(' ');
                        in_space = true;
                    }
                } else {
                    result.push(c);
                    in_space = false;
                }
            }
            s = result.trim_end().to_owned();
            return s;
        }

        // --ignore-space-at-eol: strip trailing whitespace
        if self.ignore_space_at_eol {
            s = s.trim_end().to_owned();
        }

        s
    }

    /// Byte-oriented line normalisation for diffing (aligned with merge three-way compare rules).
    fn normalize_line_bytes(&self, line: &[u8]) -> Vec<u8> {
        let mut bytes = line.to_vec();

        if self.ignore_cr_at_eol && bytes.ends_with(b"\r\n") {
            let len = bytes.len();
            bytes.remove(len - 2);
        }

        if self.ignore_all_space {
            return bytes
                .into_iter()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
        }

        if self.ignore_space_change {
            let mut out = Vec::with_capacity(bytes.len());
            let mut in_ws = false;
            for ch in bytes {
                if ch.is_ascii_whitespace() {
                    if !in_ws {
                        out.push(b' ');
                        in_ws = true;
                    }
                } else {
                    out.push(ch);
                    in_ws = false;
                }
            }
            while out.last().is_some_and(|b| b.is_ascii_whitespace()) {
                out.pop();
            }
            return out;
        }

        if self.ignore_space_at_eol {
            if bytes.last().copied() == Some(b'\n') {
                let mut body = bytes[..bytes.len() - 1].to_vec();
                while body.last().is_some_and(|b| b.is_ascii_whitespace()) {
                    body.pop();
                }
                body.push(b'\n');
                bytes = body;
            } else {
                while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
                    bytes.pop();
                }
            }
        }

        bytes
    }
}

/// A [`Write`] wrapper that prepends a fixed prefix at the start of every output line
/// (implements `git diff --line-prefix=<p>`: the prefix appears before each emitted line,
/// including stat, raw, and patch bodies).
struct LinePrefixWriter<W: Write> {
    inner: W,
    prefix: Vec<u8>,
    at_line_start: bool,
}

impl<W: Write> LinePrefixWriter<W> {
    fn new(inner: W, prefix: &str) -> Self {
        Self {
            inner,
            prefix: prefix.as_bytes().to_vec(),
            at_line_start: true,
        }
    }
}

impl<W: Write> Write for LinePrefixWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            if self.at_line_start {
                self.inner.write_all(&self.prefix)?;
                self.at_line_start = false;
            }
            self.inner.write_all(std::slice::from_ref(&b))?;
            if b == b'\n' {
                self.at_line_start = true;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// One logical line for `--no-index` diffing: compare key plus original bytes for output.
struct NoIndexLineSlot {
    display: Vec<u8>,
    compare: Vec<u8>,
}

/// Split blob bytes into lines, each retaining a trailing `\n` when present (Git line convention).
fn no_index_split_lines(data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            lines.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        lines.push(data[start..].to_vec());
    }
    lines
}

fn no_index_line_body_is_blank(line: &[u8]) -> bool {
    let end = if line.last().copied() == Some(b'\n') {
        line.len().saturating_sub(1)
    } else {
        line.len()
    };
    let end = if end > 0 && line.get(end - 1) == Some(&b'\r') {
        end - 1
    } else {
        end
    };
    line[..end].iter().all(|b| b.is_ascii_whitespace())
}

fn no_index_build_line_slots(data: &[u8], mode: &WhitespaceMode) -> Vec<NoIndexLineSlot> {
    no_index_split_lines(data)
        .into_iter()
        .filter(|line| !mode.ignore_blank_lines || !no_index_line_body_is_blank(line))
        .map(|line| NoIndexLineSlot {
            compare: mode.normalize_line_bytes(&line),
            display: line,
        })
        .collect()
}

/// Unified diff body (`---` / `+++` / hunks) for `--no-index`, optional algorithm and whitespace rules.
fn no_index_unified_patch_body(
    old_bytes: &[u8],
    new_bytes: &[u8],
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    mode: &WhitespaceMode,
    algorithm: similar::Algorithm,
    use_git_histogram: bool,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    let old_slots = no_index_build_line_slots(old_bytes, mode);
    let new_slots = no_index_build_line_slots(new_bytes, mode);

    // Compare keys must not include trailing `\n`: `similar`'s unified writer adds a newline
    // when `newline_terminated` is false (slice diff); a `\n` in the value would double spacing.
    let line_key = |bytes: &[u8]| -> String {
        let mut end = bytes.len();
        if end > 0 && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let old_compare_owned: Vec<String> = old_slots.iter().map(|s| line_key(&s.compare)).collect();
    let new_compare_owned: Vec<String> = new_slots.iter().map(|s| line_key(&s.compare)).collect();
    let old_compare: Vec<&str> = old_compare_owned.iter().map(|s| s.as_str()).collect();
    let new_compare: Vec<&str> = new_compare_owned.iter().map(|s| s.as_str()).collect();

    if use_git_histogram && !mode.any() {
        let old_str = String::from_utf8_lossy(old_bytes);
        let new_str = String::from_utf8_lossy(new_bytes);
        let hunks = unified_diff_histogram_hunks_only(
            old_str.as_ref(),
            new_str.as_ref(),
            context_lines,
            inter_hunk_context,
        );
        let old_label = if old_path == "/dev/null" {
            "--- /dev/null\n".to_string()
        } else {
            format!(
                "--- {}\n",
                format_diff_path_with_prefix("a/", old_path, quote_path_fully)
            )
        };
        let new_label = if new_path == "/dev/null" {
            "+++ /dev/null\n".to_string()
        } else {
            format!(
                "+++ {}\n",
                format_diff_path_with_prefix("b/", new_path, quote_path_fully)
            )
        };
        let mut output = String::new();
        output.push_str(&old_label);
        output.push_str(&new_label);
        output.push_str(&hunks);
        return output;
    }

    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_slices(&old_compare, &new_compare);
    let compacted_ops =
        diff_slice_ops_compacted(&old_compare, &new_compare, algorithm, indent_heuristic);

    let old_label = if old_path == "/dev/null" {
        "--- /dev/null\n".to_string()
    } else {
        format!(
            "--- {}\n",
            format_diff_path_with_prefix("a/", old_path, quote_path_fully)
        )
    };
    let new_label = if new_path == "/dev/null" {
        "+++ /dev/null\n".to_string()
    } else {
        format!(
            "+++ {}\n",
            format_diff_path_with_prefix("b/", new_path, quote_path_fully)
        )
    };

    let line_body_for_patch = |line: &[u8]| -> String {
        let mut end = line.len();
        if end > 0 && line[end - 1] == b'\n' {
            end -= 1;
        }
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
        String::from_utf8_lossy(&line[..end]).into_owned()
    };

    let mut output = String::new();
    output.push_str(&old_label);
    output.push_str(&new_label);

    let max_common_gap = context_lines.saturating_mul(2);
    let group_radius = max_common_gap.div_ceil(2);
    let op_groups = group_diff_ops(compacted_ops, group_radius);

    for ops in op_groups {
        if ops.is_empty() {
            continue;
        }
        let hunk = UnifiedDiffHunk::new(ops, &diff, false);
        let hunk_str = format!("{hunk}");
        let Some(first_newline) = hunk_str.find('\n') else {
            output.push_str(&hunk_str);
            continue;
        };
        let header_line = &hunk_str[..first_newline];
        output.push_str(header_line);
        output.push('\n');
        for change in hunk.iter_changes() {
            match change.tag() {
                ChangeTag::Equal => {
                    let Some(idx) = change.new_index() else {
                        continue;
                    };
                    let raw = &new_slots[idx].display;
                    output.push(' ');
                    output.push_str(&line_body_for_patch(raw));
                    output.push('\n');
                }
                ChangeTag::Delete => {
                    let Some(idx) = change.old_index() else {
                        continue;
                    };
                    let raw = &old_slots[idx].display;
                    output.push('-');
                    output.push_str(&line_body_for_patch(raw));
                    output.push('\n');
                }
                ChangeTag::Insert => {
                    let Some(idx) = change.new_index() else {
                        continue;
                    };
                    let raw = &new_slots[idx].display;
                    output.push('+');
                    output.push_str(&line_body_for_patch(raw));
                    output.push('\n');
                }
            }
        }
    }

    output
}

/// Resolve line-diff algorithm for `git diff` from flags and argv order (last wins).
fn effective_line_diff_algorithm(args: &Args) -> (similar::Algorithm, bool) {
    let raw: Vec<String> = std::env::args().collect();
    let mut best: Option<(usize, CliDiffAlgo)> = None;
    let mut record = |pos: Option<usize>, algo: CliDiffAlgo| {
        if let Some(p) = pos {
            if best.is_none_or(|(bp, _)| p >= bp) {
                best = Some((p, algo));
            }
        }
    };
    record(
        raw.iter().rposition(|a| a == "--histogram"),
        CliDiffAlgo {
            similar: similar::Algorithm::Patience,
            histogram_git: true,
        },
    );
    record(
        raw.iter().rposition(|a| a == "--patience"),
        CliDiffAlgo {
            similar: similar::Algorithm::Patience,
            histogram_git: false,
        },
    );
    record(
        raw.iter().rposition(|a| a == "--minimal"),
        CliDiffAlgo {
            similar: similar::Algorithm::Lcs,
            histogram_git: false,
        },
    );
    if let Some(name) = args.diff_algorithm.as_deref() {
        let pos = raw
            .iter()
            .rposition(|a| a == "--diff-algorithm" || a.starts_with("--diff-algorithm="));
        let c = match name.to_lowercase().as_str() {
            "histogram" => CliDiffAlgo {
                similar: similar::Algorithm::Patience,
                histogram_git: true,
            },
            "patience" => CliDiffAlgo {
                similar: similar::Algorithm::Patience,
                histogram_git: false,
            },
            "minimal" => CliDiffAlgo {
                similar: similar::Algorithm::Lcs,
                histogram_git: false,
            },
            "myers" | "default" => CliDiffAlgo {
                similar: similar::Algorithm::Myers,
                histogram_git: false,
            },
            _ => CliDiffAlgo {
                similar: similar::Algorithm::Myers,
                histogram_git: false,
            },
        };
        record(pos, c);
    }
    best.map(|(_, a)| (a.similar, a.histogram_git))
        .unwrap_or((similar::Algorithm::Myers, false))
}

/// Short blob id for `index` lines (matches `git rev-parse --short` default length).
fn no_index_blob_abbrev(data: &[u8]) -> String {
    let oid = Odb::hash_object_data(ObjectKind::Blob, data);
    let hex = oid.to_hex();
    let len = 7_usize.min(hex.len());
    hex[..len].to_owned()
}

/// Arguments for `grit diff`.
#[derive(Debug, Default, ClapArgs)]
#[command(about = "Show changes between commits, commit and working tree, etc.")]
pub struct Args {
    /// Show staged changes (index vs HEAD). Alias: --staged.
    #[arg(long = "cached", alias = "staged")]
    pub cached: bool,

    /// Diff against the merge base of HEAD and the given commit (or of two commits).
    #[arg(long = "merge-base")]
    pub merge_base: bool,

    /// Show a diffstat summary instead of the patch.
    /// Accepts optional `--stat=<width>[,<name-width>[,<count>]]`.
    #[arg(long = "stat", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub stat: Option<String>,

    /// Limit the number of files shown in --stat output.
    #[arg(long = "stat-count")]
    pub stat_count: Option<usize>,

    /// Set the width of the --stat output.
    #[arg(long = "stat-width")]
    pub stat_width: Option<usize>,

    /// Set the width of the graph portion of --stat output.
    #[arg(long = "stat-graph-width")]
    pub stat_graph_width: Option<usize>,

    /// Set the width of the filename portion of --stat output.
    #[arg(long = "stat-name-width")]
    pub stat_name_width: Option<usize>,

    /// Show only the summary line: N files changed, N insertions(+), N deletions(-).
    #[arg(long = "shortstat")]
    pub shortstat: bool,

    /// Compact per-file summary with mode/new/gone annotations (like `git diff --compact-summary`).
    #[arg(long = "compact-summary")]
    pub compact_summary: bool,

    /// Show machine-readable stat (additions/deletions per file).
    #[arg(long = "numstat")]
    pub numstat: bool,

    /// Show only the names of changed files.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Show the names and status of changed files.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Show raw diff format (:old-mode new-mode old-oid new-oid status\tpath).
    #[arg(long = "raw")]
    pub raw: bool,

    /// Do not abbreviate object IDs.
    #[arg(long = "no-abbrev")]
    pub no_abbrev: bool,

    /// Show the full object ID in raw and index output.
    #[arg(long = "full-index")]
    pub full_index: bool,

    /// Abbreviation length for object IDs in raw output.
    #[arg(long = "abbrev", value_name = "N")]
    pub abbrev: Option<usize>,

    /// Merge hunks that are within <N> lines of each other.
    #[arg(long = "inter-hunk-context", value_name = "N")]
    pub inter_hunk_context: Option<usize>,

    /// Ignore submodule changes. Accepts: all, dirty, untracked, none.
    #[arg(long = "ignore-submodules", value_name = "WHEN", default_missing_value = "all", num_args = 0..=1, require_equals = true)]
    pub ignore_submodules: Option<String>,

    /// Detect and color moved lines differently.
    #[arg(long = "color-moved", value_name = "MODE", default_missing_value = "default", num_args = 0..=1, require_equals = true)]
    pub color_moved: Option<String>,

    /// How whitespace is ignored when performing move detection (`--color-moved-ws=<modes>`).
    #[arg(long = "color-moved-ws", value_name = "MODES")]
    pub color_moved_ws: Option<String>,

    /// Break complete rewrite into delete + add pair.
    #[arg(short = 'B', long = "break-rewrites")]
    pub break_rewrites: bool,

    /// Omit preimage lines for deleted files (irreversible delete).
    #[arg(short = 'D', long = "irreversible-delete")]
    pub irreversible_delete: bool,

    /// Output a binary diff that can be applied with git-apply.
    #[arg(long = "binary")]
    pub binary: bool,

    /// Reverse the diff (swap old and new).
    #[arg(short = 'R')]
    pub reverse: bool,

    /// Show a condensed summary of extended header info (renames, mode changes).
    #[arg(long = "summary")]
    pub summary: bool,

    /// Exit with status 1 if there are differences, 0 otherwise.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Suppress diff output; implies --exit-code.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Rotate the output to start at the named file.
    #[arg(long = "rotate-to")]
    pub rotate_to: Option<String>,

    /// Skip output until the named file.
    #[arg(long = "skip-to")]
    pub skip_to: Option<String>,

    /// Order files according to the given orderfile.
    #[arg(short = 'O')]
    pub order_file: Option<String>,

    /// Use the histogram diff algorithm.
    #[arg(long = "histogram")]
    pub histogram: bool,

    /// Use the patience diff algorithm.
    #[arg(long = "patience")]
    pub patience: bool,

    /// Use the minimal diff algorithm.
    #[arg(long = "minimal")]
    pub minimal: bool,

    /// Select a diff algorithm (myers, minimal, patience, histogram).
    #[arg(long = "diff-algorithm")]
    pub diff_algorithm: Option<String>,

    /// Enable indent heuristic (default in modern git).
    #[arg(long = "indent-heuristic")]
    pub indent_heuristic: bool,

    /// Disable indent heuristic.
    #[arg(long = "no-indent-heuristic")]
    pub no_indent_heuristic: bool,

    /// Anchor the diff on the given text (can be repeated).
    #[arg(long = "anchored")]
    pub anchored: Vec<String>,

    /// Show relative paths from the given subdirectory.
    #[arg(long = "relative", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub relative: Option<Option<String>>,

    /// Disable --relative.
    #[arg(long = "no-relative")]
    pub no_relative: bool,

    /// Colorize the output. Values: always, never, auto.
    #[arg(long = "color", value_name = "WHEN", default_missing_value = "always", num_args = 0..=1, require_equals = true)]
    pub color: Option<String>,

    /// Show a word-level diff with `[-removed-]{+added+}` markers.
    #[arg(long = "word-diff", value_name = "MODE", default_missing_value = "plain", num_args = 0..=1, require_equals = true, overrides_with = "word_diff")]
    pub word_diff: Option<String>,

    /// Regular expression that defines a word for word diff (Git `--word-diff-regex`).
    #[arg(long = "word-diff-regex", value_name = "REGEX")]
    pub word_diff_regex: Option<String>,

    /// Colored word diff; optional `=<regex>` (`--color-words=pat`). Bare `--color-words` must not
    /// consume a path (t4034-diff-words).
    #[arg(
        long = "color-words",
        value_name = "REGEX",
        default_missing_value = "",
        num_args = 0..=1,
        require_equals = true
    )]
    pub color_words: Option<String>,

    /// Ignore all whitespace when comparing lines (-w).
    #[arg(short = 'w', long = "ignore-all-space")]
    pub ignore_all_space: bool,

    /// Ignore changes in amount of whitespace (-b).
    #[arg(short = 'b', long = "ignore-space-change")]
    pub ignore_space_change: bool,

    /// Ignore whitespace at end of line.
    #[arg(long = "ignore-space-at-eol")]
    pub ignore_space_at_eol: bool,

    /// Ignore changes whose lines are all blank.
    #[arg(long = "ignore-blank-lines")]
    pub ignore_blank_lines: bool,

    /// Ignore carriage-return at end of line.
    #[arg(long = "ignore-cr-at-eol")]
    pub ignore_cr_at_eol: bool,

    /// Generate patch output (default behavior; for compatibility with git).
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Suppress all patch output (`-s` / `--no-patch`, same as Git).
    #[arg(short = 's', long = "no-patch")]
    pub no_patch: bool,

    /// Emit raw diff lines before the unified patch (same as Git `diff --patch-with-raw`).
    #[arg(long = "patch-with-raw")]
    pub patch_with_raw: bool,

    /// Emit `--stat` before the unified patch (same as Git `diff --patch-with-stat`).
    #[arg(long = "patch-with-stat")]
    pub patch_with_stat: bool,

    /// Directory-level diffstat (`--dirstat=lines`, etc.). Empty value means default `changes`.
    /// Later `--dirstat` / `-X` options override earlier ones (Git semantics).
    #[arg(
        long = "dirstat",
        value_name = "PARAM",
        default_missing_value = "",
        num_args = 0..=1,
        require_equals = true,
        action = clap::ArgAction::Append
    )]
    pub dirstat: Vec<String>,

    /// Synonym for `--dirstat=files` (optional `=<param>` like `--dirstat`).
    #[arg(
        long = "dirstat-by-file",
        value_name = "PARAM",
        default_missing_value = "",
        num_args = 0..=1,
        require_equals = true
    )]
    pub dirstat_by_file: Option<String>,

    /// Set `--dirstat=cumulative` (Git synonym).
    #[arg(long = "cumulative")]
    pub dirstat_cumulative_flag: bool,

    /// Number of context lines in unified diff output (default: 3).
    #[arg(
        short = 'U',
        long = "unified",
        value_name = "N",
        allow_hyphen_values = true
    )]
    pub unified: Option<usize>,

    /// Show whole surrounding functions as context (same as Git `-W`).
    #[arg(short = 'W', long = "function-context")]
    pub function_context: bool,

    /// Detect renames.
    #[arg(short = 'M', long = "find-renames", value_name = "N", default_missing_value = "50", num_args = 0..=1, require_equals = true)]
    pub find_renames: Option<String>,

    /// Submodule diff output (`log` is the default for bare `--submodule`, matching Git).
    #[arg(long = "submodule", value_name = "FORMAT", default_missing_value = "log", num_args = 0..=1)]
    pub submodule: Option<String>,

    /// Disable external diff drivers (no-op, for compatibility).
    #[arg(long = "no-ext-diff")]
    pub no_ext_diff: bool,

    /// Compare two paths outside a git repository.
    #[arg(long = "no-index")]
    pub no_index: bool,

    /// Disable external diff drivers (accepted for compatibility, no-op).

    /// Disable textconv filters (accepted for compatibility, no-op).
    #[arg(long = "no-textconv")]
    pub no_textconv: bool,

    /// Check for whitespace errors in the diff.
    #[arg(long = "check")]
    pub check: bool,

    /// Set the source prefix (default: "a/").
    #[arg(long = "src-prefix", value_name = "PREFIX")]
    pub src_prefix: Option<String>,

    /// Set the destination prefix (default: "b/").
    #[arg(long = "dst-prefix", value_name = "PREFIX")]
    pub dst_prefix: Option<String>,

    /// Do not show any source or destination prefix.
    #[arg(long = "no-prefix")]
    pub no_prefix: bool,

    /// Override config and use the default "a/"/"b/" prefix.
    #[arg(long = "default-prefix")]
    pub default_prefix: bool,

    /// Line prefix for every line of output.
    #[arg(long = "line-prefix", value_name = "PREFIX")]
    pub line_prefix: Option<String>,

    /// Redirect diff output to a file (default stdout).
    #[arg(long = "output", value_name = "file")]
    pub output_path: Option<PathBuf>,

    /// Disable rename detection (must not be abbreviated).
    #[arg(long = "no-renames")]
    pub no_renames: bool,

    /// Detect copies (treat as rename detection for now).
    #[arg(short = 'C', long = "find-copies", value_name = "N", default_missing_value = "50", num_args = 0..=1, require_equals = true, overrides_with = "find_copies")]
    pub find_copies: Option<String>,

    /// Limit exhaustive rename/copy detection candidates.
    #[arg(short = 'l', value_name = "N")]
    pub rename_limit: Option<usize>,

    /// Find copies harder (look at unmodified files as source).
    #[arg(long = "find-copies-harder")]
    pub find_copies_harder: bool,

    /// Pickaxe: look for diffs that change the number of occurrences of the specified string.
    /// Parsed manually from trailing args since -S<string> value is attached.
    #[arg(skip)]
    pub pickaxe_string: Option<String>,

    /// Pickaxe: look for diffs whose patch text contains added/removed lines matching regex.
    /// Parsed manually from trailing args since -G takes a space-separated value.
    #[arg(skip)]
    pub pickaxe_grep: Option<String>,

    /// Ignore lines matching regex (`-I` / `--ignore-matching-lines`). Parsed from trailing args.
    #[arg(skip)]
    pub ignore_matching_lines: Vec<String>,

    /// Filter diff output by change type (e.g. `R` for renames only). Parsed from trailing args.
    #[arg(skip)]
    pub diff_filter: Option<String>,

    /// Treat the string given to -S as a POSIX extended regex.
    #[arg(long = "pickaxe-regex")]
    pub pickaxe_regex: bool,

    /// Commits or paths. Use `--` to separate revisions from paths.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parsed `--dirstat` / `diff.dirstat` options (Git-compatible).
#[derive(Clone, Debug)]
pub(crate) struct DirstatOptions {
    /// `changes` (byte damage), `lines` (insertion+deletion lines), or `files` (1 per file).
    mode: DirstatMode,
    cumulative: bool,
    /// Minimum permille (parts per thousand) to print a line; default 30 = 3%.
    permille: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirstatMode {
    Changes,
    Lines,
    Files,
}

impl Default for DirstatOptions {
    fn default() -> Self {
        Self {
            mode: DirstatMode::Changes,
            cumulative: false,
            permille: 30,
        }
    }
}

impl DirstatOptions {
    fn is_default_everything(&self) -> bool {
        self.mode == DirstatMode::Changes && !self.cumulative && self.permille == 30
    }
}

/// Apply a comma-separated `--dirstat` / `diff.dirstat` parameter string to `opts`.
///
/// Mirrors Git's `parse_dirstat_params`: every token is processed and *all* unrecognised
/// tokens / malformed percentages accumulate into `errors` (one indented line each), rather
/// than aborting at the first bad token. The caller decides whether a non-empty `errors`
/// list is fatal (CLI `--dirstat`/`-X`) or merely a warning (`diff.dirstat` config). The
/// `strict` flag only controls the error-message header, not when errors are collected.
fn parse_dirstat_apply_tokens(
    params: &str,
    opts: &mut DirstatOptions,
    _strict: bool,
    errors: &mut Vec<String>,
) {
    if params.is_empty() {
        return;
    }
    for raw in params.split(',') {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }
        if p.eq_ignore_ascii_case("changes") {
            opts.mode = DirstatMode::Changes;
        } else if p.eq_ignore_ascii_case("lines") {
            opts.mode = DirstatMode::Lines;
        } else if p.eq_ignore_ascii_case("files") {
            opts.mode = DirstatMode::Files;
        } else if p.eq_ignore_ascii_case("noncumulative") {
            opts.cumulative = false;
        } else if p.eq_ignore_ascii_case("cumulative") {
            opts.cumulative = true;
        } else if p.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            match parse_dirstat_percentage_permille(p) {
                Some(pm) => opts.permille = pm,
                None => {
                    errors.push(format!(
                        "  Failed to parse dirstat cut-off percentage '{p}'\n"
                    ));
                }
            }
        } else {
            errors.push(format!("  Unknown dirstat parameter '{p}'\n"));
        }
    }
}

/// Apply CLI `--dirstat`/`-X` tokens, accumulating into `cli_errors` (joined and reported
/// fatally by the caller). Kept as a thin wrapper so existing call sites read naturally.
fn parse_dirstat_apply_tokens_cli(
    params: &str,
    opts: &mut DirstatOptions,
    cli_errors: &mut Vec<String>,
) {
    parse_dirstat_apply_tokens(params, opts, true, cli_errors);
}

fn parse_dirstat_params_lenient(params: &str) -> (DirstatOptions, Vec<String>) {
    let mut o = DirstatOptions::default();
    let mut errors = Vec::new();
    parse_dirstat_apply_tokens(params, &mut o, false, &mut errors);
    // Config errors are reported as a single warning with all offending tokens.
    let warnings = if errors.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Found errors in 'diff.dirstat' config variable:\n{}",
            errors.concat()
        )]
    };
    (o, warnings)
}

#[derive(Clone, Debug)]
struct DirstatFile {
    name: String,
    changed: u64,
}

/// Write the `diff --git` line for `git diff --no-index`.
fn write_no_index_diff_git_line(
    out: &mut String,
    path_a: &str,
    path_b: &str,
    quote_path_fully: bool,
) {
    let a = format_diff_path_with_prefix("a/", path_a, quote_path_fully);
    let b = format_diff_path_with_prefix("b/", path_b, quote_path_fully);
    let _ = writeln!(out, "diff --git {a} {b}");
}

fn abbrev_oid_hex(data: &[u8], abbrev_len: usize) -> String {
    let oid = Odb::hash_object_data(ObjectKind::Blob, data);
    let hex = oid.to_hex();
    let len = abbrev_len.min(hex.len());
    hex[..len].to_owned()
}

fn write_no_index_index_lines(
    out: &mut String,
    data_a: &[u8],
    data_b: &[u8],
    mode_a: &str,
    mode_b: &str,
    abbrev_len: usize,
) {
    let a = abbrev_oid_hex(data_a, abbrev_len);
    let b = abbrev_oid_hex(data_b, abbrev_len);
    if mode_a == mode_b {
        let _ = writeln!(out, "index {a}..{b} {mode_a}");
    } else {
        let _ = writeln!(out, "index {a}..{b}");
        let _ = writeln!(out, "old mode {mode_a}");
        let _ = writeln!(out, "new mode {mode_b}");
    }
}

/// Build the unstaged patch Git writes for `git add -e` (index vs work tree, 7 context lines).
///
/// Matches `edit_patch` in Git's `builtin/add.c`: `run_diff_files` with `context = 7`, patch format,
/// no color, and submodule dirty paths ignored like `ignore_dirty_submodules`.
pub(crate) fn unstaged_patch_for_add_edit(
    repo: &Repository,
    pathspecs: &[String],
) -> Result<String> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e.into()),
    };

    let entries = diff_index_to_worktree(&repo.odb, &index, work_tree, false, false)?;

    let cwd = repo.effective_pathspec_cwd();
    let mut pathspec_prefix_buf = show_prefix(repo, &cwd);
    if !pathspec_prefix_buf.is_empty() {
        pathspec_prefix_buf.pop();
    }
    let pathspec_prefix = (!pathspec_prefix_buf.is_empty()).then_some(pathspec_prefix_buf.as_str());
    let resolved_specs: Vec<String> = pathspecs
        .iter()
        .map(|p| resolve_pathspec(p, work_tree, pathspec_prefix.as_deref()))
        .collect();
    let entries = filter_by_paths(entries, &resolved_specs);

    let ignore_sm = "dirty";

    let diff_config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let merged_attrs = load_gitattributes_for_diff(repo)?;
    let merged_attrs = Arc::new(merged_attrs);
    let ignore_case_attrs = diff_config
        .get("core.ignorecase")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
    let diff_algo_ctx = DiffAlgoContext {
        attrs: Arc::clone(&merged_attrs),
        config: Arc::new(diff_config.clone()),
        ignore_case_attrs,
    };
    let diff_algo_cli = parse_cli_diff_algorithm_from_argv();

    let mut diff_args = Args::default();
    diff_args.color = Some("never".to_owned());
    let (src_prefix, dst_prefix) = resolve_diff_prefixes(&diff_args, repo, false);
    let relative_prefix = resolve_diff_relative_prefix(Some(work_tree), &repo.git_dir, &diff_args);

    let empty_sm: HashMap<String, String> = HashMap::new();
    let empty_gm: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    write_patch_with_prefix(
        &mut out,
        repo,
        &entries,
        &repo.odb,
        &repo.git_dir,
        &diff_config,
        7,
        false,
        None,
        Some(work_tree),
        false,
        7,
        0,
        false,
        false,
        false,
        true,
        &src_prefix,
        &dst_prefix,
        diff_config.quote_path_fully(),
        None,
        submodule_ignore_flags_from_diff_arg(ignore_sm),
        Some(ignore_sm),
        &diff_config,
        &empty_sm,
        &empty_gm,
        None,
        &WhitespaceMode::default(),
        &diff_algo_ctx,
        diff_algo_cli,
        false,
        false,
        true,
        None,
        true,
        relative_prefix.as_deref(),
        resolve_indent_heuristic(&diff_config, false, false),
    )?;
    Ok(String::from_utf8(out).context("diff patch was not valid UTF-8")?)
}

/// Run the `diff` command.
pub fn run(mut args: Args) -> Result<()> {
    // Parse --stat=<width>[,<name-width>[,<count>]] into separate fields
    let mut stat_enabled = if let Some(ref val) = args.stat {
        if !val.is_empty() {
            let parts: Vec<&str> = val.split(',').collect();
            if let Some(w) = parts.first().and_then(|s| s.parse::<usize>().ok()) {
                if args.stat_width.is_none() {
                    args.stat_width = Some(w);
                }
            }
            if let Some(nw) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
                if args.stat_name_width.is_none() {
                    args.stat_name_width = Some(nw);
                }
            }
            if let Some(c) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) {
                if args.stat_count.is_none() {
                    args.stat_count = Some(c);
                }
            }
        }
        true
    } else {
        false
    };

    // --no-index: compare two files outside a git repository
    if args.no_index {
        return run_no_index(args);
    }

    let repo_opt = Repository::discover(None).ok();
    let precompose_paths = repo_opt.as_ref().is_some_and(|r| {
        grit_lib::precompose_config::effective_core_precomposeunicode(Some(&r.git_dir))
    });
    if precompose_paths {
        for a in args.args.iter_mut() {
            *a = grit_lib::unicode_normalization::precompose_utf8_path(a).into_owned();
        }
    }

    let raw_args: Vec<String> = std::env::args().collect();
    let has_separator = raw_args.iter().any(|a| a == "--");
    let (mut revs, raw_path_args) = parse_rev_and_paths(
        &args.args,
        has_separator,
        precompose_paths,
        repo_opt
            .as_ref()
            .and_then(|r| r.work_tree.as_deref())
            .is_some(),
    );
    // Options parsed by clap can remain in the `revs` bucket when `--` separates paths
    // (`git diff -D -- path`). Drop duplicates so they are not treated as revision names.
    if args.irreversible_delete {
        revs.retain(|r| r != "-D" && r != "--irreversible-delete");
    }
    if args.break_rewrites {
        revs.retain(|r| r != "-B" && r != "--break-rewrites");
    }

    // Outside any repository, `git diff <path> <path>` behaves like `diff --no-index` (t4035,
    // t1517). When there is no work tree, parse_rev_and_paths cannot classify args as paths
    // (it has no index/worktree to resolve pathspecs against) and instead funnels everything
    // into `revs`, so we cannot rely on raw_path_args here. Inspect the raw argv directly:
    // exactly two non-option operands, no `--` separator, no `--cached`, and both operands
    // existing on disk means git falls back to implicit `--no-index`.
    if repo_opt.is_none() && !args.cached && args.args.iter().all(|a| a != "--") {
        let operands: Vec<&String> = args.args.iter().filter(|a| !a.starts_with('-')).collect();
        if operands.len() == 2
            && operands
                .iter()
                .all(|p| std::fs::symlink_metadata(Path::new(p.as_str())).is_ok())
        {
            return run_no_index(args);
        }
    }
    // Inside a repo (or with `--`), keep the existing classification-based fallback.
    if repo_opt.is_none() && revs.is_empty() && raw_path_args.len() == 2 && !args.cached {
        return run_no_index(args);
    }

    let repo = repo_opt.context("not a git repository")?;
    let diff_config_early = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if args.order_file.is_none() {
        if let Some(p) = diff_config_early
            .get("diff.orderfile")
            .or_else(|| diff_config_early.get("diff.orderFile"))
        {
            let t = p.trim();
            if !t.is_empty() {
                args.order_file = Some(t.to_owned());
            }
        }
    }
    let quote_path_fully = diff_config_early.quote_path_fully();

    let patch_context = if let Some(u) = args.unified {
        u
    } else {
        let cfg = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
            .context("loading git config")?;
        grit_lib::config::resolve_diff_context_lines(&cfg)
            .map_err(|m| anyhow::anyhow!(m))?
            .unwrap_or(3)
    };

    if args.inter_hunk_context.is_none() {
        if let Ok(cfg) = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true) {
            if let Some(raw) = cfg.get("diff.interhunkcontext") {
                match cfg.get_i64("diff.interhunkcontext") {
                    Some(Ok(n)) if n < 0 => {
                        anyhow::bail!("negative value for 'diff.interHunkContext': '{raw}'");
                    }
                    Some(Ok(_)) => {}
                    _ => anyhow::bail!("invalid value for 'diff.interHunkContext': '{raw}'"),
                }
            }
        }
    }

    let merged_attrs = match load_gitattributes_for_diff(&repo) {
        Ok(m) => m,
        Err(Error::InvalidRef(msg)) if msg.starts_with("bad --attr-source") => {
            eprintln!("fatal: bad --attr-source or GIT_ATTR_SOURCE");
            std::process::exit(128);
        }
        Err(e) => return Err(e.into()),
    };
    let merged_attrs = Arc::new(merged_attrs);
    let diff_config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let ignore_case_attrs = diff_config
        .get("core.ignorecase")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
    let diff_algo_ctx = DiffAlgoContext {
        attrs: Arc::clone(&merged_attrs),
        config: Arc::new(diff_config.clone()),
        ignore_case_attrs,
    };
    let diff_algo_cli = parse_cli_diff_algorithm_from_argv();

    if args.submodule.as_deref().is_none_or(str::is_empty)
        && raw_args
            .iter()
            .any(|a| a == "--submodule" || a.starts_with("--submodule="))
    {
        args.submodule = Some(
            raw_args
                .iter()
                .find_map(|a| a.strip_prefix("--submodule="))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "log".to_string()),
        );
    }
    // No CLI `--submodule`: fall back to the `diff.submodule` config value (Git's
    // `git_diff_ui_config`). Plumbing (`diff-index`/`diff-tree`) ignores this and is handled in
    // its own command, so this porcelain default only affects `git diff`. See t4041/t4060 #3.
    if args.submodule.as_deref().is_none_or(str::is_empty) {
        if let Some(val) = diff_config.get("diff.submodule") {
            let v = val.trim();
            if matches!(v, "log" | "short" | "diff") {
                args.submodule = Some(v.to_string());
            }
        }
    }
    let emit_unified_patch = diff_emit_unified_patch_from_argv(&raw_args);
    let indent_heuristic = resolve_indent_heuristic(
        &diff_config_early,
        args.indent_heuristic,
        args.no_indent_heuristic,
    );

    let cwd = env::current_dir().context("failed to read current directory")?;
    let pathspec_prefix = repo
        .work_tree
        .as_ref()
        .map(|_| show_prefix(&repo, &cwd))
        .filter(|p| !p.is_empty())
        .map(|mut p| {
            p.pop(); // trailing '/'
            p
        });
    let paths: Vec<String> = raw_path_args
        .iter()
        .map(|p| {
            if let Some(wt) = repo.work_tree.as_ref() {
                resolve_pathspec(p, wt, pathspec_prefix.as_deref())
            } else {
                p.clone()
            }
        })
        .collect();
    // Resolve diff prefixes from config and command-line options
    let (src_prefix, dst_prefix) = resolve_diff_prefixes(&args, &repo, args.cached);

    // `git diff <rev>:<path> <file>` — compare a blob from history to a worktree file.
    if revs.len() == 1
        && paths.len() == 1
        && !args.cached
        && split_treeish_colon(&revs[0]).is_some()
    {
        return run_diff_blob_vs_file(
            &repo,
            &args,
            &revs[0],
            &paths[0],
            None::<&str>,
            &src_prefix,
            &dst_prefix,
            patch_context,
            Arc::clone(&merged_attrs),
            diff_config.clone(),
            ignore_case_attrs,
            diff_algo_cli,
            &cwd,
            quote_path_fully,
            indent_heuristic,
        );
    }

    // `git diff <blob-oid> <file>` — raw object id vs path (t4063: prefers filename in headers).
    if revs.len() == 1 && paths.len() == 1 && !args.cached {
        if split_treeish_colon(&revs[0]).is_none() {
            if let Some(wt) = repo.work_tree.as_ref() {
                if fs::symlink_metadata(wt.join(&paths[0])).is_ok() {
                    let oid = resolve_revision(&repo, &revs[0]).ok();
                    let is_blob = oid.is_some_and(|o| {
                        repo.odb
                            .read(&o)
                            .map(|obj| obj.kind == ObjectKind::Blob)
                            .unwrap_or(false)
                    });
                    if is_blob {
                        return run_diff_blob_vs_file(
                            &repo,
                            &args,
                            &revs[0],
                            &paths[0],
                            Some(paths[0].as_str()),
                            &src_prefix,
                            &dst_prefix,
                            patch_context,
                            Arc::clone(&merged_attrs),
                            diff_config.clone(),
                            ignore_case_attrs,
                            diff_algo_cli,
                            &cwd,
                            quote_path_fully,
                            indent_heuristic,
                        );
                    }
                }
            }
        }
    }

    // `git diff <path> <path>` outside the repo (Git: implicit `--no-index` when either path is
    // outside the work tree). When both paths are inside the repository, two pathspecs limit the
    // normal index/worktree diff (t4011: `git diff file.bin link.bin`).
    if revs.is_empty() && paths.len() == 2 && !args.cached {
        if let Some(wt) = repo.work_tree.as_ref() {
            let outside =
                !path_inside_work_tree(wt, &paths[0]) || !path_inside_work_tree(wt, &paths[1]);
            if outside {
                let d0 = strip_pathspec_cwd_prefix(&paths[0], pathspec_prefix.as_deref());
                let d1 = strip_pathspec_cwd_prefix(&paths[1], pathspec_prefix.as_deref());
                return run_diff_two_paths(
                    &repo,
                    &args,
                    &paths[0],
                    &paths[1],
                    &d0,
                    &d1,
                    &src_prefix,
                    &dst_prefix,
                );
            }
        }
    }

    // Expand A...B (symmetric diff) → merge-base(A,B)..B
    // Expand A..B → A B (two-rev diff)
    // trailing_var_arg may capture flags like --name-only into args.
    // Move them back into the flags struct so they take effect.
    let mut want_combined_diff = false;
    let mut combined_diff_dense = false;
    let mut extra_revs = Vec::new();
    let mut rev_idx = 0;
    while rev_idx < revs.len() {
        let r = &revs[rev_idx];
        if r.starts_with("--") || r.starts_with("-") && r.len() > 1 {
            // Re-apply trailing flags
            match r.as_str() {
                "-c" => {
                    want_combined_diff = true;
                    combined_diff_dense = false;
                }
                "--name-only" => args.name_only = true,
                "--name-status" => args.name_status = true,
                "--numstat" => args.numstat = true,
                "--shortstat" => args.shortstat = true,
                "--compact-summary" => args.compact_summary = true,
                "--patch-with-raw" => args.patch_with_raw = true,
                "--patch-with-stat" => args.patch_with_stat = true,
                "--summary" => args.summary = true,
                "--quiet" | "-q" => args.quiet = true,
                "--cached" | "--staged" => args.cached = true,
                s if s.starts_with("--stat-width=") => {
                    if let Some(v) = s.strip_prefix("--stat-width=").and_then(|x| x.parse().ok()) {
                        args.stat_width = Some(v);
                    }
                    stat_enabled = true;
                }
                s if s.starts_with("--stat-name-width=") => {
                    if let Some(v) = s
                        .strip_prefix("--stat-name-width=")
                        .and_then(|x| x.parse().ok())
                    {
                        args.stat_name_width = Some(v);
                    }
                    stat_enabled = true;
                }
                s if s.starts_with("--stat-graph-width=") => {
                    if let Some(v) = s
                        .strip_prefix("--stat-graph-width=")
                        .and_then(|x| x.parse().ok())
                    {
                        args.stat_graph_width = Some(v);
                    }
                    stat_enabled = true;
                }
                s if s.starts_with("--stat-count=") => {
                    if let Some(v) = s.strip_prefix("--stat-count=").and_then(|x| x.parse().ok()) {
                        args.stat_count = Some(v);
                    }
                    stat_enabled = true;
                }
                s if s == "--stat" || s.starts_with("--stat=") => {
                    if s == "--stat" {
                        if args.stat.is_none() {
                            args.stat = Some(String::new());
                        }
                    } else if let Some(val) = s.strip_prefix("--stat=") {
                        args.stat = Some(val.to_owned());
                    }
                    if let Some(ref val) = args.stat {
                        if !val.is_empty() {
                            let parts: Vec<&str> = val.split(',').collect();
                            if let Some(w) = parts.first().and_then(|s| s.parse::<usize>().ok()) {
                                if args.stat_width.is_none() {
                                    args.stat_width = Some(w);
                                }
                            }
                            if let Some(nw) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
                                if args.stat_name_width.is_none() {
                                    args.stat_name_width = Some(nw);
                                }
                            }
                            if let Some(c) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) {
                                if args.stat_count.is_none() {
                                    args.stat_count = Some(c);
                                }
                            }
                        }
                    }
                    stat_enabled = true;
                }
                "--exit-code" => args.exit_code = true,
                "--no-patch" => args.no_patch = true,
                "--raw" => args.raw = true,
                "--no-abbrev" => args.no_abbrev = true,
                "--full-index" => args.full_index = true,
                "--binary" => args.binary = true,
                s if s.starts_with("--abbrev=") => {
                    if let Some(val) = s.strip_prefix("--abbrev=") {
                        args.abbrev = val.parse().ok();
                    }
                }
                s if s.starts_with("--inter-hunk-context=") => {
                    if let Some(val) = s.strip_prefix("--inter-hunk-context=") {
                        args.inter_hunk_context = val.parse().ok();
                    }
                }
                "--no-ext-diff" => {
                    args.no_ext_diff = true;
                }
                "--no-textconv" => {
                    args.no_textconv = true;
                }
                "--check" => {
                    args.check = true;
                }
                s if s.starts_with("--ignore-submodules") => {
                    args.ignore_submodules = Some(
                        s.strip_prefix("--ignore-submodules=")
                            .unwrap_or("all")
                            .to_owned(),
                    );
                }
                s if s.starts_with("--color-moved-ws=") => {
                    args.color_moved_ws =
                        Some(s.strip_prefix("--color-moved-ws=").unwrap_or("").to_owned());
                }
                "--no-color-moved" => {
                    args.color_moved = Some("no".to_owned());
                }
                "--no-color-moved-ws" => {
                    args.color_moved_ws = Some("no".to_owned());
                }
                s if s.starts_with("--color-moved=") => {
                    args.color_moved =
                        Some(s.strip_prefix("--color-moved=").unwrap_or("").to_owned());
                }
                "--color-moved" => {
                    args.color_moved = Some("default".to_owned());
                }
                "--color" => {
                    args.color = Some("always".to_owned());
                }
                "--no-color" => {
                    args.color = Some("never".to_owned());
                }
                s if s.starts_with("--color=") => {
                    args.color = Some(s.strip_prefix("--color=").unwrap_or("").to_owned());
                }
                // `--ws-error-highlight=<kinds>` / `--ws-error-highlight <kinds>` is consumed
                // separately from raw argv by `ws_error_highlight_from_argv`; just accept it here
                // (and skip its detached value, which lands in the rev bucket) so it is not
                // rejected as an unknown option.
                s if s.starts_with("--ws-error-highlight=") => {}
                "--ws-error-highlight" => {
                    rev_idx += 1;
                }
                s if s.starts_with("-O") && s.len() > 2 => {
                    let path = s[2..].to_string();
                    if path == "/dev/null" {
                        args.order_file = None;
                    } else {
                        args.order_file = Some(path);
                    }
                }
                "--no-prefix" => {
                    args.no_prefix = true;
                }
                "--default-prefix" => {
                    args.default_prefix = true;
                }
                "--no-renames" => {
                    args.no_renames = true;
                }
                s if s.starts_with("--src-prefix=") => {
                    args.src_prefix =
                        Some(s.strip_prefix("--src-prefix=").unwrap_or("").to_owned());
                }
                s if s.starts_with("--dst-prefix=") => {
                    args.dst_prefix =
                        Some(s.strip_prefix("--dst-prefix=").unwrap_or("").to_owned());
                }
                s if s.starts_with("--line-prefix=") => {
                    args.line_prefix =
                        Some(s.strip_prefix("--line-prefix=").unwrap_or("").to_owned());
                }
                s if s.starts_with("--diff-filter=") => {
                    args.diff_filter =
                        Some(s.strip_prefix("--diff-filter=").unwrap_or("").to_owned());
                }
                s if s == "-M"
                    || (s.starts_with("-M")
                        && s.len() > 2
                        && s[2..].bytes().all(|b| b.is_ascii_digit() || b == b'%')) =>
                {
                    let val = if s.len() > 2 { &s[2..] } else { "50" };
                    args.find_renames = Some(val.to_owned());
                }
                s if s.starts_with("--find-renames") => {
                    if let Some(val) = s.strip_prefix("--find-renames=") {
                        args.find_renames = Some(val.to_owned());
                    } else {
                        args.find_renames = Some("50".to_owned());
                    }
                }
                // `--find-copies-harder` enables copy detection *and* lets unmodified
                // preimage files act as copy sources. Must precede the generic
                // `--find-copies` arm below since it shares that prefix.
                "--find-copies-harder" => {
                    args.find_copies_harder = true;
                    if args.find_copies.is_none() {
                        args.find_copies = Some("50".to_owned());
                    }
                }
                // `-C[<n>]` / `--find-copies[=<n>]` is copy detection (combined diff is `--cc`).
                // A *second* `-C` (e.g. `git diff -C -C`) requests `--find-copies-harder`,
                // matching Git's `diff_opt_parse`.
                s if s == "-C"
                    || (s.starts_with("-C")
                        && s.len() > 2
                        && s[2..].bytes().all(|b| b.is_ascii_digit() || b == b'%'))
                    || s.starts_with("--find-copies") =>
                {
                    let val = if let Some(rest) = s.strip_prefix("--find-copies=") {
                        rest.to_owned()
                    } else if s.starts_with("-C") && s.len() > 2 {
                        s[2..].to_owned()
                    } else {
                        "50".to_owned()
                    };
                    // Repeating bare `-C` / `--find-copies` upgrades to find-copies-harder.
                    if (s == "-C" || s == "--find-copies") && args.find_copies.is_some() {
                        args.find_copies_harder = true;
                    }
                    args.find_copies = Some(val);
                }
                "-l" => {
                    if rev_idx + 1 < revs.len() {
                        rev_idx += 1;
                        if let Ok(n) = revs[rev_idx].parse::<usize>() {
                            args.rename_limit = Some(n);
                        }
                    }
                }
                s if s.starts_with("-l") && s.len() > 2 => {
                    if let Ok(n) = s[2..].parse::<usize>() {
                        args.rename_limit = Some(n);
                    }
                }
                "--cc" => {
                    want_combined_diff = true;
                    combined_diff_dense = true;
                }
                s if s.starts_with("-S") => {
                    if s.len() > 2 {
                        args.pickaxe_string = Some(s[2..].to_owned());
                    } else if rev_idx + 1 < revs.len() {
                        rev_idx += 1;
                        args.pickaxe_string = Some(revs[rev_idx].clone());
                    }
                }
                s if s.starts_with("-G") => {
                    if s.len() > 2 {
                        args.pickaxe_grep = Some(s[2..].to_owned());
                    } else if rev_idx + 1 < revs.len() {
                        rev_idx += 1;
                        args.pickaxe_grep = Some(revs[rev_idx].clone());
                    }
                }
                "-I" | "--ignore-matching-lines" => {
                    if rev_idx + 1 < revs.len() {
                        rev_idx += 1;
                        args.ignore_matching_lines.push(revs[rev_idx].clone());
                    }
                }
                s if s.starts_with("-I") && s.len() > 2 => {
                    args.ignore_matching_lines.push(s[2..].to_owned());
                }
                s if s.starts_with("--ignore-matching-lines=") => {
                    args.ignore_matching_lines.push(
                        s.strip_prefix("--ignore-matching-lines=")
                            .unwrap_or("")
                            .to_owned(),
                    );
                }
                "--pickaxe-regex" => {
                    args.pickaxe_regex = true;
                }
                "--pickaxe-all" => {
                    // Accepted for compatibility
                }
                s if s == "--dirstat" => {
                    args.dirstat.push(String::new());
                }
                s if s.starts_with("--dirstat=") => {
                    args.dirstat
                        .push(s.strip_prefix("--dirstat=").unwrap_or("").to_owned());
                }
                s if s == "--dirstat-by-file" => {
                    args.dirstat_by_file = Some(String::new());
                }
                s if s.starts_with("--dirstat-by-file=") => {
                    args.dirstat_by_file = Some(
                        s.strip_prefix("--dirstat-by-file=")
                            .unwrap_or("")
                            .to_owned(),
                    );
                }
                "--cumulative" => {
                    args.dirstat_cumulative_flag = true;
                }
                "-W" | "--function-context" => {
                    args.function_context = true;
                }
                // `git diff` is always recursive; `-r` / `-t` are accepted as no-ops for
                // compatibility with diff-tree-style invocations.
                "-r" | "-t" => {}
                s if s.starts_with("--") => {
                    return Err(anyhow::Error::new(ExplicitExit {
                        code: 129,
                        message: format!(
                            "error: unknown option `{}`\nusage: git diff [<options>] [<commit>] [--] [<path>...]",
                            s.trim_start_matches('-')
                        ),
                    }));
                }
                _ => {
                    extra_revs.push(r.clone());
                    rev_idx += 1;
                    continue;
                }
            }
        } else {
            extra_revs.push(r.clone());
        }
        rev_idx += 1;
    }
    revs = extra_revs;
    // `git diff <commit> <commit>...<commit>` (3+ commits) produces an N-way combined diff:
    // the FIRST commit is the merge result, the rest are parents (builtin/diff.c
    // `builtin_diff_combined`), and it defaults to the dense combined (`--cc`) format.
    let auto_combined = revs.len() >= 3 && !args.cached && !want_combined_diff;
    if revs.len() >= 3 && !args.cached {
        want_combined_diff = true;
        if auto_combined {
            combined_diff_dense = true;
        }
    }

    let symmetric_tokens = revs.iter().filter(|r| is_symmetric_diff(r)).count();
    let two_dot_range_tokens = revs
        .iter()
        .filter(|r| r.contains("..") && !is_symmetric_diff(r))
        .count();
    // A range argument (`A..B` or `A...B`) cannot be combined with any other
    // revision. This mirrors builtin/diff.c's `symdiff_prepare`, which rejects
    // "git diff A..B C", "git diff C A...B", "git diff A..B C..D", etc. with the
    // usage message (the C check is `lpos >= 0 && othercount > 0`).
    if symmetric_tokens > 1
        || (symmetric_tokens == 1 && revs.len() != 1)
        || two_dot_range_tokens > 1
        || (two_dot_range_tokens == 1 && revs.len() != 1)
    {
        bail!("usage: grit diff [<options>] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] --cached [--merge-base] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] [--merge-base] <commit> [<commit>...] <commit> [--] [<path>...]");
    }
    if revs.len() > 2 && !want_combined_diff {
        bail!("usage: grit diff [<options>] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] --cached [--merge-base] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] [--merge-base] <commit> [<commit>...] <commit> [--] [<path>...]");
    }

    if args.merge_base {
        if args.cached {
            if revs.len() != 1 {
                bail!("fatal: --merge-base does not work with ranges");
            }
            if revs[0].contains("..") && !is_symmetric_diff(&revs[0]) {
                bail!("fatal: --merge-base does not work with ranges");
            }
            let head_oid = resolve_revision_as_commit(&repo, "HEAD")
                .map_err(|e| anyhow::anyhow!("unable to get HEAD: {e}"))?;
            let other = resolve_commit_ish_for_merge_base(&repo, &revs[0])?;
            let mb_oid = match merge_base_for_diff_index(&repo, head_oid, other) {
                Ok(oid) => oid,
                Err(MergeBaseForDiffError::None) => bail!("fatal: no merge base found"),
                Err(MergeBaseForDiffError::Multiple) => bail!("fatal: multiple merge bases found"),
                Err(MergeBaseForDiffError::Other(msg)) => bail!("{msg}"),
            };
            revs = vec![mb_oid.to_hex()];
        } else if revs.len() == 1 && revs[0].contains("..") && !is_symmetric_diff(&revs[0]) {
            bail!("fatal: --merge-base does not work with ranges");
        } else if revs.is_empty() {
            bail!("usage: grit diff [<options>] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] --cached [--merge-base] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] [--merge-base] <commit> [<commit>...] <commit> [--] [<path>...]");
        } else if revs.len() > 2 {
            bail!("usage: grit diff [<options>] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] --cached [--merge-base] [<commit>] [--] [<path>...]\n   or: grit diff [<options>] [--merge-base] <commit> [<commit>...] <commit> [--] [<path>...]");
        } else if revs.len() == 2 {
            let a = resolve_commit_ish_for_merge_base(&repo, &revs[0])?;
            let b = resolve_commit_ish_for_merge_base(&repo, &revs[1])?;
            let mb_oid = match merge_base_for_diff_two_commits(&repo, a, b) {
                Ok(oid) => oid,
                Err(MergeBaseForDiffError::None) => bail!("fatal: no merge base found"),
                Err(MergeBaseForDiffError::Multiple) => bail!("fatal: multiple merge bases found"),
                Err(MergeBaseForDiffError::Other(msg)) => bail!("{msg}"),
            };
            revs = vec![mb_oid.to_hex(), revs[1].clone()];
        } else {
            // one revision: merge-base(HEAD, rev) vs index/worktree
            let head_oid = resolve_revision_as_commit(&repo, "HEAD")
                .map_err(|e| anyhow::anyhow!("unable to get HEAD: {e}"))?;
            let other = resolve_commit_ish_for_merge_base(&repo, &revs[0])?;
            let mb_oid = match merge_base_for_diff_index(&repo, head_oid, other) {
                Ok(oid) => oid,
                Err(MergeBaseForDiffError::None) => bail!("fatal: no merge base found"),
                Err(MergeBaseForDiffError::Multiple) => bail!("fatal: multiple merge bases found"),
                Err(MergeBaseForDiffError::Other(msg)) => bail!("{msg}"),
            };
            revs = vec![mb_oid.to_hex()];
        }
    }

    let mut symmetric_warn_multiple_bases = false;
    let mut symmetric_left_name = String::new();
    let mut symmetric_right_name = String::new();
    let mut symmetric_base_name = String::new();

    // `git diff -C -C` (a repeated copy-detection flag) requests `--find-copies-harder`,
    // letting unmodified preimage files act as copy sources. clap collapses the two `-C`
    // tokens (`overrides_with`), so inspect raw argv directly: an explicit
    // `--find-copies-harder`, or two or more bare `-C`/`--find-copies` flags.
    {
        let mut copy_flag_count = 0usize;
        for a in raw_args.iter() {
            if a == "--find-copies-harder" {
                args.find_copies_harder = true;
                if args.find_copies.is_none() {
                    args.find_copies = Some("50".to_owned());
                }
            } else if a == "-C" || a == "--find-copies" {
                copy_flag_count += 1;
            }
        }
        if copy_flag_count >= 2 {
            args.find_copies_harder = true;
        }
    }

    // -C implies -M (copy detection requires rename detection)
    if args.find_copies.is_some() && args.find_renames.is_none() {
        args.find_renames = Some("50".to_owned());
    }
    if args.rename_limit.is_some_and(|n| n > 0) && args.find_copies.is_some() {
        eprintln!("warning: exhaustive rename detection was skipped due to too many files");
    }

    let rename_threshold: Option<u32> = if args.no_renames {
        None
    } else if let Some(ref threshold_str) = args.find_renames {
        Some(parse_diff_rename_threshold(threshold_str))
    } else {
        use grit_lib::config::ConfigSet;
        let config =
            ConfigSet::load(Some(&repo.git_dir), false).unwrap_or_else(|_| ConfigSet::new());
        match config.get("diff.renames") {
            Some(val) => {
                let val_lower = val.to_lowercase();
                match val_lower.as_str() {
                    "false" | "no" | "0" => None,
                    "true" | "yes" | "1" | "" => Some(50),
                    "copies" | "copy" => Some(50),
                    _ => None,
                }
            }
            None => Some(50),
        }
    };

    let mut _symmetric = false;
    if revs.len() == 1 {
        if revs[0].ends_with("^!") {
            let expanded = expand_rev_token_circ_bang(&repo, &revs[0])?;
            if expanded.len() > 2 {
                // Merge commit: `rev^!` excludes all parents, which produces a
                // combined diff (merge result against every parent), defaulting
                // to the dense `--cc` format like git.
                let commit_oid = resolve_revision(&repo, &expanded[0])
                    .with_context(|| format!("unknown revision: '{}'", expanded[0]))?;
                let mut new_revs = vec![commit_oid.to_hex()];
                for p in &expanded[1..] {
                    let parent_spec = p.strip_prefix('^').unwrap_or(p.as_str());
                    let parent_oid = resolve_revision(&repo, parent_spec)
                        .with_context(|| format!("unknown revision: '{parent_spec}'"))?;
                    new_revs.push(parent_oid.to_hex());
                }
                revs = new_revs;
                if !want_combined_diff {
                    want_combined_diff = true;
                    combined_diff_dense = true;
                }
            } else if expanded.len() == 2 {
                let parent_spec = expanded[1]
                    .strip_prefix('^')
                    .unwrap_or(expanded[1].as_str());
                let parent_oid = resolve_revision(&repo, parent_spec)
                    .with_context(|| format!("unknown revision: '{parent_spec}'"))?;
                let commit_oid = resolve_revision(&repo, &expanded[0])
                    .with_context(|| format!("unknown revision: '{}'", expanded[0]))?;
                revs = vec![parent_oid.to_hex(), commit_oid.to_hex()];
            }
        } else if let Some((left_spec, right_spec)) = try_treeish_blob_range(&revs[0]) {
            revs = vec![left_spec, right_spec];
        } else if let Some((left, right)) = revs[0].split_once("...") {
            let left = if left.is_empty() { "HEAD" } else { left };
            let right = if right.is_empty() { "HEAD" } else { right };
            symmetric_left_name = left.to_string();
            symmetric_right_name = right.to_string();
            let left_oid = resolve_revision(&repo, left)
                .with_context(|| format!("unknown revision: '{left}'"))?;
            let right_oid = resolve_revision(&repo, right)
                .with_context(|| format!("unknown revision: '{right}'"))?;
            let bases = grit_lib::rev_list::merge_bases(&repo, left_oid, right_oid, false)?;
            if bases.is_empty() {
                bail!("fatal: {}: no merge base", revs[0]);
            }
            symmetric_warn_multiple_bases = bases.len() > 1;
            let chosen = bases[0];
            symmetric_base_name = abbreviate_object_id(&repo, chosen, 7)
                .unwrap_or_else(|_| chosen.to_hex()[..7.min(40)].to_string());
            revs = vec![chosen.to_string(), right_oid.to_string()];
            _symmetric = true;
        } else if let Some((left, right)) = revs[0].split_once("..") {
            let left = if left.is_empty() { "HEAD" } else { left };
            let right = if right.is_empty() { "HEAD" } else { right };
            revs = vec![left.to_owned(), right.to_owned()];
        }
    }

    if symmetric_warn_multiple_bases {
        eprintln!(
            "warning: {}...{}: multiple merge bases, using {}",
            symmetric_left_name, symmetric_right_name, symmetric_base_name
        );
    }
    if want_combined_diff && revs.len() >= 3 && !args.cached {
        fn tree_oid_for_diff_spec(repo: &Repository, spec: &str) -> Result<ObjectId> {
            commit_or_tree_oid(repo, spec)
        }
        // The first revision is the merge result; the remaining ones are its parents
        // (git's `first_non_parent` for `diff A B C...`).
        let merge_rev = revs
            .first()
            .ok_or_else(|| anyhow!("combined diff requires at least one revision"))?;
        let merge_tree = tree_oid_for_diff_spec(&repo, merge_rev)?;
        let mut parent_trees = Vec::with_capacity(revs.len() - 1);
        for s in &revs[1..] {
            parent_trees.push(tree_oid_for_diff_spec(&repo, s)?);
        }
        let walk = CombinedTreeDiffOptions {
            recursive: true,
            tree_in_recursive: false,
        };
        let parent_opts: Vec<Option<ObjectId>> = parent_trees.iter().copied().map(Some).collect();
        let mut cpaths =
            combined_diff_paths_trees(&repo.odb, &merge_tree, &parent_opts, &walk, None)?;
        cpaths = filter_combined_paths(cpaths, &paths);
        // `-O<orderfile>` applies to combined diffs too (t4056).
        if let Some(ref order_path) = args.order_file {
            let patterns = grit_lib::diff::read_orderfile_patterns(order_path, &cwd)?;
            cpaths.sort_by_key(|p| {
                patterns
                    .iter()
                    .position(|pat| grit_lib::diff::orderfile_pattern_matches(pat, &p.path))
                    .unwrap_or(patterns.len())
            });
        }
        let has_diff = !cpaths.is_empty();
        let abbrev_opt = if args.full_index || args.no_abbrev {
            None
        } else {
            Some(args.abbrev.map(|n| n.max(4).min(40)).unwrap_or(7))
        };
        let mut stdout = io::stdout().lock();
        if args.name_only {
            for p in &cpaths {
                writeln!(
                    stdout,
                    "{}",
                    grit_lib::quote_path::quote_c_style(&p.path, quote_path_fully)
                )?;
            }
        } else if args.raw {
            for p in &cpaths {
                writeln!(stdout, "{}", format_combined_raw_line(p, abbrev_opt))?;
            }
        } else if emit_unified_patch && !args.no_patch {
            let ws = CombinedDiffWsOptions {
                ignore_all_space: args.ignore_all_space,
                ignore_space_change: args.ignore_space_change,
                ignore_space_at_eol: args.ignore_space_at_eol,
                ignore_cr_at_eol: args.ignore_cr_at_eol,
            };
            let rename_thresh = args
                .find_renames
                .as_deref()
                .map(parse_diff_rename_threshold)
                .unwrap_or(50);
            let patch_abbrev = abbrev_opt.unwrap_or(40);
            for p in &cpaths {
                let parent_blob_paths = if args.find_renames.is_some() {
                    combined_merge_parent_blob_paths(
                        &repo.odb,
                        &p.path,
                        &parent_trees,
                        &merge_tree,
                        rename_thresh,
                    )
                } else {
                    None
                };
                if let Some(patch) = format_combined_textconv_patch(
                    &repo.git_dir,
                    &diff_config,
                    &repo.odb,
                    &p.path,
                    &parent_trees,
                    &merge_tree,
                    patch_abbrev,
                    patch_context,
                    combined_diff_dense,
                    !args.no_textconv,
                    ws,
                    false,
                    parent_blob_paths.as_deref(),
                    &p.parents,
                    quote_path_fully,
                ) {
                    match args.line_prefix.as_deref() {
                        Some(prefix) if !prefix.is_empty() => {
                            for line in patch.split_inclusive('\n') {
                                write!(stdout, "{prefix}{line}")?;
                            }
                        }
                        _ => write!(stdout, "{patch}")?,
                    }
                }
            }
        }
        if args.exit_code || args.quiet {
            if has_diff {
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let work_tree = repo.work_tree.as_deref();

    // Load index (empty if not found)
    let index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e.into()),
    };
    // `git diff` walks the index through unpack-trees' cache-tree fast path, which aborts on a
    // cache-tree whose entry counts run past the index (a tree with duplicate path entries —
    // t4058-diff-duplicates). Unlike the index-write check, this guard is not gated by
    // GIT_TEST_CHECK_CACHE_TREE; a well-formed index always verifies cleanly.
    grit_lib::write_tree::verify_cache_tree(&index)?;
    let merged_attrs = if args.cached {
        Arc::new(load_cached_diff_gitattributes(&repo, &index)?)
    } else {
        merged_attrs
    };

    // Get HEAD tree OID (None if unborn)
    let head_tree = get_head_tree(&repo)?;

    // Determine whether worktree is involved (for content fallback)
    let wt_for_content: Option<&Path> = match (args.cached, revs.len()) {
        (true, _) => None,       // --cached: index vs tree, no worktree
        (false, 0) => work_tree, // unstaged: index vs worktree
        (false, 1) => work_tree, // one rev: tree vs worktree
        (_, 2) => None,          // two revs: tree vs tree
        (_, 3) if want_combined_diff && args.name_only && !args.cached => None,
        _ => None,
    };

    let entries: Vec<DiffEntry> = if want_combined_diff
        && revs.len() == 3
        && args.name_only
        && !args.cached
    {
        let merge_oid = resolve_revision(&repo, &revs[0])
            .with_context(|| format!("unknown revision: '{}'", revs[0]))?;
        let p_a = resolve_revision(&repo, &revs[1])
            .with_context(|| format!("unknown revision: '{}'", revs[1]))?;
        let p_b = resolve_revision(&repo, &revs[2])
            .with_context(|| format!("unknown revision: '{}'", revs[2]))?;
        let merge_obj = repo
            .odb
            .read(&merge_oid)
            .with_context(|| format!("reading object {merge_oid}"))?;
        if merge_obj.kind != ObjectKind::Commit {
            bail!("combined diff requires a merge commit");
        }
        let merge_commit = parse_commit(&merge_obj.data).context("parsing merge commit")?;
        if merge_commit.parents.len() != 2 {
            bail!("combined diff requires a merge commit with exactly two parents");
        }
        let parents_ok = merge_commit.parents == [p_a, p_b] || merge_commit.parents == [p_b, p_a];
        if !parents_ok {
            bail!("combined diff: revisions do not match merge parents");
        }
        let names = combined_diff_paths(&repo.odb, &merge_commit.tree, &[p_a, p_b]);
        let z = zero_oid();
        names
            .into_iter()
            // TODO(byte-paths): combined_diff_paths yields lossy String paths (Phase 2/4).
            .map(|p| DiffEntry {
                status: DiffStatus::Modified,
                old_path: Some(RepoPathBuf::from_string(p.clone())),
                new_path: Some(RepoPathBuf::from_string(p)),
                old_mode: "100644".to_string(),
                new_mode: "100644".to_string(),
                old_oid: z,
                new_oid: z,
                score: None,
            })
            .collect()
    } else {
        match (args.cached, revs.len()) {
            (true, 0) => {
                // --cached with no revision: index vs HEAD
                diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)?
            }
            (true, 1) => {
                // --cached with one revision: index vs that commit's tree
                let tree_oid = commit_or_tree_oid(&repo, &revs[0])?;
                diff_index_to_tree(&repo.odb, &index, Some(&tree_oid), false)?
            }
            (false, 0) => {
                // No flags: unstaged changes (index vs worktree)
                let wt = work_tree
                    .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
                diff_index_to_worktree(&repo.odb, &index, wt, false, false)?
            }
            (false, 1) => {
                // One revision: tree vs worktree
                let tree_oid = commit_or_tree_oid(&repo, &revs[0])?;
                let wt = work_tree
                    .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
                diff_tree_to_worktree(&repo.odb, Some(&tree_oid), wt, &index)?
            }
            (_, 2) => {
                match (
                    blob_side_for_blob_diff_spec(&repo, &revs[0])?,
                    blob_side_for_blob_diff_spec(&repo, &revs[1])?,
                ) {
                    (Some(a), Some(b)) => {
                        if a.oid == b.oid && a.mode == b.mode {
                            Vec::new()
                        } else {
                            vec![DiffEntry {
                                status: DiffStatus::Modified,
                                // TODO(byte-paths): blob-diff spec paths are lossy String (Phase 2/4).
                                old_path: Some(RepoPathBuf::from_string(a.path)),
                                new_path: Some(RepoPathBuf::from_string(b.path)),
                                old_mode: a.mode,
                                new_mode: b.mode,
                                old_oid: a.oid,
                                new_oid: b.oid,
                                score: None,
                            }]
                        }
                    }
                    _ => {
                        let tree1 = commit_or_tree_oid(&repo, &revs[0])?;
                        let tree2 = commit_or_tree_oid(&repo, &revs[1])?;
                        diff_trees(&repo.odb, Some(&tree1), Some(&tree2), "")?
                    }
                }
            }
            _ => {
                bail!("too many revisions");
            }
        }
    };

    // Filter by pathspecs
    let entries = filter_by_paths(entries, &paths);

    // Build whitespace mode from flags
    let ws_mode = WhitespaceMode {
        ignore_all_space: args.ignore_all_space,
        ignore_space_change: args.ignore_space_change,
        ignore_space_at_eol: args.ignore_space_at_eol,
        ignore_blank_lines: args.ignore_blank_lines,
        ignore_cr_at_eol: args.ignore_cr_at_eol,
    };

    let line_ignore_res: Option<Vec<Regex>> = if args.ignore_matching_lines.is_empty() {
        None
    } else {
        let mut compiled = Vec::with_capacity(args.ignore_matching_lines.len());
        for p in &args.ignore_matching_lines {
            match Regex::new(p) {
                Ok(re) => compiled.push(re),
                Err(_) => {
                    eprintln!("error: invalid regex given to -I: {p}");
                    std::process::exit(129);
                }
            }
        }
        Some(compiled)
    };
    let line_ignore = line_ignore_res.as_deref();

    // When a whitespace-ignore mode is active, filter out entries whose
    // normalised content is identical. Deletions, additions, and mode
    // changes are always reported regardless of whitespace.
    //
    // `--ignore-blank-lines` (alone) is handled by xdiff-style hunk suppression below so that
    // line numbers stay intact, so it is excluded from this normalization-equality filter.
    let entries = if ws_mode.any() && !ws_mode.only_blank_lines() {
        entries
            .into_iter()
            .filter(|e| {
                // Always keep deletions, additions, and mode changes
                if e.status == DiffStatus::Deleted || e.status == DiffStatus::Added {
                    return true;
                }
                if e.old_mode != e.new_mode {
                    return true;
                }
                let old = read_content(&repo.odb, &e.old_oid, None, e.path());
                let new = read_content(&repo.odb, &e.new_oid, wt_for_content, e.path());
                ws_mode.normalize(&old) != ws_mode.normalize(&new)
            })
            .collect()
    } else {
        entries
    };

    let entries = if line_ignore.is_some() || ws_mode.ignore_blank_lines {
        let empty: &[Regex] = &[];
        let ign = line_ignore.unwrap_or(empty);
        entries
            .into_iter()
            .filter(|e| !entry_hidden_by_line_ignore(e, &repo.odb, wt_for_content, &ws_mode, ign))
            .collect()
    } else {
        entries
    };

    let needs_blob_prefetch_before_rename = wt_for_content.is_none()
        && (args.shortstat
            || stat_enabled
            || args.stat_count.is_some()
            || args.stat_width.is_some()
            || args.stat_graph_width.is_some()
            || args.stat_name_width.is_some()
            || args.numstat
            || !args.dirstat.is_empty()
            || args.dirstat_by_file.is_some()
            || (emit_unified_patch
                && !args.no_patch
                && !args.raw
                && !args.name_only
                && !args.name_status));
    prefetch_promisor_for_diff_entries(
        &repo,
        &entries,
        wt_for_content,
        PromisorDiffPrefetch {
            rename_detection: rename_threshold.is_some(),
            break_rewrites: false,
            needs_blob_content: needs_blob_prefetch_before_rename,
        },
    );

    let entries = if let Some(threshold) = rename_threshold {
        if args.find_copies.is_some() {
            // `--find-copies-harder` (`-C -C`) lets *unmodified* preimage files act as copy
            // sources. Git seeds the rename pool with the whole preimage; replicate that by
            // flattening the source tree into (path, mode, oid) tuples. Only the tree-based
            // diff modes can supply a flat preimage cheaply; for index/worktree sources we
            // fall back to the modified-only behaviour (empty slice).
            //
            // A rename limit (`-l <n>`) being hit disables the exhaustive scan: Git degrades
            // to "only found copies from modified paths", so the unmodified preimage must not
            // be added as copy sources (t4001 "many rename source candidates").
            let rename_limited = args.rename_limit.is_some_and(|n| n > 0);
            let source_tree_entries: Vec<(String, String, ObjectId)> =
                if args.find_copies_harder && !rename_limited {
                    source_tree_oid_for_find_copies_harder(&repo, &args, &revs, head_tree.as_ref())
                        .and_then(|t| grit_lib::diff::head_path_states(&repo.odb, Some(&t)).ok())
                        .map(|m| {
                            m.into_iter()
                                .map(|(p, (mode, oid))| (p, grit_lib::diff::format_mode(mode), oid))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
            detect_copies(
                &repo.odb,
                None,
                entries,
                threshold,
                args.find_copies_harder,
                &source_tree_entries,
            )
        } else {
            detect_renames(&repo.odb, None, entries, threshold)
        }
    } else {
        entries
    };

    // Submodule ignore: `--ignore-submodules`, `diff.ignoreSubmodules`, and `submodule.*.ignore`
    // (matches Git combined ordering for t4027).
    let ignore_sm = args.ignore_submodules.as_deref();
    let path_to_sm_name = work_tree
        .map(|wt| submodule_path_to_name_map(wt, &diff_config))
        .unwrap_or_default();
    let gm_sub_ignore = work_tree
        .map(gitmodules_submodule_name_to_ignore)
        .unwrap_or_default();
    let entries: Vec<DiffEntry> = entries
        .into_iter()
        .filter(|e| {
            !(gitlink_suppressed_ignore_all(
                e,
                ignore_sm,
                &path_to_sm_name,
                &gm_sub_ignore,
                &diff_config,
            ) || gitlink_same_oid_suppressed(
                e,
                ignore_sm,
                &path_to_sm_name,
                &gm_sub_ignore,
                &diff_config,
                work_tree,
            ))
        })
        .collect();

    let entries = if let Some(ref df) = args.diff_filter {
        if df.is_empty() {
            entries
        } else {
            apply_diff_filter(entries, df)
        }
    } else {
        entries
    };

    // Apply pickaxe filtering (-G <regex> or -S <string> [--pickaxe-regex]).
    let entries = if let Some(ref pattern) = args.pickaxe_grep {
        // -G: show only entries whose diff text has added/removed lines matching the regex
        let re = regex::Regex::new(pattern)
            .with_context(|| format!("invalid pickaxe regex: {pattern}"))?;
        entries
            .into_iter()
            .filter(|e| {
                let old = read_content(&repo.odb, &e.old_oid, None, e.path());
                let new = read_content(&repo.odb, &e.new_oid, wt_for_content, e.path());
                // Check if any added or removed line matches
                for line in new.lines() {
                    if re.is_match(line) {
                        return true;
                    }
                }
                for line in old.lines() {
                    if re.is_match(line) {
                        return true;
                    }
                }
                false
            })
            .collect()
    } else if let Some(ref needle) = args.pickaxe_string {
        if args.pickaxe_regex {
            // -S with --pickaxe-regex: treat needle as regex, filter by occurrence count change
            let re = regex::Regex::new(needle)
                .with_context(|| format!("invalid pickaxe regex: {needle}"))?;
            entries
                .into_iter()
                .filter(|e| {
                    let old = read_content(&repo.odb, &e.old_oid, None, e.path());
                    let new = read_content(&repo.odb, &e.new_oid, wt_for_content, e.path());
                    let old_count = re.find_iter(&old).count();
                    let new_count = re.find_iter(&new).count();
                    old_count != new_count
                })
                .collect()
        } else {
            // -S without --pickaxe-regex: filter by string occurrence count change
            entries
                .into_iter()
                .filter(|e| {
                    let old = read_content(&repo.odb, &e.old_oid, None, e.path());
                    let new = read_content(&repo.odb, &e.new_oid, wt_for_content, e.path());
                    let old_count = old.matches(needle.as_str()).count();
                    let new_count = new.matches(needle.as_str()).count();
                    old_count != new_count
                })
                .collect()
        }
    } else {
        entries
    };

    // Apply --relative path prefix stripping.
    let entries = {
        let prefix = resolve_diff_relative_prefix(work_tree, &repo.git_dir, &args);
        if let Some(ref pfx) = prefix {
            entries
                .into_iter()
                .filter_map(|mut e| {
                    // Filter: at least one path must be under the prefix
                    let old_match = e
                        .old_path
                        .as_ref()
                        .is_some_and(|p| p.starts_with_bytes(pfx.as_bytes()));
                    let new_match = e
                        .new_path
                        .as_ref()
                        .is_some_and(|p| p.starts_with_bytes(pfx.as_bytes()));
                    if !old_match && !new_match {
                        return None;
                    }
                    // Strip prefix from paths, then strip leading '/' (byte-wise).
                    let strip = |p: &mut RepoPathBuf| {
                        if let Some(stripped) = p.as_bytes().strip_prefix(pfx.as_bytes()) {
                            let mut t = stripped;
                            while let Some(rest) = t.strip_prefix(b"/") {
                                t = rest;
                            }
                            *p = RepoPathBuf::from_bytes(t.to_vec());
                        }
                    };
                    if let Some(ref mut p) = e.old_path {
                        strip(p);
                    }
                    if let Some(ref mut p) = e.new_path {
                        strip(p);
                    }
                    Some(e)
                })
                .collect()
        } else {
            entries
        }
    };

    // Apply orderfile sorting if specified
    let entries = if let Some(ref order_path) = args.order_file {
        grit_lib::diff::apply_orderfile_entries(entries, order_path, &cwd)?
    } else {
        entries
    };

    let entries = grit_lib::diff::apply_rotate_skip_entries(
        entries,
        args.rotate_to.as_deref(),
        args.skip_to.as_deref(),
    )?;

    // Apply -R: reverse the diff (swap old and new sides)
    let mut entries = if args.reverse {
        entries
            .into_iter()
            .map(|mut e| {
                std::mem::swap(&mut e.old_oid, &mut e.new_oid);
                std::mem::swap(&mut e.old_mode, &mut e.new_mode);
                // Swap paths for every entry, including additions/deletions where one side
                // is `None`. The previous `if let Some(old_path)` branch skipped Added files
                // and produced invalid patches (`--- /dev/null` + `+++ /dev/null`).
                std::mem::swap(&mut e.old_path, &mut e.new_path);
                // Invert the status
                e.status = match e.status {
                    grit_lib::diff::DiffStatus::Added => grit_lib::diff::DiffStatus::Deleted,
                    grit_lib::diff::DiffStatus::Deleted => grit_lib::diff::DiffStatus::Added,
                    other => other,
                };
                e
            })
            .collect()
    } else {
        entries
    };

    let dirstat_cli_active = !args.dirstat.is_empty() || args.dirstat_by_file.is_some();
    let (dirstat_opts, dirstat_config_warnings) =
        resolve_dirstat_options(&args, &repo.git_dir, dirstat_cli_active)?;
    let relative_prefix_for_paths = resolve_diff_relative_prefix(work_tree, &repo.git_dir, &args);

    if args.break_rewrites {
        prefetch_promisor_for_diff_entries(
            &repo,
            &entries,
            wt_for_content,
            PromisorDiffPrefetch {
                rename_detection: false,
                break_rewrites: true,
                needs_blob_content: false,
            },
        );
        for entry in &mut entries {
            if entry.status != DiffStatus::Modified {
                continue;
            }
            let old_raw = read_content_raw(&repo.odb, &entry.old_oid);
            let new_raw = read_content_raw_or_worktree(
                &repo.odb,
                &entry.new_oid,
                wt_for_content,
                entry.path(),
            );
            if should_break_rewrite_for_stat(&old_raw, &new_raw) {
                if let Some(pct) = rewrite_dissimilarity_index_percent(&old_raw, &new_raw) {
                    entry.score = Some(pct);
                }
            }
        }
    }

    if args.compact_summary {
        stat_enabled = true;
    }
    let stat_block_active = stat_enabled
        || args.stat_count.is_some()
        || args.stat_width.is_some()
        || args.stat_graph_width.is_some()
        || args.stat_name_width.is_some();
    let show_unified_patch = diff_show_unified_patch_last_wins(&raw_args);
    let format_besides_unified_patch = args.shortstat
        || stat_block_active
        || args.raw
        || args.numstat
        || args.name_only
        || args.name_status
        || (args.summary && !stat_block_active)
        || dirstat_opts.is_some()
        || args.compact_summary
        || args.patch_with_raw
        || args.patch_with_stat;

    let has_entries_for_exit = !entries.is_empty();
    let mut conflict_combined_patches: Vec<String> = Vec::new();
    // Combined `diff --cc` for unmerged paths: required for conflicted index entries even when
    // `MERGE_HEAD` is absent (e.g. t4027 `update-index --index-info` submodule conflict).
    if !args.cached && revs.is_empty() && work_tree.is_some() {
        let mut conflict_paths: Vec<String> = entries
            .iter()
            .filter(|e| e.status == DiffStatus::Unmerged)
            .map(|e| e.path().to_string())
            .collect();
        conflict_paths.sort();
        conflict_paths.dedup();
        if !conflict_paths.is_empty() {
            use grit_lib::config::ConfigSet;
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            let patch_abbrev = if args.full_index {
                40usize
            } else if let Some(n) = args.abbrev {
                n.max(4).min(40)
            } else {
                7
            };
            if let Some(wt) = work_tree {
                for display_path in &conflict_paths {
                    let repo_path = repo_relative_path_for_relative_display(
                        display_path,
                        relative_prefix_for_paths.as_deref(),
                    );
                    let key = repo_path.as_bytes();
                    let Some(e2) = index.get(key, 2) else {
                        continue;
                    };
                    let Some(e3) = index.get(key, 3) else {
                        continue;
                    };
                    let e1 = index.get(key, 1);
                    // Two-way merges (add/add, etc.) omit stage 1; `format_worktree_conflict_combined`
                    // still needs three OIDs — Git uses the null OID for the missing base.
                    let e1_oid = e1.map(|e| e.oid).unwrap_or_else(zero_oid);
                    let file_path = wt.join(&repo_path);
                    let wt_bytes = match std::fs::symlink_metadata(&file_path) {
                        Ok(m) if m.is_dir() => Vec::new(),
                        _ => std::fs::read(&file_path).unwrap_or_default(),
                    };
                    let patch = if e1.is_some_and(|e| {
                        e.mode == MODE_GITLINK && e2.mode == MODE_GITLINK && e3.mode == MODE_GITLINK
                    }) {
                        let result_line = if wt_bytes.is_empty() {
                            if file_path.join(".git").exists() {
                                resolve_revision(&repo, "HEAD")
                                    .map(|oid| format!("Subproject commit {}", oid.to_hex()))
                                    .unwrap_or_else(|_| {
                                        format!("Subproject commit {}", zero_oid().to_hex())
                                    })
                            } else {
                                format!("Subproject commit {}", zero_oid().to_hex())
                            }
                        } else {
                            String::from_utf8_lossy(&wt_bytes).into_owned()
                        };
                        format_gitlink_unmerged_conflict_combined(
                            display_path,
                            &e2.oid,
                            &e3.oid,
                            &result_line,
                            patch_abbrev,
                        )
                    } else {
                        format_worktree_conflict_combined(
                            &repo.git_dir,
                            &config,
                            &repo.odb,
                            display_path,
                            &e1_oid,
                            &e2.oid,
                            &e3.oid,
                            &wt_bytes,
                            patch_abbrev,
                        )
                    };
                    conflict_combined_patches.push(patch);
                }
            }
            // Git keeps index↔worktree `U`/`M` lines for `--raw` / `--name-only` during conflicts;
            // combined `diff --cc` replaces them only when unified patch is the sole format.
            let strip_conflict_index_lines = show_unified_patch && !format_besides_unified_patch;
            if strip_conflict_index_lines {
                entries.retain(|e| !conflict_paths.iter().any(|p| e.path() == p.as_str()));
            }
        }
    }

    let has_diff =
        has_entries_for_exit || !entries.is_empty() || !conflict_combined_patches.is_empty();

    let stdout_is_tty = io::stdout().is_terminal();
    let use_color = diff_use_color(
        args.color.as_deref(),
        &diff_config,
        args.output_path.as_deref(),
    );

    let out: Box<dyn Write> = if let Some(ref p) = args.output_path {
        let resolved = if p.is_absolute() {
            p.clone()
        } else {
            cwd.join(p)
        };
        Box::new(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&resolved)
                .with_context(|| format!("failed to open output file {}", resolved.display()))?,
        )
    } else {
        Box::new(io::stdout())
    };
    // `--line-prefix=<p>` prepends `<p>` to every emitted line; wrap the writer so all
    // sections (stat, raw, patch) share the same prefix and the per-section emitters can
    // stay prefix-agnostic.
    let mut out: Box<dyn Write> = match args.line_prefix.as_deref() {
        Some(p) if !p.is_empty() => Box::new(LinePrefixWriter::new(out, p)),
        _ => out,
    };

    let effective_word_diff_opt = effective_word_diff(&args, stdout_is_tty);
    let use_color_patch = use_color
        || effective_word_diff_opt
            .as_ref()
            .is_some_and(|w| w.force_color);

    // Check diff.suppressBlankEmpty config
    let suppress_blank_empty = {
        use grit_lib::config::ConfigSet;
        let config =
            ConfigSet::load(Some(&repo.git_dir), false).unwrap_or_else(|_| ConfigSet::new());
        config
            .get("diff.suppressBlankEmpty")
            .map(|v| v == "true")
            .unwrap_or(false)
    };

    // `--check`: whitespace / conflict-marker diagnostics. With `--exit-code`, Git uses 1 for a
    // diff that passes the check and 3 when the check fails; without `--exit-code`, a failed
    // check exits 2 (see t4017-diff-retval).
    if args.check {
        let config_for_attrs = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let has_ws_errors = check_whitespace_errors(
            &mut out,
            &entries,
            &repo.odb,
            wt_for_content,
            merged_attrs.as_ref(),
            ignore_case_attrs,
            &config_for_attrs,
        )?;
        if has_ws_errors {
            if args.exit_code {
                std::process::exit(3);
            }
            std::process::exit(2);
        }
        if args.exit_code || args.quiet {
            if has_diff {
                std::process::exit(1);
            }
            return Ok(());
        }
        return Ok(());
    }

    // `--quiet` alone suppresses stdout, but it still must honor `--exit-code` (below). `-s` /
    // `--no-patch` suppresses the unified patch without implying `--quiet`.
    let quiet_suppresses_stdout = args.quiet && !format_besides_unified_patch;

    // External-diff exit-code bookkeeping: when an external driver runs, its
    // `found_changes` (driven by the driver exit code under trustExitCode)
    // overrides the entry-derived `has_diff` for `--exit-code` / `--quiet`.
    let mut ext_diff_ran = false;
    let mut ext_found_changes = false;

    for w in &dirstat_config_warnings {
        eprintln!("warning: {w}");
    }

    if !quiet_suppresses_stdout {
        let context_lines = patch_context;
        let inter_hunk_context = args
            .inter_hunk_context
            .or_else(|| {
                grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
                    .ok()
                    .and_then(|cfg| cfg.get("diff.interhunkcontext"))
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(0);
        let full_hex = repo.odb.hash_algo().hex_len();
        let patch_abbrev = if args.full_index {
            full_hex
        } else if let Some(n) = args.abbrev {
            n.max(4).min(full_hex)
        } else {
            7
        };
        let oid_len = if args.full_index || args.no_abbrev {
            full_hex
        } else if let Some(n) = args.abbrev {
            n.max(4).min(full_hex)
        } else {
            7
        };
        let mut wrote_output = false;
        let mut need_blank_before_patch = false;

        if args.raw {
            write_raw(&mut out, &entries, oid_len, !args.cached && revs.len() <= 1)?;
            wrote_output = true;
            need_blank_before_patch = true;
        }
        if args.numstat {
            write_numstat(
                &mut out,
                &entries,
                &repo.odb,
                wt_for_content,
                args.break_rewrites,
                line_ignore,
                &ws_mode,
                &diff_algo_ctx,
                diff_algo_cli,
            )?;
            wrote_output = true;
            need_blank_before_patch = true;
        }
        if args.shortstat {
            write_shortstat(
                &mut out,
                &entries,
                &repo.odb,
                wt_for_content,
                args.break_rewrites,
                line_ignore,
                &ws_mode,
                &diff_algo_ctx,
                diff_algo_cli,
            )?;
            wrote_output = true;
            need_blank_before_patch = true;
        }
        if stat_block_active {
            if args.compact_summary {
                write_compact_summary(
                    &mut out,
                    &entries,
                    &repo.odb,
                    wt_for_content,
                    args.break_rewrites,
                    line_ignore,
                    &ws_mode,
                    &diff_algo_ctx,
                    diff_algo_cli,
                    &repo.git_dir,
                    quote_path_fully,
                )?;
            } else {
                write_stat(
                    &mut out,
                    &entries,
                    &repo.odb,
                    wt_for_content,
                    args.stat_count,
                    args.stat_width,
                    args.stat_name_width,
                    args.break_rewrites,
                    args.stat_graph_width,
                    "",
                    &repo.git_dir,
                    line_ignore,
                    &ws_mode,
                    quote_path_fully,
                    &diff_algo_ctx,
                    diff_algo_cli,
                )?;
            }
            if args.summary {
                write_diff_summary(&mut out, &entries, args.break_rewrites, quote_path_fully)?;
            }
            wrote_output = true;
            need_blank_before_patch = true;
        } else if args.summary {
            write_diff_summary(&mut out, &entries, args.break_rewrites, quote_path_fully)?;
            wrote_output = true;
            need_blank_before_patch = true;
        }
        if args.name_only {
            write_name_only(&mut out, &entries, quote_path_fully)?;
            wrote_output = true;
            need_blank_before_patch = true;
        }
        if args.name_status {
            write_name_status(&mut out, &entries, quote_path_fully)?;
            wrote_output = true;
            need_blank_before_patch = true;
        }
        if let Some(ref ds) = dirstat_opts {
            write_dirstat(
                &mut out,
                ds,
                &entries,
                &repo.odb,
                wt_for_content,
                args.break_rewrites,
            )?;
            wrote_output = true;
            need_blank_before_patch = true;
        }

        if args.patch_with_raw && show_unified_patch && !args.raw {
            write_raw(&mut out, &entries, oid_len, !args.cached && revs.len() <= 1)?;
            need_blank_before_patch = true;
            wrote_output = true;
        }
        if args.patch_with_stat && show_unified_patch && !stat_block_active {
            write_stat(
                &mut out,
                &entries,
                &repo.odb,
                wt_for_content,
                args.stat_count,
                args.stat_width,
                args.stat_name_width,
                args.break_rewrites,
                args.stat_graph_width,
                "",
                &repo.git_dir,
                line_ignore,
                &ws_mode,
                quote_path_fully,
                &diff_algo_ctx,
                diff_algo_cli,
            )?;
            need_blank_before_patch = true;
            wrote_output = true;
        }
        if show_unified_patch {
            if need_blank_before_patch && wrote_output {
                writeln!(out)?;
            }
            // `git diff --stat -p` prints stat then patch only when `-p`/`-u`/etc. appear on argv;
            // plain `--stat` must not append hunks (matches Git). But a plain `git diff` with no
            // competing format (no --stat/--raw/--name-only/... and no -s/--no-patch, the latter
            // already cleared show_unified_patch) must emit the unified patch body — that is the
            // default Git behavior. Gate the extra hunks on either an explicit -p alongside a
            // stat/format, or the absence of any other format entirely.
            let submodule_fmt_requested = args.submodule.as_deref().is_some_and(|s| !s.is_empty());
            // We are already inside `if show_unified_patch`, which applies last-flag-wins among
            // `-s`/`--no-patch` and the patch-enabling flags (`-p`, `--patch-with-raw`, ...).
            // Do NOT re-gate on `args.no_patch` here: for `--no-patch --patch-with-stat` the later
            // `--patch-with-*` re-enables the unified body even though `args.no_patch` stays set.
            let show_unified_after_stat = show_unified_patch
                && (diff_cli_requests_unified_patch_alongside_stat(&raw_args)
                    || submodule_fmt_requested
                    || !format_besides_unified_patch);
            if show_unified_after_stat {
                for patch in &conflict_combined_patches {
                    write!(out, "{patch}")?;
                }
                let diff_config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
                    .unwrap_or_default();
                let external_diff = resolve_env_config_external_diff(&diff_config);
                // `--color-moved`: when active with color on, render the patch into a
                // buffer, run move detection, then re-color moved lines before flushing.
                let move_ctx = resolve_color_moved(&args, &diff_config, use_color_patch, &ws_mode);
                // When move detection is active, render the patch into a buffer so we can
                // recolor moved lines; otherwise write straight to `out`.
                let mut move_buf: Vec<u8> = Vec::new();
                let summary = write_patch_with_prefix(
                    &mut move_buf,
                    &repo,
                    &entries,
                    &repo.odb,
                    &repo.git_dir,
                    &diff_config,
                    context_lines,
                    use_color_patch,
                    effective_word_diff_opt.as_ref(),
                    wt_for_content,
                    suppress_blank_empty,
                    patch_abbrev,
                    inter_hunk_context,
                    args.binary,
                    args.break_rewrites,
                    args.irreversible_delete,
                    !args.no_textconv,
                    &src_prefix,
                    &dst_prefix,
                    quote_path_fully,
                    args.submodule.as_deref(),
                    submodule_ignore_flags_from_diff_arg(ignore_sm.unwrap_or("none")),
                    ignore_sm,
                    &diff_config,
                    &path_to_sm_name,
                    &gm_sub_ignore,
                    line_ignore,
                    &ws_mode,
                    &diff_algo_ctx,
                    diff_algo_cli,
                    args.cached,
                    args.function_context,
                    args.no_ext_diff,
                    external_diff.as_ref().map(|(c, t)| (c.as_str(), *t)),
                    true,
                    relative_prefix_for_paths.as_deref(),
                    indent_heuristic,
                )?;
                if let Some((mode, ws_flags, colors)) = move_ctx {
                    let colored = String::from_utf8_lossy(&move_buf);
                    let recolored = apply_color_moved(&colored, mode, ws_flags, &colors);
                    out.write_all(recolored.as_bytes())?;
                } else {
                    out.write_all(&move_buf)?;
                }
                // Flush patch output before any external-diff "died" failure so the
                // program's stdout (already written) is visible (t4020).
                out.flush().ok();
                if let Some(path) = summary.died_path {
                    eprintln!("fatal: external diff died, stopping at {path}");
                    std::process::exit(128);
                }
                if summary.invoked {
                    ext_found_changes = summary.found_changes;
                    ext_diff_ran = true;
                }
            }
        }
    }

    // `--quiet` suppressed the patch block above, so the external driver never
    // ran. Git still invokes it (with stdout discarded) to learn `found_changes`
    // from the exit code when an external diff is configured (t4020 #56-#64).
    if quiet_suppresses_stdout && (args.exit_code || args.quiet) && !args.no_ext_diff {
        let diff_config =
            grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let env_cfg_ext = resolve_env_config_external_diff(&diff_config);
        // Run when env/config OR any attribute driver could apply.
        let any_attr_driver = entries.iter().any(|e| {
            grit_lib::merge_diff::diff_attr_external_driver(
                &repo.git_dir,
                &diff_config,
                &e.path().to_string_lossy(),
            )
            .is_some()
        });
        if env_cfg_ext.is_some() || any_attr_driver {
            let mut sink = io::sink();
            let summary = write_patch_with_prefix(
                &mut sink,
                &repo,
                &entries,
                &repo.odb,
                &repo.git_dir,
                &diff_config,
                3,
                false,
                None,
                wt_for_content,
                false,
                7,
                0,
                args.binary,
                args.break_rewrites,
                args.irreversible_delete,
                !args.no_textconv,
                &src_prefix,
                &dst_prefix,
                quote_path_fully,
                args.submodule.as_deref(),
                submodule_ignore_flags_from_diff_arg(ignore_sm.unwrap_or("none")),
                ignore_sm,
                &diff_config,
                &path_to_sm_name,
                &gm_sub_ignore,
                line_ignore,
                &ws_mode,
                &diff_algo_ctx,
                diff_algo_cli,
                args.cached,
                args.function_context,
                args.no_ext_diff,
                env_cfg_ext.as_ref().map(|(c, t)| (c.as_str(), *t)),
                false,
                relative_prefix_for_paths.as_deref(),
                indent_heuristic,
            )?;
            if let Some(path) = summary.died_path {
                eprintln!("fatal: external diff died, stopping at {path}");
                std::process::exit(128);
            }
            if summary.invoked {
                ext_found_changes = summary.found_changes;
                ext_diff_ran = true;
            }
        }
    }

    if args.exit_code || args.quiet {
        // With an external driver, Git's `found_changes` (driven by the driver's
        // exit code under trustExitCode) overrides the entry-derived `has_diff`.
        let changed = if ext_diff_ran {
            ext_found_changes
        } else {
            has_diff
        };
        if changed {
            std::process::exit(1);
        }
    }

    Ok(())
}

/// `git diff <rev>:<path> <file>` — blob at revision vs worktree file (t4063-diff-blobs).
fn run_diff_blob_vs_file(
    repo: &Repository,
    args: &Args,
    rev_path: &str,
    file_path: &str,
    display_old_blob_as: Option<&str>,
    src_prefix: &str,
    dst_prefix: &str,
    patch_context: usize,
    merged_attrs: Arc<grit_lib::attributes::ParsedGitAttributes>,
    diff_config: grit_lib::config::ConfigSet,
    ignore_case_attrs: bool,
    diff_algo_cli: Option<CliDiffAlgo>,
    cwd: &Path,
    quote_path_fully: bool,
    indent_heuristic: bool,
) -> Result<()> {
    let wt = repo
        .work_tree
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
    let abs = wt.join(file_path);

    let (old_path, old_mode, old_oid) = if split_treeish_colon(rev_path).is_some() {
        let tree_side = resolve_treeish_blob_at_path(repo, rev_path)
            .with_context(|| format!("unknown revision: '{rev_path}'"))?;
        (tree_side.path, tree_side.mode, tree_side.oid)
    } else {
        let oid = resolve_revision(repo, rev_path)
            .with_context(|| format!("unknown revision: '{rev_path}'"))?;
        let obj = repo
            .odb
            .read(&oid)
            .with_context(|| format!("reading object {oid}"))?;
        if obj.kind != ObjectKind::Blob {
            bail!("object '{oid}' does not name a blob");
        }
        let label = display_old_blob_as
            .map(str::to_owned)
            .unwrap_or_else(|| oid.to_hex());
        (label, "100644".to_owned(), oid)
    };

    let wt_mode = worktree_file_mode_octal(&abs);
    let disk_oid = Odb::hash_object_data(ObjectKind::Blob, &fs::read(&abs).unwrap_or_default());

    let entries = vec![DiffEntry {
        status: DiffStatus::Modified,
        // TODO(byte-paths): blob-diff display paths are lossy String (Phase 2/4).
        old_path: Some(RepoPathBuf::from_string(old_path)),
        new_path: Some(RepoPathBuf::from_string(file_path.to_owned())),
        old_mode,
        new_mode: wt_mode,
        old_oid,
        new_oid: disk_oid,
        score: None,
    }];
    let entry = &entries[0];

    let has_diff = entry.old_oid != entry.new_oid || entry.old_mode != entry.new_mode;
    if !has_diff {
        return Ok(());
    }

    if args.check {
        let mut out: Box<dyn Write> = if let Some(ref p) = args.output_path {
            let resolved = if p.is_absolute() {
                p.clone()
            } else {
                cwd.join(p)
            };
            Box::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&resolved)
                    .with_context(|| {
                        format!("failed to open output file {}", resolved.display())
                    })?,
            )
        } else {
            Box::new(io::stdout())
        };
        let config_for_attrs = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let ignore_case = config_for_attrs
            .get("core.ignorecase")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
        let has_ws_errors = check_whitespace_errors(
            &mut out,
            &entries,
            &repo.odb,
            Some(wt.as_ref()),
            merged_attrs.as_ref(),
            ignore_case,
            &config_for_attrs,
        )?;
        if has_ws_errors {
            if args.exit_code {
                std::process::exit(3);
            }
            std::process::exit(2);
        }
        if args.exit_code || args.quiet {
            std::process::exit(1);
        }
        return Ok(());
    }

    let diff_algo_ctx = DiffAlgoContext {
        attrs: merged_attrs,
        config: Arc::new(diff_config.clone()),
        ignore_case_attrs,
    };
    let inter_hunk_context = args
        .inter_hunk_context
        .or_else(|| {
            diff_config
                .get("diff.interhunkcontext")
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0);
    let patch_abbrev = if args.full_index {
        40usize
    } else if let Some(n) = args.abbrev {
        n.max(4).min(40)
    } else {
        7
    };
    let stdout_is_tty = io::stdout().is_terminal();
    let use_color_base = diff_use_color(
        args.color.as_deref(),
        &diff_config,
        args.output_path.as_deref(),
    );
    let effective_word_diff_opt = effective_word_diff(args, stdout_is_tty);
    let use_color_patch = use_color_base
        || effective_word_diff_opt
            .as_ref()
            .is_some_and(|w| w.force_color);
    let suppress_blank_empty = diff_config
        .get("diff.suppressBlankEmpty")
        .map(|v| v == "true")
        .unwrap_or(false);

    let external_diff = resolve_env_config_external_diff(&diff_config);
    let relative_prefix_for_paths =
        resolve_diff_relative_prefix(Some(wt.as_ref()), &repo.git_dir, args);

    let mut out: Box<dyn Write> = if let Some(ref p) = args.output_path {
        let resolved = if p.is_absolute() {
            p.clone()
        } else {
            cwd.join(p)
        };
        Box::new(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&resolved)
                .with_context(|| format!("failed to open output file {}", resolved.display()))?,
        )
    } else {
        Box::new(io::stdout())
    };

    let show_patch = !args.quiet && !args.no_patch;
    if show_patch {
        let empty_sm: HashMap<String, String> = HashMap::new();
        let empty_gm: HashMap<String, String> = HashMap::new();
        write_patch_with_prefix(
            &mut out,
            repo,
            &entries,
            &repo.odb,
            &repo.git_dir,
            &diff_config,
            patch_context,
            use_color_patch,
            effective_word_diff_opt.as_ref(),
            Some(wt.as_ref()),
            suppress_blank_empty,
            patch_abbrev,
            inter_hunk_context,
            args.binary,
            args.break_rewrites,
            args.irreversible_delete,
            !args.no_textconv,
            src_prefix,
            dst_prefix,
            quote_path_fully,
            args.submodule.as_deref(),
            submodule_ignore_flags_from_diff_arg(
                args.ignore_submodules.as_deref().unwrap_or("none"),
            ),
            args.ignore_submodules.as_deref(),
            &diff_config,
            &empty_sm,
            &empty_gm,
            None,
            &WhitespaceMode::default(),
            &diff_algo_ctx,
            diff_algo_cli,
            false,
            args.function_context,
            args.no_ext_diff,
            external_diff.as_ref().map(|(c, t)| (c.as_str(), *t)),
            true,
            relative_prefix_for_paths.as_deref(),
            indent_heuristic,
        )?;
    }

    if args.summary {
        write_diff_summary(&mut out, &entries, args.break_rewrites, quote_path_fully)?;
    }

    if (args.exit_code || args.quiet) && has_diff {
        std::process::exit(1);
    }
    Ok(())
}

/// Compare a path relative to the repository work tree to a second path (often outside the repo).
///
/// This matches `git diff <in-repo-path> <other-path>` when both arguments exist on disk and are
/// not revision specs.
///
/// `path_in_repo` / `path_other` are used for reading the work tree (after pathspec resolution).
/// `display_in_repo` / `display_other` are used in `---` / `+++` labels (repo-root-relative).
fn run_diff_two_paths(
    repo: &Repository,
    args: &Args,
    path_in_repo: &str,
    path_other: &str,
    display_in_repo: &str,
    display_other: &str,
    src_prefix: &str,
    dst_prefix: &str,
) -> Result<()> {
    let wt = repo
        .work_tree
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let read_path_or_symlink = |p: &Path, name: &str| -> Result<Vec<u8>> {
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                return std::fs::read_link(p)
                    .map(|target| target.to_string_lossy().into_owned().into_bytes())
                    .with_context(|| format!("could not read symlink '{name}'"));
            }
        }
        std::fs::read(p).with_context(|| format!("could not read '{name}'"))
    };

    let abs_in_repo = wt.join(path_in_repo);
    let other = Path::new(path_other);
    let abs_other = if other.is_absolute() {
        other.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| wt.to_path_buf())
            .join(other)
    };

    let data_a = read_path_or_symlink(&abs_in_repo, path_in_repo)?;
    let data_b = read_path_or_symlink(&abs_other, path_other)?;

    let text_a = String::from_utf8_lossy(&data_a);
    let text_b = String::from_utf8_lossy(&data_b);

    let ws_mode = WhitespaceMode {
        ignore_all_space: args.ignore_all_space,
        ignore_space_change: args.ignore_space_change,
        ignore_space_at_eol: args.ignore_space_at_eol,
        ignore_blank_lines: args.ignore_blank_lines,
        ignore_cr_at_eol: args.ignore_cr_at_eol,
    };

    let mut has_diff = text_a != text_b;
    if has_diff && ws_mode.any() && ws_mode.normalize(&text_a) == ws_mode.normalize(&text_b) {
        has_diff = false;
    }

    if !has_diff {
        return Ok(());
    }

    if args.quiet {
        std::process::exit(1);
    }

    let context_lines = if let Some(u) = args.unified {
        u
    } else {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).context("loading git config")?;
        grit_lib::config::resolve_diff_context_lines(&cfg)
            .map_err(|m| anyhow::anyhow!(m))?
            .unwrap_or(3)
    };

    let diff_cfg = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let indent_h =
        resolve_indent_heuristic(&diff_cfg, args.indent_heuristic, args.no_indent_heuristic);
    let quote_path_fully = diff_cfg.quote_path_fully();

    let patch = unified_diff_with_prefix(
        text_a.as_ref(),
        text_b.as_ref(),
        display_in_repo,
        display_other,
        context_lines,
        0,
        src_prefix,
        dst_prefix,
        indent_h,
        quote_path_fully,
    );

    let mut out = io::stdout().lock();
    let show_patch = !args.no_patch;
    if show_patch {
        let use_color = diff_use_color(args.color.as_deref(), &diff_cfg, None);
        if use_color {
            for line in patch.lines() {
                if line.starts_with("@@") {
                    writeln!(out, "{CYAN}{line}{RESET}")?;
                } else if line.starts_with('+') && !line.starts_with("+++") {
                    writeln!(out, "{GREEN}{line}{RESET}")?;
                } else if line.starts_with('-') && !line.starts_with("---") {
                    writeln!(out, "{RED}{line}{RESET}")?;
                } else if line.starts_with("diff ")
                    || line.starts_with("---")
                    || line.starts_with("+++")
                {
                    writeln!(out, "{BOLD}{line}{RESET}")?;
                } else {
                    writeln!(out, "{line}")?;
                }
            }
        } else {
            write!(out, "{patch}")?;
        }
    }

    if (args.exit_code || args.quiet) && has_diff {
        std::process::exit(1);
    }
    Ok(())
}

fn no_index_attr_rules(config: &grit_lib::config::ConfigSet) -> Vec<grit_lib::crlf::AttrRule> {
    let mut rules = Vec::new();
    if let Some(p) = config.get("core.attributesfile") {
        let path = Path::new(p.trim());
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        if let Ok(content) = fs::read_to_string(&path) {
            rules.extend(parse_gitattributes_content(&content));
        }
    }
    rules
}

fn no_index_apply_textconv(
    raw: &[u8],
    path_display: &str,
    rules: &[grit_lib::crlf::AttrRule],
    config: &grit_lib::config::ConfigSet,
    command_cwd: &Path,
) -> String {
    let fa = get_file_attrs(rules, path_display, false, config);
    let DiffAttr::Driver(ref driver) = fa.diff_attr else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    if grit_lib::merge_diff::diff_textconv_cmd_line(config, driver).is_none() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    run_textconv(command_cwd, config, driver, raw)
        .unwrap_or_else(|| String::from_utf8_lossy(raw).into_owned())
}

/// Split args on `--` to separate revisions from paths.
///
/// Run `diff --no-index <path_a> <path_b>` — compare two files outside a repo.
fn run_no_index(args: Args) -> Result<()> {
    // Collect paths (skip "--" separators and unrecognized flags). Everything
    // after a literal "--" is a pathspec, including a bare "-" which means
    // standard input. A bare "-" outside of "--" is also treated as a path.
    let mut paths: Vec<&String> = Vec::new();
    let mut after_dd = false;
    for a in args.args.iter() {
        if a.as_str() == "--" {
            after_dd = true;
            continue;
        }
        if after_dd || a.as_str() == "-" || !a.starts_with('-') {
            paths.push(a);
        }
    }
    if paths.len() != 2 {
        bail!("diff --no-index requires exactly two paths");
    }

    let path_a_str = paths[0].as_str().to_owned();
    let path_b_str = paths[1].as_str().to_owned();

    let path_a = Path::new(path_a_str.as_str());
    let path_b = Path::new(path_b_str.as_str());

    // If both paths are directories, diff all files recursively
    if path_a.is_dir() && path_b.is_dir() {
        return run_no_index_dirs(args, path_a, path_b);
    }

    let repo_opt = Repository::discover(None).ok();
    let merged_attrs = if let Some(ref r) = repo_opt {
        match load_gitattributes_for_diff(r) {
            Ok(m) => m,
            Err(Error::InvalidRef(msg)) if msg.starts_with("bad --attr-source") => {
                eprintln!("fatal: bad --attr-source or GIT_ATTR_SOURCE");
                std::process::exit(128);
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        grit_lib::attributes::ParsedGitAttributes::default()
    };
    let diff_config = repo_opt
        .as_ref()
        .map(|r| {
            grit_lib::config::ConfigSet::load(Some(&r.git_dir), true)
                .unwrap_or_else(|_| grit_lib::config::ConfigSet::new())
        })
        .unwrap_or_default();
    let quote_path_fully = diff_config.quote_path_fully();
    let ignore_case_attrs = diff_config
        .get("core.ignorecase")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
    let diff_algo_ctx = DiffAlgoContext {
        attrs: Arc::new(merged_attrs),
        config: Arc::new(diff_config.clone()),
        ignore_case_attrs,
    };
    let diff_algo_cli = parse_cli_diff_algorithm_from_argv();
    let indent_heuristic = resolve_indent_heuristic(
        diff_algo_ctx.config.as_ref(),
        args.indent_heuristic,
        args.no_indent_heuristic,
    );

    // Validate the funcname driver up front so a malformed pattern (e.g. a negated
    // last expression, `diff.<driver>.funcname = "!static"`) is reported as a fatal
    // error with exit 128, matching git, instead of being silently ignored.
    for rel in [path_a_str.as_str(), path_b_str.as_str()] {
        if let Err(msg) = matcher_for_path_parsed(
            diff_algo_ctx.config.as_ref(),
            &diff_algo_ctx.attrs.rules,
            &diff_algo_ctx.attrs.macros,
            rel,
            diff_algo_ctx.ignore_case_attrs,
        ) {
            eprintln!("fatal: {msg}");
            std::process::exit(128);
        }
    }

    // Read file or symlink target (for symlinks, read the target path as content).
    // A bare "-" means read from standard input.
    let read_path_or_symlink = |p: &Path, name: &str| -> Result<Vec<u8>> {
        if name == "-" {
            let mut buf = Vec::new();
            io::Read::read_to_end(&mut io::stdin().lock(), &mut buf)
                .context("could not read from standard input")?;
            return Ok(buf);
        }
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                return std::fs::read_link(p)
                    .map(|target| target.to_string_lossy().into_owned().into_bytes())
                    .with_context(|| format!("could not read symlink '{}'", name));
            }
        }
        std::fs::read(p).with_context(|| format!("could not read '{}'", name))
    };
    let data_a = read_path_or_symlink(path_a, &path_a_str)?;
    let data_b = read_path_or_symlink(path_b, &path_b_str)?;

    let config = grit_lib::config::ConfigSet::load(None, true).unwrap_or_default();
    let attr_rules = no_index_attr_rules(&config);
    let textconv_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let text_a = no_index_apply_textconv(
        &data_a,
        paths[0].as_str(),
        &attr_rules,
        &config,
        &textconv_cwd,
    );
    let text_b = no_index_apply_textconv(
        &data_b,
        paths[1].as_str(),
        &attr_rules,
        &config,
        &textconv_cwd,
    );

    if text_a == text_b {
        return Ok(());
    }

    let ws_mode = WhitespaceMode {
        ignore_all_space: args.ignore_all_space,
        ignore_space_change: args.ignore_space_change,
        ignore_space_at_eol: args.ignore_space_at_eol,
        ignore_blank_lines: args.ignore_blank_lines,
        ignore_cr_at_eol: args.ignore_cr_at_eol,
    };

    if ws_mode.any() && ws_mode.normalize(&text_a) == ws_mode.normalize(&text_b) {
        return Ok(());
    }

    if args.quiet {
        std::process::exit(1);
    }
    let context_lines = args.unified.unwrap_or(3);
    let inter_hunk_context = args
        .inter_hunk_context
        .or_else(|| {
            grit_lib::config::ConfigSet::load(None, true)
                .ok()
                .and_then(|cfg| cfg.get("diff.interhunkcontext"))
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0);
    let patch_abbrev = if args.full_index {
        40usize
    } else if let Some(n) = args.abbrev {
        n.max(4).min(40)
    } else {
        7
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let algo_for_paths = |rel_a: &str, rel_b: &str| -> (similar::Algorithm, bool) {
        let a = diff_algorithm_for_path(rel_a, diff_algo_cli, &diff_algo_ctx);
        let b = diff_algorithm_for_path(rel_b, diff_algo_cli, &diff_algo_ctx);
        if a == b {
            a
        } else {
            diff_algo_from_config_default(&diff_algo_ctx.config)
        }
    };

    if args.name_only {
        writeln!(out, "{path_b_str}")?;
        std::process::exit(1);
    }

    if args.name_status {
        writeln!(out, "M\t{path_b_str}")?;
        std::process::exit(1);
    }

    if args.numstat {
        let (algo, hist) = algo_for_paths(path_a_str.as_str(), path_b_str.as_str());
        let (adds, dels) = count_changes_with_algorithm(&text_a, &text_b, algo, hist);
        writeln!(out, "{}\t{}\t{}", adds, dels, path_b_str)?;
        std::process::exit(1);
    }

    if args.stat.is_some() || args.shortstat {
        let (algo, hist) = algo_for_paths(path_a_str.as_str(), path_b_str.as_str());
        let (adds, dels) = count_changes_with_algorithm(&text_a, &text_b, algo, hist);
        if args.stat.is_some() {
            let display = if path_a_str != path_b_str {
                format!("{path_a_str} => {path_b_str}")
            } else {
                path_a_str.clone()
            };
            let stat_line = format_stat_line(&display, adds, dels, display.len());
            writeln!(out, "{stat_line}")?;
        }
        let mut summary = " 1 file changed".to_string();
        if adds > 0 {
            summary.push_str(&format!(
                ", {} insertion{}(+)",
                adds,
                if adds == 1 { "" } else { "s" }
            ));
        }
        if dels > 0 {
            summary.push_str(&format!(
                ", {} deletion{}(-)",
                dels,
                if dels == 1 { "" } else { "s" }
            ));
        }
        writeln!(out, "{summary}")?;
        std::process::exit(1);
    }

    let use_anchored = if !args.anchored.is_empty() {
        let raw_args: Vec<String> = std::env::args().collect();
        let last_anchored_pos = raw_args.iter().rposition(|a| a.starts_with("--anchored"));
        let last_other_algo_pos = raw_args.iter().rposition(|a| {
            a == "--patience"
                || a == "--histogram"
                || a == "--minimal"
                || a.starts_with("--diff-algorithm")
        });
        match (last_anchored_pos, last_other_algo_pos) {
            (Some(a), Some(o)) => a > o,
            (Some(_), None) => true,
            _ => false,
        }
    } else {
        false
    };

    let (line_algo_anchored, line_hist_anchored) = effective_line_diff_algorithm(&args);
    let (algo_sim, algo_hist) = parse_cli_diff_algorithm_from_argv()
        .map(|c| (c.similar, c.histogram_git))
        .unwrap_or_else(|| algo_for_paths(path_a_str.as_str(), path_b_str.as_str()));

    let stdout_is_tty = io::stdout().is_terminal();
    let effective_word_diff_opt = effective_word_diff(&args, stdout_is_tty);
    let use_color = match args.color.as_deref() {
        Some("always") => true,
        Some("never") => false,
        Some("auto") | None => stdout_is_tty && grit_lib::terminal::ansi_supported(),
        Some(_) => false,
    };
    let use_color_patch = use_color
        || effective_word_diff_opt
            .as_ref()
            .is_some_and(|w| w.force_color);
    let cfg_for_word_regex = if repo_opt.is_some() {
        &diff_config
    } else {
        &config
    };

    let diff_body = if let Some(ref wd) = effective_word_diff_opt {
        let word_re_opt = if let Some(ref o) = wd.regex_override {
            WordRe::build(o, false)
        } else {
            word_regex_pattern_for_path_parsed(
                cfg_for_word_regex,
                &diff_algo_ctx.attrs.rules,
                &diff_algo_ctx.attrs.macros,
                path_a_str.as_str(),
                ignore_case_attrs,
            )
            .and_then(|(pat, ic)| WordRe::build(&pat, ic))
        };
        word_diff_generate_patch(
            cfg_for_word_regex,
            &text_a,
            &text_b,
            path_a_str.as_str(),
            path_b_str.as_str(),
            context_lines,
            inter_hunk_context,
            "a/",
            "b/",
            wd,
            use_color_patch,
            word_re_opt.as_ref(),
            &diff_algo_ctx,
            diff_algo_cli,
            indent_heuristic,
            quote_path_fully,
        )
    } else if use_anchored {
        anchored_unified_diff(
            &text_a,
            &text_b,
            &path_a_str,
            &path_b_str,
            context_lines,
            &args.anchored,
            line_algo_anchored,
            line_hist_anchored,
            indent_heuristic,
            quote_path_fully,
        )
    } else if args.function_context
        && !ws_mode.ignore_all_space
        && !ws_mode.ignore_space_change
        && !ws_mode.ignore_space_at_eol
        && !ws_mode.ignore_cr_at_eol
    {
        // `--function-context` (optionally with `--ignore-blank-lines`): the
        // worktree path expands hunks to whole functions and then drops
        // ignorable changes. Reuse the same machinery here (the no-index body
        // generator does not support `-W`). No userdiff driver applies to a bare
        // `--no-index` path, so the built-in funcname detection is used.
        let func_matcher = matcher_for_path_parsed(
            cfg_for_word_regex,
            &diff_algo_ctx.attrs.rules,
            &diff_algo_ctx.attrs.macros,
            path_a_str.as_str(),
            ignore_case_attrs,
        )
        .unwrap_or(None);
        let body = unified_diff_with_prefix_and_funcname_and_algorithm(
            &text_a,
            &text_b,
            paths[0].as_str(),
            paths[1].as_str(),
            context_lines,
            inter_hunk_context,
            "a/",
            "b/",
            func_matcher.as_ref(),
            algo_sim,
            true,
            algo_hist,
            indent_heuristic,
            quote_path_fully,
        );
        if ws_mode.ignore_blank_lines {
            suppress_ignored_hunks_in_patch(&body, &[], true)
        } else {
            body
        }
    } else {
        // Diff the textconv (or UTF-8 lossy) view so hunks match what we compare for exit status
        // and stats; raw blob bytes would ignore `diff.<driver>.textconv` in the patch body (t4042).
        no_index_unified_patch_body(
            text_a.as_bytes(),
            text_b.as_bytes(),
            paths[0].as_str(),
            paths[1].as_str(),
            context_lines,
            inter_hunk_context,
            &ws_mode,
            algo_sim,
            algo_hist,
            indent_heuristic,
            quote_path_fully,
        )
    };

    let old_abbrev = abbrev_oid_hex(&data_a, patch_abbrev);
    let new_abbrev = abbrev_oid_hex(&data_b, patch_abbrev);
    let mode_str = if path_a
        .symlink_metadata()
        .ok()
        .is_some_and(|m| m.file_type().is_symlink())
        || path_b
            .symlink_metadata()
            .ok()
            .is_some_and(|m| m.file_type().is_symlink())
    {
        "120000"
    } else {
        "100644"
    };

    let git_a = format_diff_path_with_prefix("a/", paths[0].as_str(), quote_path_fully);
    let git_b = format_diff_path_with_prefix("b/", paths[1].as_str(), quote_path_fully);
    if use_color_patch {
        let mut colored = String::new();
        use std::fmt::Write as _;
        let _ = writeln!(colored, "{BOLD}diff --git {git_a} {git_b}{RESET}");
        let _ = writeln!(
            colored,
            "{BOLD}index {old_abbrev}..{new_abbrev} {mode_str}{RESET}"
        );
        if effective_word_diff_opt.is_some() {
            let mut first = true;
            for line in diff_body.lines() {
                if first && (line.starts_with("--- ") || line.starts_with("+++ ")) {
                    let _ = writeln!(colored, "{BOLD}{line}{RESET}");
                    first = false;
                    continue;
                }
                if line.starts_with("--- ") || line.starts_with("+++ ") {
                    let _ = writeln!(colored, "{BOLD}{line}{RESET}");
                } else {
                    let _ = writeln!(colored, "{line}");
                }
            }
        } else {
            for line in diff_body.lines() {
                if line.starts_with("@@") {
                    // Split a trailing funcname out of the cyan range (Git format).
                    if let Some(close) = line.get(2..).and_then(|s| s.find("@@")).map(|i| i + 2) {
                        let range = &line[..close + 2];
                        let rest = &line[close + 2..];
                        if let Some(func) = rest.strip_prefix(' ') {
                            if func.is_empty() {
                                let _ = writeln!(colored, "{CYAN}{range}{RESET} ");
                            } else {
                                let _ =
                                    writeln!(colored, "{CYAN}{range}{RESET} {RESET}{func}{RESET}");
                            }
                        } else {
                            let _ = writeln!(colored, "{CYAN}{range}{rest}{RESET}");
                        }
                    } else {
                        let _ = writeln!(colored, "{CYAN}{line}{RESET}");
                    }
                } else if let Some(body) =
                    line.strip_prefix('+').filter(|_| !line.starts_with("+++"))
                {
                    // Git emits new lines with the sign split off (`emit_line_0`
                    // with set_sign): `<col>+<RESET><col>body<RESET>`.
                    let _ = writeln!(colored, "{GREEN}+{RESET}{GREEN}{body}{RESET}");
                } else if line.starts_with('-') && !line.starts_with("---") {
                    let _ = writeln!(colored, "{RED}{line}{RESET}");
                } else if line.starts_with("diff ")
                    || line.starts_with("---")
                    || line.starts_with("+++")
                {
                    let _ = writeln!(colored, "{BOLD}{line}{RESET}");
                } else {
                    // Context lines are emitted with a trailing reset by Git.
                    let _ = writeln!(colored, "{line}{RESET}");
                }
            }
        }
        // `--color-moved`: recolor moved lines (the no-index config is `config`).
        let move_ctx = resolve_color_moved(&args, &config, use_color_patch, &ws_mode);
        if let Some((mode, ws_flags, colors)) = move_ctx {
            let recolored = apply_color_moved(&colored, mode, ws_flags, &colors);
            out.write_all(recolored.as_bytes())?;
        } else {
            out.write_all(colored.as_bytes())?;
        }
    } else {
        writeln!(out, "diff --git {git_a} {git_b}")?;
        writeln!(out, "index {old_abbrev}..{new_abbrev} {mode_str}")?;
        write!(out, "{diff_body}")?;
    }

    if args.exit_code || args.quiet {
        std::process::exit(1);
    }
    std::process::exit(1);
}

/// Diff two directories recursively with --no-index.
fn run_no_index_dirs(args: Args, dir_a: &Path, dir_b: &Path) -> Result<()> {
    use std::collections::BTreeSet;

    /// Leaf content for diff: symlink target as bytes, or regular file bytes. Directories are not
    /// leaves (caller only collects real dirs and symlink files).
    fn read_no_index_leaf(path: &Path) -> Option<Vec<u8>> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() {
            return std::fs::read_link(path)
                .ok()
                .map(|t| t.to_string_lossy().into_owned().into_bytes());
        }
        if meta.is_file() {
            return std::fs::read(path).ok();
        }
        None
    }

    fn collect_files(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let ft = entry.file_type()?;
            // Do not follow symlinks: `c -> b` must be one leaf (t2080), not a walk into `b/`.
            if ft.is_symlink() {
                out.insert(rel);
            } else if ft.is_dir() {
                collect_files(&entry.path(), &rel, out)?;
            } else {
                out.insert(rel);
            }
        }
        Ok(())
    }

    let mut files_a = BTreeSet::new();
    let mut files_b = BTreeSet::new();
    collect_files(dir_a, "", &mut files_a)?;
    collect_files(dir_b, "", &mut files_b)?;

    let repo_opt = Repository::discover(None).ok();
    let merged_attrs = if let Some(ref r) = repo_opt {
        match load_gitattributes_for_diff(r) {
            Ok(m) => m,
            Err(Error::InvalidRef(msg)) if msg.starts_with("bad --attr-source") => {
                eprintln!("fatal: bad --attr-source or GIT_ATTR_SOURCE");
                std::process::exit(128);
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        grit_lib::attributes::ParsedGitAttributes::default()
    };
    let diff_config = repo_opt
        .as_ref()
        .map(|r| {
            grit_lib::config::ConfigSet::load(Some(&r.git_dir), true)
                .unwrap_or_else(|_| grit_lib::config::ConfigSet::new())
        })
        .unwrap_or_default();
    let quote_path_fully = diff_config.quote_path_fully();
    let ignore_case_attrs = diff_config
        .get("core.ignorecase")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
    let diff_algo_ctx = DiffAlgoContext {
        attrs: Arc::new(merged_attrs),
        config: Arc::new(diff_config.clone()),
        ignore_case_attrs,
    };
    let diff_algo_cli = parse_cli_diff_algorithm_from_argv();
    let indent_heuristic = resolve_indent_heuristic(
        diff_algo_ctx.config.as_ref(),
        args.indent_heuristic,
        args.no_indent_heuristic,
    );
    let patch_abbrev = if args.full_index {
        40usize
    } else if let Some(n) = args.abbrev {
        n.max(4).min(40)
    } else {
        7
    };

    let all_files: BTreeSet<_> = files_a.iter().chain(files_b.iter()).cloned().collect();
    let mut has_diff = false;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let context_lines = args.unified.unwrap_or(3);
    let inter_hunk_context = args.inter_hunk_context.unwrap_or(0);
    let ws_mode = WhitespaceMode {
        ignore_all_space: args.ignore_all_space,
        ignore_space_change: args.ignore_space_change,
        ignore_space_at_eol: args.ignore_space_at_eol,
        ignore_blank_lines: args.ignore_blank_lines,
        ignore_cr_at_eol: args.ignore_cr_at_eol,
    };

    for rel in &all_files {
        let fa = dir_a.join(rel);
        let fb = dir_b.join(rel);
        let data_a = read_no_index_leaf(&fa);
        let data_b = read_no_index_leaf(&fb);

        match (&data_a, &data_b) {
            (Some(a), Some(b)) if a == b => continue,
            _ => {}
        }

        let text_a = data_a
            .as_ref()
            .map(|d| String::from_utf8_lossy(d).to_string())
            .unwrap_or_default();
        let text_b = data_b
            .as_ref()
            .map(|d| String::from_utf8_lossy(d).to_string())
            .unwrap_or_default();
        if ws_mode.any() && ws_mode.normalize(&text_a) == ws_mode.normalize(&text_b) {
            continue;
        }

        has_diff = true;
        let label_a = format!("{}/{}", dir_a.display(), rel);
        let label_b = format!("{}/{}", dir_b.display(), rel);

        if args.name_only {
            writeln!(out, "{}", label_b)?;
            continue;
        }
        if args.name_status {
            let status = match (&data_a, &data_b) {
                (None, Some(_)) => "A",
                (Some(_), None) => "D",
                _ => "M",
            };
            writeln!(out, "{}\t{}", status, label_b)?;
            continue;
        }
        if args.raw {
            // `git diff --no-index --raw`: emit the raw plumbing line with all-zero OIDs (there
            // are no objects in --no-index mode). Path shown is the second argument's side.
            let mode_a = fa
                .symlink_metadata()
                .ok()
                .and_then(|m| m.file_type().is_symlink().then_some("120000"))
                .unwrap_or("100644");
            let mode_b = fb
                .symlink_metadata()
                .ok()
                .and_then(|m| m.file_type().is_symlink().then_some("120000"))
                .unwrap_or("100644");
            let (status, om, nm, path) = match (&data_a, &data_b) {
                (None, Some(_)) => ("A", "000000", mode_b, label_b.as_str()),
                (Some(_), None) => ("D", mode_a, "000000", label_a.as_str()),
                _ => ("M", mode_a, mode_b, label_b.as_str()),
            };
            let raw_abbrev = if args.abbrev == Some(0) || args.no_abbrev {
                40usize
            } else if let Some(n) = args.abbrev {
                n.clamp(4, 40)
            } else {
                7
            };
            let ell = if raw_abbrev < 40
                && std::env::var("GIT_PRINT_SHA1_ELLIPSIS").ok().as_deref() == Some("yes")
            {
                "..."
            } else {
                ""
            };
            let zeros = format!("{}{ell}", "0".repeat(raw_abbrev));
            writeln!(out, ":{om} {nm} {zeros} {zeros} {status}\t{path}")?;
            has_diff = true;
            continue;
        }

        let display_old: &str = if data_a.is_none() {
            "/dev/null"
        } else {
            rel.as_str()
        };
        let display_new: &str = if data_b.is_none() {
            "/dev/null"
        } else {
            rel.as_str()
        };
        let git_old = format_diff_path_with_prefix("a/", rel.as_str(), quote_path_fully);
        let git_new = format_diff_path_with_prefix("b/", rel.as_str(), quote_path_fully);
        writeln!(out, "diff --git {git_old} {git_new}")?;
        let mode_a = fa
            .symlink_metadata()
            .ok()
            .and_then(|m| m.file_type().is_symlink().then_some("120000"))
            .unwrap_or("100644");
        let mode_b = fb
            .symlink_metadata()
            .ok()
            .and_then(|m| m.file_type().is_symlink().then_some("120000"))
            .unwrap_or("100644");
        if data_a.is_none() {
            writeln!(out, "new file mode {mode_b}")?;
        } else if data_b.is_none() {
            writeln!(out, "deleted file mode {mode_a}")?;
        }
        let da = data_a.as_deref().unwrap_or(&[]);
        let db = data_b.as_deref().unwrap_or(&[]);
        let a = abbrev_oid_hex(da, patch_abbrev);
        let b = abbrev_oid_hex(db, patch_abbrev);
        if mode_a == mode_b {
            writeln!(out, "index {a}..{b} {mode_a}")?;
        } else {
            writeln!(out, "index {a}..{b}")?;
            writeln!(out, "old mode {mode_a}")?;
            writeln!(out, "new mode {mode_b}")?;
        }
        let (algo, use_git_histogram) =
            diff_algorithm_for_path(rel.as_str(), diff_algo_cli, &diff_algo_ctx);
        let func_matcher = matcher_for_path_parsed(
            diff_algo_ctx.config.as_ref(),
            &diff_algo_ctx.attrs.rules,
            &diff_algo_ctx.attrs.macros,
            rel.as_str(),
            diff_algo_ctx.ignore_case_attrs,
        )
        .unwrap_or(None);
        let patch = unified_diff_with_prefix_and_funcname_and_algorithm(
            &text_a,
            &text_b,
            display_old,
            display_new,
            context_lines,
            inter_hunk_context,
            "a/",
            "b/",
            func_matcher.as_ref(),
            algo,
            args.function_context,
            use_git_histogram,
            indent_heuristic,
            quote_path_fully,
        );
        write!(out, "{patch}")?;
    }

    if has_diff {
        std::process::exit(1);
    }
    Ok(())
}

/// If `--` is present, everything before is revisions, everything after is paths.
/// Otherwise, we try each arg: if it exists as a file, treat it (and all
/// subsequent args) as paths rather than revisions.
/// Parse `-M` / `--find-renames` similarity: optional trailing `%`, capped at 100 (Git truncates).
fn parse_diff_rename_threshold(raw: &str) -> u32 {
    let t = raw.trim();
    let num = t.strip_suffix('%').unwrap_or(t);
    num.parse::<u32>().unwrap_or(50).min(100)
}

/// Apply `--diff-filter` (include uppercase status letters, exclude lowercase).
fn apply_diff_filter(entries: Vec<DiffEntry>, filter: &str) -> Vec<DiffEntry> {
    let mut include: Option<std::collections::HashSet<char>> = None;
    let mut exclude: std::collections::HashSet<char> = std::collections::HashSet::new();
    for c in filter.chars() {
        if c == '*' {
            continue;
        }
        if c.is_ascii_uppercase() {
            include.get_or_insert_with(Default::default).insert(c);
        } else if c.is_ascii_lowercase() {
            exclude.insert(c.to_ascii_uppercase());
        }
    }
    entries
        .into_iter()
        .filter(|e| {
            let ch = e.status.letter();
            if exclude.contains(&ch) {
                return false;
            }
            if let Some(ref inc) = include {
                return inc.contains(&ch);
            }
            true
        })
        .collect()
}

/// Whether the user asked for a unified patch in addition to another format (e.g. `--stat`).
///
/// Unlike [`diff_emit_unified_patch_from_argv`], this starts from **no** patch: plain
/// `git diff --stat` must not append hunks, while `git diff --stat -p` must (matches Git).
fn diff_cli_requests_unified_patch_alongside_stat(argv: &[String]) -> bool {
    let Some(diff_pos) = argv.iter().position(|a| a == "diff") else {
        return false;
    };
    let mut emit = false;
    for arg in argv.iter().skip(diff_pos + 1) {
        if arg == "--" {
            break;
        }
        if arg == "-s" || arg == "--no-patch" {
            emit = false;
            continue;
        }
        if arg == "-p" || arg == "--patch" || arg == "-u" {
            emit = true;
            continue;
        }
        // `--patch-with-raw` / `--patch-with-stat` request the unified patch alongside the
        // raw/stat output (git's `--patch-with-*`).
        if arg == "--patch-with-raw" || arg == "--patch-with-stat" {
            emit = true;
            continue;
        }
        if arg == "--submodule" || arg.starts_with("--submodule=") {
            emit = true;
            continue;
        }
        if arg.starts_with("-U") || arg.starts_with("--unified") {
            emit = true;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            const COMBINABLE: &[u8] = b"spuwqRb";
            let bytes = arg.as_bytes();
            let tail = &bytes[1..];
            if !tail.is_empty() && tail.iter().all(|b| COMBINABLE.contains(b)) {
                for &b in tail {
                    match b {
                        b's' => emit = false,
                        b'p' | b'u' => emit = true,
                        b'w' | b'b' | b'q' | b'R' => {}
                        _ => {}
                    }
                }
            }
        }
    }
    emit
}

/// Last-flag-wins patch emission after a plumbing subcommand (`diff-files`, `diff-index`, …).
///
/// Git uses last-flag-wins among `-s` / `--no-patch` (suppress) and `-p` / `--patch` / `-u` /
/// `-U` / `--unified` (show patch). Combined short flags (e.g. `-sp`) are expanded per character.
pub(crate) fn diff_emit_unified_patch_from_plumbing_argv(subcmd: &str, argv: &[String]) -> bool {
    let default_emit = subcmd != "diff-files";
    let Some(pos) = argv.iter().position(|a| a == subcmd) else {
        return default_emit;
    };
    let mut emit = default_emit;
    for arg in argv.iter().skip(pos + 1) {
        if arg == "--" {
            break;
        }
        if arg == "-s" || arg == "--no-patch" {
            emit = false;
            continue;
        }
        if arg == "-p" || arg == "--patch" || arg == "-u" {
            emit = true;
            continue;
        }
        if arg == "--patch-with-raw" || arg == "--patch-with-stat" {
            emit = true;
            continue;
        }
        if arg == "--submodule" || arg.starts_with("--submodule=") {
            emit = true;
            continue;
        }
        if arg.starts_with("-U") || arg.starts_with("--unified") {
            emit = true;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            const COMBINABLE: &[u8] = b"spuwqRb";
            let bytes = arg.as_bytes();
            let tail = &bytes[1..];
            if !tail.is_empty() && tail.iter().all(|b| COMBINABLE.contains(b)) {
                for &b in tail {
                    match b {
                        b's' => emit = false,
                        b'p' | b'u' => emit = true,
                        b'w' | b'b' | b'q' | b'R' => {}
                        _ => {}
                    }
                }
            }
        }
    }
    emit
}

/// Whether to emit unified patch hunks for this `git diff` invocation.
pub(crate) fn diff_emit_unified_patch_from_argv(argv: &[String]) -> bool {
    diff_emit_unified_patch_from_plumbing_argv("diff", argv)
}

/// Whether `git diff` should emit the unified patch (last wins among `-s` / format flags / `-p`).
fn diff_show_unified_patch_last_wins(argv: &[String]) -> bool {
    let Some(pos) = argv.iter().position(|a| a == "diff") else {
        return true;
    };
    let mut show = true;
    for arg in argv.iter().skip(pos + 1) {
        if arg == "--" {
            break;
        }
        match arg.as_str() {
            "-s" | "--no-patch" => show = false,
            "-p" | "--patch" | "-u" => show = true,
            "--patch-with-raw" | "--patch-with-stat" => show = true,
            "--submodule" | "--histogram" | "--patience" | "--minimal" => show = true,
            s if s.starts_with("--submodule=") => show = true,
            s if s.starts_with("-U") || s.starts_with("--unified") => show = true,
            "--stat" | "--shortstat" | "--numstat" | "--raw" | "--name-only" | "--name-status"
            | "--summary" | "--compact-summary" | "--dirstat" | "--dirstat-by-file"
            | "--cumulative" => show = false,
            // `-X` / `-X<param>` is the short form of `--dirstat` and likewise suppresses the
            // unified patch. This scan runs on raw argv (pre-`-X`-translation), so handle it here.
            "-X" => show = false,
            s if s.starts_with("-X") => show = false,
            s if s.starts_with("--stat=") => show = false,
            s if s.starts_with("--dirstat=") => show = false,
            s if s.starts_with("--dirstat-by-file=") => show = false,
            _ if arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.len() > 2
                && arg.as_bytes().get(1).is_some_and(|b| *b != b'-') =>
            {
                const COMBINABLE: &[u8] = b"spuwqRb";
                let bytes = arg.as_bytes();
                let tail = &bytes[1..];
                if !tail.is_empty() && tail.iter().all(|b| COMBINABLE.contains(b)) {
                    for &b in tail {
                        match b {
                            b's' => show = false,
                            b'p' | b'u' => show = true,
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    show
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffFilesEmitKind {
    Raw,
    Stat,
    NumStat,
    Shortstat,
    Dirstat,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DiffFilesStatVariant {
    #[default]
    None,
    Stat,
    CompactSummary,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DiffFilesFormatParse {
    pub emit_queue: Vec<DiffFilesEmitKind>,
    pub stat_variant: DiffFilesStatVariant,
    pub patch_with_raw: bool,
    pub patch_with_stat: bool,
    pub dirstat_cli: DirstatCliState,
    pub explicit_raw: bool,
    pub suppress_diff: bool,
    pub format: DiffFilesDefaultFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffFilesDefaultFormat {
    Raw,
    Patch,
    Stat,
    NumStat,
    NameOnly,
    NameStatus,
}

impl Default for DiffFilesDefaultFormat {
    fn default() -> Self {
        Self::Raw
    }
}

/// Parse `diff-files`-specific output flags in argv order (matches Git: `-s` suppresses following format output until `-p`).
pub(crate) fn parse_diff_files_format_argv(argv: &[String]) -> DiffFilesFormatParse {
    let mut out = DiffFilesFormatParse::default();
    let Some(pos) = argv.iter().position(|a| a == "diff-files") else {
        return out;
    };
    let mut idx = pos + 1;
    let mut end_of_options = false;
    while idx < argv.len() {
        let arg = &argv[idx];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            idx += 1;
            continue;
        }
        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "-s" | "--no-patch" => {
                    out.suppress_diff = true;
                    out.emit_queue.clear();
                    out.patch_with_raw = false;
                    out.patch_with_stat = false;
                    out.stat_variant = DiffFilesStatVariant::None;
                    out.dirstat_cli = DirstatCliState::default();
                    out.explicit_raw = false;
                    out.format = DiffFilesDefaultFormat::Raw;
                }
                "-p" | "--patch" | "-u" => {
                    out.suppress_diff = false;
                    out.format = DiffFilesDefaultFormat::Patch;
                }
                "--raw" => {
                    out.explicit_raw = true;
                    out.format = DiffFilesDefaultFormat::Raw;
                    out.emit_queue.push(DiffFilesEmitKind::Raw);
                    out.suppress_diff = false;
                }
                "--stat" => {
                    out.format = DiffFilesDefaultFormat::Stat;
                    out.stat_variant = DiffFilesStatVariant::Stat;
                    out.emit_queue.push(DiffFilesEmitKind::Stat);
                    out.suppress_diff = false;
                }
                "--compact-summary" => {
                    out.format = DiffFilesDefaultFormat::Stat;
                    out.stat_variant = DiffFilesStatVariant::CompactSummary;
                    out.emit_queue.push(DiffFilesEmitKind::Stat);
                    out.suppress_diff = false;
                }
                "--numstat" => {
                    out.format = DiffFilesDefaultFormat::NumStat;
                    out.emit_queue.push(DiffFilesEmitKind::NumStat);
                    out.suppress_diff = false;
                }
                "--shortstat" => {
                    out.emit_queue.push(DiffFilesEmitKind::Shortstat);
                    out.suppress_diff = false;
                }
                "--name-only" => {
                    out.format = DiffFilesDefaultFormat::NameOnly;
                    out.suppress_diff = false;
                }
                "--name-status" => {
                    out.format = DiffFilesDefaultFormat::NameStatus;
                    out.suppress_diff = false;
                }
                "--summary" => {
                    out.emit_queue.push(DiffFilesEmitKind::Summary);
                }
                "--patch-with-raw" => {
                    out.patch_with_raw = true;
                    out.format = DiffFilesDefaultFormat::Patch;
                    out.suppress_diff = false;
                }
                "--patch-with-stat" => {
                    out.patch_with_stat = true;
                    out.format = DiffFilesDefaultFormat::Patch;
                    out.suppress_diff = false;
                }
                "--dirstat" => {
                    out.format = DiffFilesDefaultFormat::Patch;
                    out.dirstat_cli.dirstat.push(String::new());
                    out.emit_queue.push(DiffFilesEmitKind::Dirstat);
                    out.suppress_diff = false;
                }
                s if s.starts_with("--dirstat=") => {
                    out.format = DiffFilesDefaultFormat::Patch;
                    out.dirstat_cli
                        .dirstat
                        .push(s.strip_prefix("--dirstat=").unwrap_or("").to_owned());
                    out.emit_queue.push(DiffFilesEmitKind::Dirstat);
                    out.suppress_diff = false;
                }
                "--dirstat-by-file" => {
                    out.format = DiffFilesDefaultFormat::Patch;
                    if out.dirstat_cli.dirstat_by_file.is_none() {
                        out.dirstat_cli.dirstat_by_file = Some(String::new());
                    }
                    out.emit_queue.push(DiffFilesEmitKind::Dirstat);
                    out.suppress_diff = false;
                }
                s if s.starts_with("--dirstat-by-file=") => {
                    out.format = DiffFilesDefaultFormat::Patch;
                    out.dirstat_cli.dirstat_by_file = Some(
                        s.strip_prefix("--dirstat-by-file=")
                            .unwrap_or("")
                            .to_owned(),
                    );
                    out.emit_queue.push(DiffFilesEmitKind::Dirstat);
                    out.suppress_diff = false;
                }
                "--cumulative" => {
                    out.format = DiffFilesDefaultFormat::Patch;
                    out.dirstat_cli.dirstat_cumulative_flag = true;
                    out.emit_queue.push(DiffFilesEmitKind::Dirstat);
                    out.suppress_diff = false;
                }
                _ if arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.len() > 2
                    && arg.as_bytes().get(1).is_some_and(|b| *b != b'-') =>
                {
                    const COMBINABLE: &[u8] = b"spuwqRb";
                    let bytes = arg.as_bytes();
                    let tail = &bytes[1..];
                    if !tail.is_empty() && tail.iter().all(|b| COMBINABLE.contains(b)) {
                        for &b in tail {
                            match b {
                                b's' => {
                                    out.suppress_diff = true;
                                    out.emit_queue.clear();
                                    out.patch_with_raw = false;
                                    out.patch_with_stat = false;
                                    out.stat_variant = DiffFilesStatVariant::None;
                                    out.dirstat_cli = DirstatCliState::default();
                                    out.explicit_raw = false;
                                    out.format = DiffFilesDefaultFormat::Raw;
                                }
                                b'p' | b'u' => {
                                    out.suppress_diff = false;
                                    out.format = DiffFilesDefaultFormat::Patch;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
            idx += 1;
            continue;
        }
        idx += 1;
    }
    out
}

/// CLI state for `--dirstat` / `--dirstat-by-file` / `--cumulative` (shared by `diff` and `diff-files`).
#[derive(Debug, Default, Clone)]
pub(crate) struct DirstatCliState {
    pub dirstat: Vec<String>,
    pub dirstat_by_file: Option<String>,
    pub dirstat_cumulative_flag: bool,
}

impl From<&Args> for DirstatCliState {
    fn from(args: &Args) -> Self {
        Self {
            dirstat: args.dirstat.clone(),
            dirstat_by_file: args.dirstat_by_file.clone(),
            dirstat_cumulative_flag: args.dirstat_cumulative_flag,
        }
    }
}

fn resolve_dirstat_options(
    args: &Args,
    git_dir: &std::path::Path,
    cli_active: bool,
) -> Result<(Option<DirstatOptions>, Vec<String>)> {
    resolve_dirstat_options_from_cli(&DirstatCliState::from(args), git_dir, cli_active)
}

pub(crate) fn resolve_dirstat_options_from_cli(
    cli: &DirstatCliState,
    git_dir: &std::path::Path,
    cli_active: bool,
) -> Result<(Option<DirstatOptions>, Vec<String>)> {
    use grit_lib::config::ConfigSet;
    let config = ConfigSet::load(Some(git_dir), false).unwrap_or_else(|_| ConfigSet::new());
    let mut warnings = Vec::new();

    let mut opts = DirstatOptions::default();

    if let Some(ref cfg_val) = config.get("diff.dirstat") {
        let (o, w) = parse_dirstat_params_lenient(cfg_val);
        warnings.extend(w);
        opts = o;
    }

    // Each CLI dirstat option is parsed independently and, like Git's `parse_dirstat_opt`,
    // dies with *all* of that option's bad tokens combined under one header.
    let bail_on_cli_errors = |errs: Vec<String>| -> Result<()> {
        if errs.is_empty() {
            Ok(())
        } else {
            bail!(
                "Failed to parse --dirstat/-X option parameter:\n{}",
                errs.concat()
            )
        }
    };

    if cli.dirstat_cumulative_flag {
        let mut errs = Vec::new();
        parse_dirstat_apply_tokens_cli("cumulative", &mut opts, &mut errs);
        bail_on_cli_errors(errs)?;
    }

    if let Some(ref p) = cli.dirstat_by_file {
        let mut errs = Vec::new();
        parse_dirstat_apply_tokens_cli("files", &mut opts, &mut errs);
        if !p.is_empty() {
            parse_dirstat_apply_tokens_cli(p, &mut opts, &mut errs);
        }
        bail_on_cli_errors(errs)?;
    }

    for param in &cli.dirstat {
        // Git's `parse_dirstat_params("")` is a no-op: a bare `--dirstat` enables dirstat
        // output but applies no parameters, so the `diff.dirstat` config baseline (mode,
        // cumulative, cut-off) is preserved rather than reset to the built-in defaults.
        if !param.is_empty() {
            let mut errs = Vec::new();
            parse_dirstat_apply_tokens_cli(param, &mut opts, &mut errs);
            bail_on_cli_errors(errs)?;
        }
    }

    if !cli_active && opts.is_default_everything() && config.get("diff.dirstat").is_none() {
        return Ok((None, warnings));
    }

    Ok((Some(opts), warnings))
}

fn parse_dirstat_percentage_permille(s: &str) -> Option<u32> {
    let mut parts = s.splitn(2, '.');
    let whole = parts.next()?.parse::<u32>().ok()?;
    let mut permille = whole.saturating_mul(10);
    if let Some(rest) = parts.next() {
        let mut chars = rest.chars();
        let d = chars.next()?;
        if !d.is_ascii_digit() {
            return None;
        }
        permille = permille.saturating_add(d.to_digit(10)?);
        if chars.next().is_some_and(|c| c.is_ascii_digit()) {
            // Git ignores further fractional digits after the first
        }
        if chars.any(|c| !c.is_ascii_digit()) {
            return None;
        }
    }
    Some(permille)
}

fn dirstat_damage_for_entry(
    odb: &Odb,
    entry: &DiffEntry,
    work_tree: Option<&Path>,
    break_rewrites: bool,
    mode: DirstatMode,
) -> u64 {
    let path = entry.path();

    // Git's `show_dirstat`: before any mode-specific accounting, a pair whose pre-image and
    // post-image blob OIDs are both valid and identical contributes zero damage — even in
    // `--dirstat=files` mode. This is how a content-preserving rename/copy
    // (`{src => dst}/... 0 0`) is excluded from the file count.
    let z = zero_oid();
    if entry.old_oid != z && entry.new_oid != z && entry.old_oid == entry.new_oid {
        return 0;
    }

    let old_raw = read_content_raw(odb, &entry.old_oid);
    let new_raw = read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, path);

    match mode {
        DirstatMode::Files => 1,
        DirstatMode::Lines => {
            if break_rewrites
                && entry.status == DiffStatus::Modified
                && should_break_rewrite_for_stat(&old_raw, &new_raw)
            {
                if is_binary(&old_raw) || is_binary(&new_raw) {
                    return 0;
                }
                let ins = count_git_lines(&new_raw) as u64;
                let del = count_git_lines(&old_raw) as u64;
                return ins.saturating_add(del);
            }
            let old_content = String::from_utf8_lossy(&old_raw).into_owned();
            let new_content = String::from_utf8_lossy(&new_raw).into_owned();
            let (ins, del) = count_changes(&old_content, &new_content);
            let mut damage = (ins as u64).saturating_add(del as u64);
            if is_binary(&old_raw) || is_binary(&new_raw) {
                damage = damage.div_ceil(64);
            }
            damage
        }
        DirstatMode::Changes => {
            if entry.old_oid == entry.new_oid {
                return 0;
            }
            if entry.status == DiffStatus::Added {
                return new_raw.len() as u64;
            }
            if entry.status == DiffStatus::Deleted {
                return old_raw.len() as u64;
            }
            let (copied, added) = diffcore_count_changes(&old_raw, &new_raw);
            let old_len = old_raw.len() as u64;
            let removed = old_len.saturating_sub(copied);
            let mut damage = removed.saturating_add(added);
            if damage == 0 {
                damage = 1;
            }
            damage
        }
    }
}

fn gather_dirstat_recursive(
    out: &mut impl Write,
    files: &[DirstatFile],
    idx: &mut usize,
    changed_total: u64,
    base_len: usize,
    base: &str,
    permille: u32,
    cumulative: bool,
) -> Result<u64> {
    let mut sum = 0u64;
    let mut sources = 0u32;

    while *idx < files.len() {
        let f = &files[*idx];
        let name = f.name.as_str();
        if name.len() < base_len {
            break;
        }
        if !name.starts_with(base) {
            break;
        }
        let rel = &name[base_len..];
        if let Some(slash_rel) = rel.find('/') {
            let slash_abs = base_len + slash_rel;
            let new_base_len = slash_abs + 1;
            let sub = gather_dirstat_recursive(
                out,
                files,
                idx,
                changed_total,
                new_base_len,
                &name[..new_base_len],
                permille,
                cumulative,
            )?;
            sum = sum.saturating_add(sub);
            sources = sources.saturating_add(1);
        } else {
            sum = sum.saturating_add(f.changed);
            *idx += 1;
            sources = sources.saturating_add(2);
        }
    }

    // Git (`gather_dirstat` in diff.c): report a directory line for any directory other
    // than the top level whose damage did not all funnel through a single sub-source
    // (`sources != 1`). After printing in non-cumulative mode the directory's damage is
    // *consumed* — it returns 0 so ancestors neither re-print nor double-count it. In all
    // other cases the accumulated `sum` propagates upward unchanged.
    if base_len > 0 && sources != 1 && sum > 0 && changed_total > 0 {
        let permille_val = ((sum as u128) * 1000 / (changed_total as u128)) as u32;
        if permille_val >= permille {
            let int_part = permille_val / 10;
            let frac = permille_val % 10;
            // Git formats dirstat lines as `%4d.%01d%% %s` (no leading space; the
            // width-4 field provides the alignment).
            writeln!(out, "{:>4}.{}% {}", int_part, frac, &base[..base_len])?;
            if !cumulative {
                return Ok(0);
            }
        }
    }
    Ok(sum)
}

pub(crate) fn write_dirstat(
    out: &mut impl Write,
    opts: &DirstatOptions,
    entries: &[DiffEntry],
    odb: &Odb,
    work_tree: Option<&Path>,
    break_rewrites: bool,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut files: Vec<DirstatFile> = Vec::with_capacity(entries.len());
    let mut changed_total = 0u64;
    for e in entries {
        let name = e.path().to_string();
        let damage = dirstat_damage_for_entry(odb, e, work_tree, break_rewrites, opts.mode);
        changed_total = changed_total.saturating_add(damage);
        files.push(DirstatFile {
            name,
            changed: damage,
        });
    }
    if changed_total == 0 {
        return Ok(());
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let mut idx = 0usize;
    gather_dirstat_recursive(
        out,
        &files,
        &mut idx,
        changed_total,
        0,
        "",
        opts.permille,
        opts.cumulative,
    )?;
    Ok(())
}

/// When `core.precomposeunicode` is on, the work tree may store paths in NFC while argv uses NFD.
fn resolve_pathspec_for_diff_classification(arg: &str, precompose_unicode: bool) -> Option<String> {
    use std::path::Path;
    if Path::new(arg).symlink_metadata().is_ok() {
        return Some(arg.to_owned());
    }
    // With precompose, NFD argv must match NFC on-disk paths (macOS aliases them; Linux tests
    // force the config without FS aliasing, so the NFD spelling may not exist as a path).
    if precompose_unicode {
        let nfc = grit_lib::unicode_normalization::precompose_utf8_path(arg);
        if nfc.as_ref() != arg {
            return Some(nfc.into_owned());
        }
    }
    None
}

/// Prefix for `--relative` / `diff.relative` path stripping (trailing `/` except empty root).
///
/// `--no-relative` disables only the `diff.relative` config; explicit `--relative` on the CLI
/// still applies (matches Git, t4045).
fn resolve_diff_relative_prefix(
    work_tree: Option<&Path>,
    git_dir: &Path,
    args: &Args,
) -> Option<String> {
    let from_cli = match &args.relative {
        Some(Some(p)) if !p.is_empty() => Some(p.clone()),
        Some(_) => {
            if let Some(wt) = work_tree {
                if let Ok(cwd) = std::env::current_dir() {
                    if let Ok(rel) = cwd.strip_prefix(wt) {
                        let s = rel.to_string_lossy().to_string();
                        if s.is_empty() {
                            None
                        } else {
                            Some(format!("{s}/"))
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }
        None => None,
    };
    if from_cli.is_some() {
        return from_cli;
    }
    if args.no_relative {
        return None;
    }
    let config = ConfigSet::load(Some(git_dir), false).unwrap_or_else(|_| ConfigSet::new());
    match config.get("diff.relative") {
        Some(val) if matches!(val.to_lowercase().as_str(), "true" | "yes" | "1") => {
            if let Some(wt) = work_tree {
                if let Ok(cwd) = std::env::current_dir() {
                    if let Ok(rel) = cwd.strip_prefix(wt) {
                        let s = rel.to_string_lossy().to_string();
                        if s.is_empty() {
                            None
                        } else {
                            Some(format!("{s}/"))
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

/// True when `rel_under_wt` resolves to a path under `wt` (matches Git `path_inside_repo` for diff).
fn path_inside_work_tree(wt: &Path, rel_under_wt: &str) -> bool {
    let candidate = wt.join(rel_under_wt);
    let wt_canon = wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf());
    if let Ok(c_canon) = candidate.canonicalize() {
        return c_canon.starts_with(&wt_canon);
    }
    let Some(parent) = candidate.parent() else {
        return false;
    };
    if parent == Path::new("") {
        return true;
    }
    parent
        .canonicalize()
        .map(|p| p.starts_with(&wt_canon))
        .unwrap_or(true)
}

/// Strip the cwd-relative prefix that [`resolve_pathspec`] prepends (from [`show_prefix`]).
///
/// Diff headers use repo-root-relative paths plus `a/` / `b/`; labels must not duplicate the cwd
/// segment (matches Git when running `git diff f1 f2` from a subdirectory).
fn strip_pathspec_cwd_prefix(path: &str, cwd_prefix: Option<&str>) -> String {
    let Some(pfx) = cwd_prefix.filter(|s| !s.is_empty()) else {
        return path.to_owned();
    };
    let with_slash = format!("{pfx}/");
    path.strip_prefix(&with_slash).unwrap_or(path).to_owned()
}

/// Map a `--relative`-stripped display path back to the repository-relative path for index I/O.
fn repo_relative_path_for_relative_display(display: &str, prefix: Option<&str>) -> String {
    let Some(pfx) = prefix.filter(|s| !s.is_empty()) else {
        return display.to_owned();
    };
    if pfx.ends_with('/') {
        format!("{pfx}{display}")
    } else {
        format!("{pfx}/{display}")
    }
}

/// Repository-relative path for attributes / worktree reads when `--relative` stripped display paths.
fn repo_path_for_diff_entry(entry: &DiffEntry, relative_prefix: Option<&str>) -> String {
    match relative_prefix {
        Some(p) if !p.is_empty() => {
            repo_relative_path_for_relative_display(&entry.path().to_string_lossy(), Some(p))
        }
        _ => entry.path().to_string_lossy().into_owned(),
    }
}

/// Repo-relative path for one diff side label (`old_path` / `new_path` are display paths after `--relative`).
fn repo_path_for_diff_side(display_path: &str, relative_prefix: Option<&str>) -> String {
    if display_path == "/dev/null" {
        return display_path.to_owned();
    }
    match relative_prefix {
        Some(p) if !p.is_empty() => repo_relative_path_for_relative_display(display_path, Some(p)),
        _ => display_path.to_owned(),
    }
}

fn parse_rev_and_paths(
    args: &[String],
    has_separator: bool,
    precompose_unicode: bool,
    has_work_tree: bool,
) -> (Vec<String>, Vec<String>) {
    if let Some(sep) = args.iter().position(|a| a == "--") {
        let revs = args[..sep].to_vec();
        let paths = args[sep + 1..].to_vec();
        (revs, paths)
    } else if has_separator {
        (Vec::new(), args.to_vec())
    } else {
        // Without `--`, try to guess: if an arg exists as a file/directory,
        // treat it and everything after as paths.
        let mut revs = Vec::new();
        let mut paths = Vec::new();
        let mut in_paths = false;

        for arg in args {
            if in_paths {
                paths.push(arg.clone());
            } else if arg.starts_with(":!") || arg.starts_with(":^") {
                // Git pathspec exclusion (`:!` / `:^`); never a revision (t7012, `git diff HEAD :!path`).
                in_paths = true;
                paths.push(arg.clone());
            } else if has_work_tree {
                if let Some(p) = resolve_pathspec_for_diff_classification(arg, precompose_unicode) {
                    in_paths = true;
                    paths.push(p);
                } else {
                    revs.push(arg.clone());
                }
            } else {
                revs.push(arg.clone());
            }
        }

        (revs, paths)
    }
}

/// Get HEAD's tree OID, or `None` if the repository is unborn.
fn get_head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    let head_ref = repo.git_dir.join("HEAD");
    let content = match std::fs::read_to_string(&head_ref) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let content = content.trim();

    // Resolve symbolic ref or direct OID
    let oid_str = if let Some(symref) = content.strip_prefix("ref: ") {
        // Try loose ref file first, then fall back to packed-refs
        let ref_path = repo.git_dir.join(symref);
        match std::fs::read_to_string(&ref_path) {
            Ok(s) => s.trim().to_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Look up in packed-refs
                match resolve_packed_ref(&repo.git_dir, symref) {
                    Some(oid) => oid,
                    None => return Ok(None),
                }
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        content.to_owned()
    };

    if oid_str.is_empty() {
        return Ok(None);
    }

    let oid = ObjectId::from_hex(&oid_str).context("parsing HEAD OID")?;
    let obj = repo.odb.read(&oid).context("reading HEAD commit")?;
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;
    Ok(Some(commit.tree))
}

/// Look up a ref name in the packed-refs file.
fn resolve_packed_ref(git_dir: &std::path::Path, refname: &str) -> Option<String> {
    let packed = git_dir.join("packed-refs");
    let content = std::fs::read_to_string(packed).ok()?;
    for line in content.lines() {
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let oid = parts.next()?;
        let name = parts.next()?.trim();
        if name == refname {
            return Some(oid.to_owned());
        }
    }
    None
}

/// When `spec` is `treeish:path`, resolve the blob and path; otherwise `None`.
fn blob_side_for_treeish_path_spec(
    repo: &Repository,
    spec: &str,
) -> Result<Option<TreeishBlobAtPath>> {
    match split_treeish_colon(spec) {
        Some((_, path)) if !path.is_empty() => Ok(Some(resolve_treeish_blob_at_path(repo, spec)?)),
        _ => Ok(None),
    }
}

/// Resolve `spec` to a blob side for `git diff` when comparing two blobs (raw OID or `rev:path`).
fn blob_side_for_blob_diff_spec(
    repo: &Repository,
    spec: &str,
) -> Result<Option<TreeishBlobAtPath>> {
    if let Some(side) = blob_side_for_treeish_path_spec(repo, spec)? {
        return Ok(Some(side));
    }
    let oid = match resolve_revision(repo, spec) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    let obj = match repo.odb.read(&oid) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    if obj.kind != ObjectKind::Blob {
        return Ok(None);
    }
    Ok(Some(TreeishBlobAtPath {
        path: oid.to_hex(),
        oid,
        mode: "100644".to_owned(),
    }))
}

/// Octal mode string for a worktree file (`120000` symlink, `100755` executable, else `100644`).
fn worktree_file_mode_octal(path: &Path) -> String {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return "120000".to_owned();
        }
        #[cfg(unix)]
        {
            let mode = meta.permissions().mode();
            if mode & 0o111 != 0 {
                return "100755".to_owned();
            }
        }
    }
    "100644".to_owned()
}

/// If both sides are `rev:path` with `..` between path segments, return the two specs (Git: t4063).
fn try_treeish_blob_range(spec: &str) -> Option<(String, String)> {
    let bytes = spec.as_bytes();
    let mut i = 0usize;
    let mut peel_depth = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'^' && bytes[i + 1] == b'{' {
            peel_depth += 1;
            i += 2;
            continue;
        }
        if peel_depth > 0 {
            if bytes[i] == b'}' {
                peel_depth -= 1;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'.' && bytes.get(i + 1) == Some(&b'.') {
            let left = &spec[..i];
            let right = &spec[i + 2..];
            if split_treeish_colon(left).is_some_and(|(_, p)| !p.is_empty())
                && split_treeish_colon(right).is_some_and(|(_, p)| !p.is_empty())
            {
                return Some((left.to_owned(), right.to_owned()));
            }
        }
        i += 1;
    }
    None
}

fn peel_tag_chain_to_oid(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let obj = repo
            .odb
            .read(&oid)
            .with_context(|| format!("reading object {oid}"))?;
        if obj.kind != ObjectKind::Tag {
            return Ok(oid);
        }
        let tag = parse_tag(&obj.data).context("parsing tag")?;
        oid = tag.object;
    }
}

fn object_kind_phrase(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Tree => "tree",
        ObjectKind::Blob => "blob",
        ObjectKind::Tag => "tag",
        ObjectKind::Commit => "commit",
    }
}

/// Resolve `spec` to a commit OID, peeling annotated tags but rejecting trees/blobs (for `--merge-base`).
fn resolve_commit_ish_for_merge_base(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid =
        resolve_revision(repo, spec).with_context(|| format!("unknown revision: '{spec}'"))?;
    let peeled = peel_tag_chain_to_oid(repo, oid)?;
    let obj = repo
        .odb
        .read(&peeled)
        .with_context(|| format!("reading object {peeled}"))?;
    if obj.kind != ObjectKind::Commit {
        bail!(
            "fatal: {} is a {}, not a commit",
            spec,
            object_kind_phrase(obj.kind)
        );
    }
    Ok(peeled)
}

/// Resolve a revision spec to a tree OID, handling both commit and tree objects.
/// Resolve the preimage tree OID used to seed `--find-copies-harder` copy sources.
///
/// Mirrors the diff-mode dispatch above: a two-rev range diffs `revs[0]..revs[1]`
/// (source = `revs[0]`), a single rev (or `--cached <rev>`) diffs against that rev's
/// tree, and `--cached` with no rev diffs against `HEAD`. Index/worktree-only sources
/// (`git diff`, `git diff <rev>`) have no cheap flat preimage here, so return `None`
/// and let copy detection consider only modified files.
fn source_tree_oid_for_find_copies_harder(
    repo: &Repository,
    args: &Args,
    revs: &[String],
    head_tree: Option<&ObjectId>,
) -> Option<ObjectId> {
    match (args.cached, revs.len()) {
        (true, 0) => head_tree.copied(),
        (_, 1) => commit_or_tree_oid(repo, &revs[0]).ok(),
        (false, 2) => commit_or_tree_oid(repo, &revs[0]).ok(),
        _ => None,
    }
}

fn commit_or_tree_oid(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let mut oid =
        resolve_revision(repo, spec).with_context(|| format!("unknown revision: '{spec}'"))?;
    oid = peel_tag_chain_to_oid(repo, oid)?;
    loop {
        let obj = repo
            .odb
            .read(&oid)
            .with_context(|| format!("reading object {oid}"))?;
        match obj.kind {
            ObjectKind::Tree => return Ok(oid),
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data).context("parsing commit")?;
                oid = commit.tree;
            }
            _ => bail!("object '{}' does not name a tree or commit", oid),
        }
    }
}

/// Filter combined-diff paths like [`filter_by_paths`].
fn filter_combined_paths(
    entries: Vec<grit_lib::combined_tree_diff::CombinedDiffPath>,
    paths: &[String],
) -> Vec<grit_lib::combined_tree_diff::CombinedDiffPath> {
    if paths.is_empty() {
        return entries;
    }
    let mut include_specs: Vec<&str> = Vec::new();
    let mut exclude_inners: Vec<&str> = Vec::new();
    for spec in paths {
        if let Some(inner) = spec.strip_prefix(":!").or_else(|| spec.strip_prefix(":^")) {
            exclude_inners.push(inner);
        } else {
            include_specs.push(spec.as_str());
        }
    }
    if include_specs.is_empty() && !exclude_inners.is_empty() {
        include_specs.push(".");
    }
    entries
        .into_iter()
        .filter(|e| {
            let path = e.path.as_str();
            let included = include_specs.iter().any(|spec| {
                if spec == &"." || spec.is_empty() {
                    return true;
                }
                grit_lib::pathspec::pathspec_matches(spec, path)
            });
            let excluded = exclude_inners
                .iter()
                .any(|inner| grit_lib::pathspec::pathspec_matches(inner, path));
            included && !excluded
        })
        .collect()
}

/// Filter diff entries to only those matching the given pathspecs.
/// Empty pathspecs means include everything.
///
/// Supports Git exclude pathspecs (`:!` / `:^`): when only exclusions are given, the
/// include set defaults to `.` (all paths), then exclusions are removed (same as `git rm`).
fn filter_by_paths(entries: Vec<DiffEntry>, paths: &[String]) -> Vec<DiffEntry> {
    if paths.is_empty() {
        return entries;
    }
    let mut include_specs: Vec<&str> = Vec::new();
    let mut exclude_inners: Vec<&str> = Vec::new();
    for spec in paths {
        if let Some(inner) = spec.strip_prefix(":!").or_else(|| spec.strip_prefix(":^")) {
            exclude_inners.push(inner);
        } else {
            include_specs.push(spec.as_str());
        }
    }
    if include_specs.is_empty() && !exclude_inners.is_empty() {
        include_specs.push(".");
    }
    entries
        .into_iter()
        .filter(|e| {
            // TODO(byte-paths): lossy pathspec matching until Phase 3 byte-correct matchers.
            let path = e.path().to_string_lossy();
            // Classify the entry from its modes so `dir/` style pathspecs match
            // gitlinks (t4010: `submod/` must match submodule entry `submod`).
            let old_ctx = grit_lib::pathspec::context_from_mode_octal(&e.old_mode);
            let new_ctx = grit_lib::pathspec::context_from_mode_octal(&e.new_mode);
            let ctx = grit_lib::pathspec::PathspecMatchContext {
                is_directory: old_ctx.is_directory || new_ctx.is_directory,
                is_git_submodule: old_ctx.is_git_submodule || new_ctx.is_git_submodule,
            };
            let included = include_specs.iter().any(|spec| {
                if spec == &"." || spec.is_empty() {
                    return true;
                }
                grit_lib::pathspec::matches_pathspec_with_context(spec, &path, ctx)
            });
            let excluded = exclude_inners
                .iter()
                .any(|inner| grit_lib::pathspec::matches_pathspec_with_context(inner, &path, ctx));
            included && !excluded
        })
        .collect()
}

/// Read content for a diff entry side, falling back to the working tree if
/// the OID is not in the ODB (worktree files are hashed but not stored).
fn read_content(odb: &Odb, oid: &ObjectId, work_tree: Option<&Path>, path: &RepoPath) -> String {
    let raw = read_content_raw_or_worktree(odb, oid, work_tree, path);
    String::from_utf8_lossy(&raw).into_owned()
}

/// Drop lines matching any of the `-I` / `--ignore-matching-lines` regexes (Git: ignore whole lines).
fn apply_ignore_matching_lines_to_text(text: &str, ignore: &[Regex]) -> String {
    if ignore.is_empty() {
        return text.to_owned();
    }
    let ends_with_nl = text.ends_with('\n');
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !ignore.iter().any(|re| re.is_match(line)))
        .collect();
    let mut out = kept.join("\n");
    if ends_with_nl && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// True when a changed line (the text after the leading `+`/`-`) is "ignorable":
/// it matches one of the `-I` regexes, or (with `--ignore-blank-lines`) is blank.
fn changed_line_is_ignorable(body: &str, ignore: &[Regex], ignore_blank: bool) -> bool {
    if ignore_blank && body.trim().is_empty() {
        return true;
    }
    ignore.iter().any(|re| re.is_match(body))
}

/// Suppress hunks whose changed lines are *all* ignorable (xdiff `-I` / `--ignore-blank-lines`
/// semantics: the diff is computed normally, then hunks made up entirely of ignorable changes
/// are dropped at emission, preserving line numbers in the remaining hunks).
///
/// Operates on a single-file patch body (optional leading `diff --git`/`index`/`mode` lines, the
/// `---`/`+++` header pair, then `@@` hunks). Returns the filtered patch; when no hunk survives the
/// result is empty (the caller suppresses the whole file entry).
pub(crate) fn suppress_ignored_hunks_in_patch(
    patch: &str,
    ignore: &[Regex],
    ignore_blank: bool,
) -> String {
    if ignore.is_empty() && !ignore_blank {
        return patch.to_owned();
    }
    let lines: Vec<&str> = patch.split_inclusive('\n').collect();
    // Find where the first hunk header starts; everything before it is file header.
    let first_hunk = lines.iter().position(|l| l.starts_with("@@ "));
    let Some(first_hunk) = first_hunk else {
        // No hunks (e.g. pure mode change / binary); leave untouched.
        return patch.to_owned();
    };
    let header: String = lines[..first_hunk].concat();

    let mut kept_hunks = String::new();
    let mut any_kept = false;
    let mut i = first_hunk;
    while i < lines.len() {
        // Collect this hunk: the @@ line plus following body lines until the next @@.
        let hunk_start = i;
        i += 1;
        while i < lines.len() && !lines[i].starts_with("@@ ") {
            i += 1;
        }
        let hunk_lines = &lines[hunk_start..i];

        let mut has_change = false;
        let mut all_ignorable = true;
        for &l in &hunk_lines[1..] {
            // Trim a single trailing newline for matching.
            let l_trim = l.strip_suffix('\n').unwrap_or(l);
            if let Some(body) = l_trim.strip_prefix('+') {
                has_change = true;
                if !changed_line_is_ignorable(body, ignore, ignore_blank) {
                    all_ignorable = false;
                }
            } else if let Some(body) = l_trim.strip_prefix('-') {
                has_change = true;
                if !changed_line_is_ignorable(body, ignore, ignore_blank) {
                    all_ignorable = false;
                }
            }
            // Context lines (' ') and `\ No newline` markers don't affect ignorability.
        }

        if has_change && all_ignorable {
            // Drop this hunk entirely.
            continue;
        }
        any_kept = true;
        for &l in hunk_lines {
            kept_hunks.push_str(l);
        }
    }

    if !any_kept {
        return String::new();
    }
    let mut out = header;
    out.push_str(&kept_hunks);
    out
}

/// True when `-I` / `--ignore-matching-lines` (and `--ignore-blank-lines`) removes all substantive
/// differences, so Git hides the file entirely (no `diff --git`, no `--raw`/`--name-only` line).
///
/// Mirrors xdiff: the diff is computed normally, then a file whose every hunk consists solely of
/// ignorable changes (regex match or, with `--ignore-blank-lines`, blank lines) emits no patch.
fn entry_hidden_by_line_ignore(
    entry: &DiffEntry,
    odb: &Odb,
    work_tree: Option<&Path>,
    ws_mode: &WhitespaceMode,
    ignore: &[Regex],
) -> bool {
    let ignore_blank = ws_mode.ignore_blank_lines;
    if ignore.is_empty() && !ignore_blank {
        return false;
    }
    if matches!(
        entry.status,
        DiffStatus::Renamed | DiffStatus::Copied | DiffStatus::TypeChanged | DiffStatus::Unmerged
    ) {
        return false;
    }
    if entry.old_mode != entry.new_mode {
        return false;
    }
    let old_path = entry.old_path.as_deref().unwrap_or(entry.path());
    let new_path = entry.new_path.as_deref().unwrap_or(entry.path());
    let (old, new) = match entry.status {
        DiffStatus::Added => (
            String::new(),
            read_content(odb, &entry.new_oid, work_tree, new_path),
        ),
        DiffStatus::Deleted => (
            read_content(odb, &entry.old_oid, work_tree, old_path),
            String::new(),
        ),
        _ => (
            read_content(odb, &entry.old_oid, work_tree, old_path),
            read_content(odb, &entry.new_oid, work_tree, new_path),
        ),
    };
    // Apply non-blank whitespace normalization (blank-line ignoring is handled by hunk
    // suppression so line numbers stay intact). When other ws flags are present the file would
    // already have been dropped by the ws-equality filter, so this only refines the `-I` case.
    let (old_f, new_f) = if ws_mode.any() && !ws_mode.only_blank_lines() {
        (ws_mode.normalize(&old), ws_mode.normalize(&new))
    } else {
        (old, new)
    };
    if old_f == new_f {
        return true;
    }
    // Compute a plain unified body and check whether every hunk is ignorable.
    let body = unified_diff_histogram_hunks_only(&old_f, &new_f, 3, 0);
    if body.is_empty() {
        return true;
    }
    suppress_ignored_hunks_in_patch(&body, ignore, ignore_blank).is_empty()
}

/// Read raw bytes for a diff entry side from the ODB.
fn read_content_raw(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if *oid == zero_oid() {
        return Vec::new();
    }
    match odb.read(oid) {
        Ok(obj) => obj.data,
        Err(_) => Vec::new(),
    }
}

/// Read bytes from the work tree at `path` relative to `wt`.
///
/// For symlinks, returns the link target as UTF-8 bytes (same as blob hashing), not the
/// dereferenced file contents (t4011: `git diff` for intent-to-add symlinks vs `*.bin` rules).
fn read_worktree_file_raw(wt: &Path, path: &RepoPath) -> Option<Vec<u8>> {
    let full = path.to_fs_path(wt);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if meta.file_type().is_symlink() {
        std::fs::read_link(&full)
            .ok()
            .map(|t| t.to_string_lossy().into_owned().into_bytes())
    } else {
        std::fs::read(&full).ok()
    }
}

/// Read raw bytes, falling back to the working tree if the OID isn't in the ODB.
fn read_content_raw_or_worktree(
    odb: &Odb,
    oid: &ObjectId,
    work_tree: Option<&Path>,
    path: &RepoPath,
) -> Vec<u8> {
    if *oid == zero_oid() {
        // Empty tree / new file side: read from the work tree when available (t1501 tree diffs).
        if let Some(wt) = work_tree {
            if path.as_bytes() != b"/dev/null" {
                let p = path.to_fs_path(wt);
                if std::fs::symlink_metadata(&p)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                {
                    return Vec::new();
                }
                if let Some(data) = read_worktree_file_raw(wt, path) {
                    return data;
                }
            }
        }
        return Vec::new();
    }
    // Try ODB first
    if let Ok(obj) = odb.read(oid) {
        return obj.data;
    }
    // Fall back to reading from working tree
    if let Some(wt) = work_tree {
        if path.as_bytes() != b"/dev/null" {
            let p = path.to_fs_path(wt);
            if std::fs::symlink_metadata(&p)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                return Vec::new();
            }
            if let Some(data) = read_worktree_file_raw(wt, path) {
                return data;
            }
        }
    }
    Vec::new()
}

/// Check if content appears to be binary (contains NUL bytes in first 8KB).
fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

fn is_large_for_diff(data: &[u8], config: &ConfigSet) -> bool {
    let Some(limit) = config_big_file_threshold(config) else {
        return false;
    };
    data.len() as u64 > limit
}

fn is_binary_or_large_for_diff(data: &[u8], git_dir: &Path) -> bool {
    if is_binary(data) {
        return true;
    }
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    is_large_for_diff(data, &config)
}

fn config_big_file_threshold(config: &ConfigSet) -> Option<u64> {
    config
        .get_i64("core.bigFileThreshold")
        .or_else(|| config.get_i64("core.bigfilethreshold"))
        .and_then(|value| value.ok())
        .and_then(|value| u64::try_from(value).ok())
}

/// Insertion/deletion counts for `--stat` / `--shortstat` / `--numstat`.
///
/// When `--break-rewrites` is set and Git would treat the pair as a complete rewrite,
/// counts match Git's diffstat path (full line counts) instead of Myers line diff.
fn stat_ins_del_for_entry(
    odb: &Odb,
    entry: &DiffEntry,
    work_tree: Option<&Path>,
    break_rewrites: bool,
    line_ignore: Option<&[Regex]>,
    ws_mode: &WhitespaceMode,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
) -> (usize, usize) {
    let old_raw = read_content_raw(odb, &entry.old_oid);
    let new_raw = read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, entry.path());
    if break_rewrites
        && entry.status == DiffStatus::Modified
        && should_break_rewrite_for_stat(&old_raw, &new_raw)
    {
        if is_binary(&old_raw) || is_binary(&new_raw) {
            return (0, 0);
        }
        return (count_git_lines(&new_raw), count_git_lines(&old_raw));
    }
    let mut old_content = String::from_utf8_lossy(&old_raw).into_owned();
    let mut new_content = String::from_utf8_lossy(&new_raw).into_owned();
    if let Some(ign) = line_ignore {
        if !ign.is_empty() {
            old_content = apply_ignore_matching_lines_to_text(&old_content, ign);
            new_content = apply_ignore_matching_lines_to_text(&new_content, ign);
        }
    }
    if ws_mode.any() {
        old_content = ws_mode.normalize(&old_content);
        new_content = ws_mode.normalize(&new_content);
    }
    let (algo, hist) = diff_algorithm_for_path(&entry.path().to_string_lossy(), algo_cli, algo_ctx);
    count_changes_with_algorithm(&old_content, &new_content, algo, hist)
}

/// Write a GIT binary patch block (used by --binary).
///
/// Outputs a "GIT binary patch" header followed by a deflated+base85
/// literal representation of the new content, matching git's format.
fn write_git_binary_patch(
    out: &mut impl Write,
    _old_content: &[u8],
    new_content: &[u8],
    old_path: &str,
    new_path: &str,
) -> Result<()> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    writeln!(out, "GIT binary patch")?;
    writeln!(out, "literal {}", new_content.len())?;

    // Deflate the new content
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, new_content)?;
    let compressed = encoder.finish()?;

    // Encode in base85 lines (max 52 raw bytes per line; length prefix matches `git/diff.c`).
    for chunk in compressed.chunks(52) {
        let len_char = if chunk.len() <= 26 {
            (b'A' - 1 + chunk.len() as u8) as char
        } else {
            (b'a' - 27 + chunk.len() as u8) as char
        };
        let encoded = grit_lib::git_binary_base85::encode(chunk);
        writeln!(out, "{len_char}{encoded}")?;
    }
    writeln!(out)?;

    // Reverse patch (literal of old content)
    // For simplicity, output a literal 0 if old is empty
    if _old_content.is_empty() {
        writeln!(out, "literal 0")?;
        writeln!(out, "HcmV?d00001")?;
        writeln!(out)?;
    } else {
        writeln!(out, "literal {}", _old_content.len())?;
        let mut encoder2 = ZlibEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder2, _old_content)?;
        let compressed2 = encoder2.finish()?;
        for chunk in compressed2.chunks(52) {
            let len_char = if chunk.len() <= 26 {
                (b'A' - 1 + chunk.len() as u8) as char
            } else {
                (b'a' - 27 + chunk.len() as u8) as char
            };
            let encoded = grit_lib::git_binary_base85::encode(chunk);
            writeln!(out, "{len_char}{encoded}")?;
        }
        writeln!(out)?;
    }

    let _ = (old_path, new_path); // used in header already
    Ok(())
}

/// Write a `diff --git` header plus index/mode lines.
/// Find function context for a hunk header (same logic as grit-lib).
fn find_func_context(header: &str, old_lines: &[&str]) -> Option<String> {
    let at_pos = header.find('-')?;
    let rest = &header[at_pos + 1..];
    let comma_or_space = rest.find([',', ' '])?;
    let start_str = &rest[..comma_or_space];
    let start_line: usize = start_str.parse().ok()?;
    if start_line <= 1 {
        return None;
    }
    let search_end = (start_line - 1).min(old_lines.len());
    for i in (0..search_end).rev() {
        let line = old_lines[i];
        if !line.is_empty() {
            let first = line.as_bytes()[0];
            if first != b' ' && first != b'\t' {
                let truncated = if line.len() > 40 {
                    let mut end = 40;
                    while end > 0 && !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    &line[..end]
                } else {
                    line
                };
                return Some(truncated.to_owned());
            }
        }
    }
    None
}

#[allow(dead_code)]
fn write_diff_header(out: &mut impl Write, entry: &DiffEntry, use_color: bool) -> Result<()> {
    write_diff_header_with_abbrev(out, entry, use_color, 7, true)
}

#[allow(dead_code)]
fn write_diff_header_with_abbrev(
    out: &mut impl Write,
    entry: &DiffEntry,
    use_color: bool,
    abbrev_len: usize,
    quote_path_fully: bool,
) -> Result<()> {
    write_diff_header_with_prefix(
        out,
        entry,
        use_color,
        abbrev_len,
        "a/",
        "b/",
        quote_path_fully,
        None,
    )
}

fn write_diff_header_with_prefix(
    out: &mut impl Write,
    entry: &DiffEntry,
    use_color: bool,
    abbrev_len: usize,
    src_prefix: &str,
    dst_prefix: &str,
    quote_path_fully: bool,
    work_tree: Option<&Path>,
) -> Result<()> {
    let old_path = entry
        .old_path
        .as_deref()
        .unwrap_or(entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")));
    let new_path = entry
        .new_path
        .as_deref()
        .unwrap_or(entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")));

    let (b, r) = if use_color { (BOLD, RESET) } else { ("", "") };
    let old_git = if old_path.is_empty() {
        format!("{src_prefix}")
    } else if old_path == "/dev/null" {
        "/dev/null".to_string()
    } else {
        format_diff_path_with_prefix(src_prefix, &old_path.to_string_lossy(), quote_path_fully)
    };
    let new_git = if new_path.is_empty() {
        format!("{dst_prefix}")
    } else if new_path == "/dev/null" {
        "/dev/null".to_string()
    } else {
        format_diff_path_with_prefix(dst_prefix, &new_path.to_string_lossy(), quote_path_fully)
    };
    writeln!(out, "{b}diff --git {old_git} {new_git}{r}")?;

    let abbr = |oid: &ObjectId| -> String {
        let hex = oid.to_hex();
        let len = abbrev_len.min(hex.len());
        hex[..len].to_owned()
    };
    let abbr_pair = |old: &ObjectId, new: &ObjectId| -> (String, String) {
        let old_hex = old.to_hex();
        let new_hex = new.to_hex();
        let mut len = abbrev_len.min(old_hex.len()).min(new_hex.len());
        while len < old_hex.len().min(new_hex.len()) && old_hex[..len] == new_hex[..len] {
            len += 1;
        }
        (old_hex[..len].to_owned(), new_hex[..len].to_owned())
    };

    let new_oid_for_index_line = || -> ObjectId {
        if entry.old_mode == "160000" && entry.new_mode == "160000" && entry.new_oid == zero_oid() {
            submodule_head_oid_for_gitlink_patch(entry, work_tree)
        } else {
            entry.new_oid
        }
    };

    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "{b}new file mode {}{r}", entry.new_mode)?;
            let new_for_index_line =
                if entry.old_oid == zero_oid() && entry.new_oid == empty_blob_oid() {
                    empty_blob_oid()
                } else {
                    new_oid_for_index_line()
                };
            writeln!(
                out,
                "{b}index {}..{}{r}",
                abbr(&entry.old_oid),
                abbr(&new_for_index_line)
            )?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "{b}deleted file mode {}{r}", entry.old_mode)?;
            writeln!(
                out,
                "{b}index {}..{}{r}",
                abbr(&entry.old_oid),
                abbr(&new_oid_for_index_line())
            )?;
        }
        DiffStatus::Modified => {
            if entry.old_mode != entry.new_mode {
                writeln!(out, "{b}old mode {}{r}", entry.old_mode)?;
                writeln!(out, "{b}new mode {}{r}", entry.new_mode)?;
            }
            if let Some(pct) = entry.score {
                writeln!(out, "{b}dissimilarity index {pct}%{r}")?;
            }
            // Pure mode change with identical blob: Git omits the `index` line (t3419-rebase-patch-id).
            if entry.old_oid == entry.new_oid && entry.old_mode != entry.new_mode {
                return Ok(());
            }
            if entry.old_mode == entry.new_mode {
                let new_for_index = new_oid_for_index_line();
                let (old_abbr, new_abbr) = abbr_pair(&entry.old_oid, &new_for_index);
                writeln!(out, "{b}index {old_abbr}..{new_abbr} {}{r}", entry.old_mode)?;
            } else {
                let new_for_index = new_oid_for_index_line();
                let (old_abbr, new_abbr) = abbr_pair(&entry.old_oid, &new_for_index);
                writeln!(out, "{b}index {old_abbr}..{new_abbr}{r}")?;
            }
        }
        DiffStatus::Renamed => {
            let sim = entry.score.unwrap_or(100);
            writeln!(out, "{b}similarity index {sim}%{r}")?;
            writeln!(
                out,
                "{b}rename from {}{r}",
                quote_c_style(&old_path.to_string_lossy(), quote_path_fully)
            )?;
            writeln!(
                out,
                "{b}rename to {}{r}",
                quote_c_style(&new_path.to_string_lossy(), quote_path_fully)
            )?;
            if entry.old_oid != entry.new_oid {
                // Git appends the (shared) mode to the rename's `index` line when both
                // sides have the same mode, exactly like a modify (t4015 #99).
                if entry.old_mode == entry.new_mode {
                    writeln!(
                        out,
                        "{b}index {}..{} {}{r}",
                        abbr(&entry.old_oid),
                        abbr(&entry.new_oid),
                        entry.old_mode
                    )?;
                } else {
                    writeln!(
                        out,
                        "{b}index {}..{}{r}",
                        abbr(&entry.old_oid),
                        abbr(&entry.new_oid)
                    )?;
                }
            }
        }
        DiffStatus::Copied => {
            let sim = entry.score.unwrap_or(100);
            writeln!(out, "{b}similarity index {sim}%{r}")?;
            writeln!(
                out,
                "{b}copy from {}{r}",
                quote_c_style(&old_path.to_string_lossy(), quote_path_fully)
            )?;
            writeln!(
                out,
                "{b}copy to {}{r}",
                quote_c_style(&new_path.to_string_lossy(), quote_path_fully)
            )?;
            if entry.old_oid != entry.new_oid {
                if entry.old_mode == entry.new_mode {
                    writeln!(
                        out,
                        "{b}index {}..{} {}{r}",
                        abbr(&entry.old_oid),
                        abbr(&entry.new_oid),
                        entry.old_mode
                    )?;
                } else {
                    writeln!(
                        out,
                        "{b}index {}..{}{r}",
                        abbr(&entry.old_oid),
                        abbr(&entry.new_oid)
                    )?;
                }
            }
        }
        DiffStatus::TypeChanged => {
            writeln!(out, "{b}old mode {}{r}", entry.old_mode)?;
            writeln!(out, "{b}new mode {}{r}", entry.new_mode)?;
        }
        DiffStatus::Unmerged => {}
    }

    Ok(())
}

/// One argv "side" (old or new) for an external diff invocation.
///
/// Git's `prepare_temp_file` passes `/dev/null`, `"."`, `"."` for an invalid
/// file side (the absent half of an add/delete) and a temp-file path, full hex,
/// and 6-octal mode otherwise. We mirror that here.
struct ExtDiffSide {
    /// `temp->name`: temp-file path (or the worktree path for a borrowed file),
    /// or `/dev/null` when the side is invalid.
    name: String,
    /// `temp->hex`: 40-char hex of the blob (`.` when invalid).
    hex: String,
    /// `temp->mode`: 6-octal mode (`.` when invalid).
    mode: String,
    /// Keep the temp dir alive for the duration of the call.
    _tmp: Option<tempfile::TempDir>,
}

fn ext_diff_side(
    raw: &[u8],
    oid: &ObjectId,
    mode: &str,
    path: &str,
    borrow_path: Option<&str>,
    zero_hex: bool,
) -> Result<ExtDiffSide> {
    // mode "000000" marks the invalid side of an add/delete (DIFF_FILE_VALID == 0 in Git).
    if mode == "000000" || mode.is_empty() {
        return Ok(ExtDiffSide {
            name: "/dev/null".to_owned(),
            hex: ".".to_owned(),
            mode: ".".to_owned(),
            _tmp: None,
        });
    }
    // Gitlinks: Git's `diff_populate_gitlink` puts `Subproject commit <hex>\n`
    // into the temp blob rather than any tree/blob content (t4020 #72).
    if mode == "160000" {
        let content = format!("Subproject commit {}\n", oid.to_hex());
        let base = path.rsplit('/').next().filter(|s| !s.is_empty());
        let dir = tempfile::Builder::new()
            .prefix("git-blob-")
            .tempdir()
            .context("temp dir for external diff")?;
        let file_path = match base {
            Some(b) => dir.path().join(b),
            None => dir.path().join("blob"),
        };
        fs::write(&file_path, content.as_bytes())?;
        return Ok(ExtDiffSide {
            name: file_path.to_string_lossy().into_owned(),
            hex: oid.to_hex(),
            mode: mode.to_owned(),
            _tmp: Some(dir),
        });
    }
    // Git's `reuse_worktree_file`: a regular worktree file is borrowed in place,
    // so the argv carries the real path; the OID is null because the file is not
    // a registered blob. Symlinks (and other non-regular content) still go to a
    // temp blob but keep the null OID.
    let hex = if zero_hex {
        zero_oid().to_hex()
    } else {
        oid.to_hex()
    };
    if let Some(path) = borrow_path {
        return Ok(ExtDiffSide {
            name: path.to_owned(),
            hex,
            mode: mode.to_owned(),
            _tmp: None,
        });
    }
    // Git's `prep_temp_blob` uses `mks_tempfile_dt("git-blob-XXXXXX", base)`:
    // the blob is written into a randomly-named directory under a file whose
    // name is the path's basename, so the argv path's basename is "pretty"
    // (preserves the extension, t4020 #68).
    let base = path.rsplit('/').next().filter(|s| !s.is_empty());
    let dir = tempfile::Builder::new()
        .prefix("git-blob-")
        .tempdir()
        .context("temp dir for external diff")?;
    let file_path = match base {
        Some(b) => dir.path().join(b),
        None => dir.path().join("blob"),
    };
    fs::write(&file_path, raw)?;
    Ok(ExtDiffSide {
        name: file_path.to_string_lossy().into_owned(),
        hex,
        mode: mode.to_owned(),
        _tmp: Some(dir),
    })
}

/// Outcome of one external-diff invocation, mirroring `run_external_diff`.
enum ExtDiffOutcome {
    /// The program ran and was treated as a no-change (trustExitCode, rc==0).
    NoChange,
    /// The program ran (or was skipped) and a change is recorded.
    Changed,
    /// `fatal: external diff died, stopping at <name>` — caller must die(128).
    Died(String),
}

/// Run `GIT_EXTERNAL_DIFF` / `diff.external` / a `diff.<name>.command` driver.
///
/// Mirrors Git's `run_external_diff` + `prepare_shell_cmd` (use_shell=1):
/// argv is `cmd name old-file old-hex old-mode new-file new-hex new-mode [other]`,
/// with `GIT_DIFF_PATH_COUNTER` / `GIT_DIFF_PATH_TOTAL` exported.
#[allow(clippy::too_many_arguments)]
fn run_external_diff_for_patch(
    out: &mut impl Write,
    cmd_line: &str,
    display_path: &str,
    other_path: Option<&str>,
    old_raw: &[u8],
    new_raw: &[u8],
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    old_mode: &str,
    new_mode: &str,
    old_borrow: Option<&str>,
    old_zero_hex: bool,
    new_borrow: Option<&str>,
    new_zero_hex: bool,
    path_counter: usize,
    path_total: usize,
    trust_exit_code: bool,
    want_output: bool,
) -> Result<ExtDiffOutcome> {
    let cmd_line = cmd_line.trim();
    if cmd_line.is_empty() {
        bail!("empty external diff command");
    }

    // Git: `if (!pgm->trust_exit_code && !o->file) { o->found_changes = 1; return; }`
    // Under --quiet with an untrusted driver we never even spawn the program.
    if !trust_exit_code && !want_output {
        return Ok(ExtDiffOutcome::Changed);
    }

    // Temp-file basenames follow each side's path: the old side uses `name`,
    // the new side uses the rename destination (`other`) when present.
    let new_path = other_path.unwrap_or(display_path);
    let old_side = ext_diff_side(
        old_raw,
        old_oid,
        old_mode,
        display_path,
        old_borrow,
        old_zero_hex,
    )?;
    let new_side = ext_diff_side(
        new_raw,
        new_oid,
        new_mode,
        new_path,
        new_borrow,
        new_zero_hex,
    )?;

    const SHELL_META: &[char] = &[
        '|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', ' ', '\t', '\n', '*', '?',
        '[', '#', '~', '=', '%',
    ];
    let needs_c = cmd_line.chars().any(|c| SHELL_META.contains(&c));

    // Positional args after the program name: name, old(3), new(3), [other].
    let mut pos: Vec<&str> = vec![
        display_path,
        &old_side.name,
        &old_side.hex,
        &old_side.mode,
        &new_side.name,
        &new_side.hex,
        &new_side.mode,
    ];
    if let Some(other) = other_path {
        pos.push(other);
    }

    let mut cmd;
    if needs_c {
        // sh -c "<cmd> \"$@\"" <cmd> <pos...>
        cmd = Command::new("sh");
        let c_script = format!("{cmd_line} \"$@\"");
        cmd.arg("-c").arg(&c_script).arg(cmd_line);
        for a in &pos {
            cmd.arg(a);
        }
    } else {
        // No shell metacharacters: exec the program directly.
        cmd = Command::new(cmd_line);
        for a in &pos {
            cmd.arg(a);
        }
    }

    cmd.current_dir(external_diff_worktree_root())
        .env("GIT_PREFIX", external_diff_git_prefix())
        .env("GIT_DIFF_PATH_COUNTER", path_counter.to_string())
        .env("GIT_DIFF_PATH_TOTAL", path_total.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());

    // Git: `if (!o->file) cmd.no_stdout = 1;` — under --quiet the program's
    // stdout is discarded; only its exit status matters.
    if want_output {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null());
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn external diff {cmd_line:?}"))?;
    if want_output {
        let mut stdout = child.stdout.take().context("external diff stdout")?;
        io::copy(&mut stdout, out)?;
    }
    let status = child.wait().context("waiting for external diff")?;
    let rc = status.code().unwrap_or(-1);

    // Git's exit-code interpretation in run_external_diff().
    Ok(if !trust_exit_code && rc == 0 {
        ExtDiffOutcome::Changed
    } else if trust_exit_code && rc == 0 {
        ExtDiffOutcome::NoChange
    } else if trust_exit_code && rc == 1 {
        ExtDiffOutcome::Changed
    } else {
        ExtDiffOutcome::Died(display_path.to_owned())
    })
}

/// Resolve the `GIT_EXTERNAL_DIFF` (env) / `diff.external` (config) driver and
/// its trust-exit-code flag. Env takes precedence over config; env trust comes
/// from `GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE`, config trust from `diff.trustExitCode`.
fn resolve_env_config_external_diff(
    config: &grit_lib::config::ConfigSet,
) -> Option<(String, bool)> {
    if let Ok(cmd) = std::env::var("GIT_EXTERNAL_DIFF") {
        if !cmd.trim().is_empty() {
            let trust = std::env::var("GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE")
                .ok()
                .and_then(|v| grit_lib::config::parse_bool(v.as_str()).ok())
                .unwrap_or(false);
            return Some((cmd, trust));
        }
    }
    let cmd = config
        .get("diff.external")
        .filter(|s| !s.trim().is_empty())?;
    let trust = config
        .get("diff.trustExitCode")
        .and_then(|v| grit_lib::config::parse_bool(v.as_str()).ok())
        .unwrap_or(false);
    Some((cmd, trust))
}

fn external_diff_worktree_root() -> PathBuf {
    let mut cur = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if cur.join(".git").exists() {
            return cur;
        }
        if !cur.pop() {
            break;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn external_diff_git_prefix() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = external_diff_worktree_root();
    let Ok(rel) = cwd.strip_prefix(root) else {
        return String::new();
    };
    if rel.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", rel.to_string_lossy().replace('\\', "/"))
    }
}

fn mode_is_symlink_mode_str(mode: &str) -> bool {
    mode == "120000"
}

fn mode_is_regular_blob_mode_str(mode: &str) -> bool {
    matches!(mode, "100644" | "100755")
}

/// Git splits a regular-file ↔ symlink type change into two patches (deleted old + added new).
fn write_typechange_blob_symlink_split_patch(
    out: &mut impl Write,
    entry: &DiffEntry,
    use_color: bool,
    abbrev_len: usize,
    src_prefix: &str,
    dst_prefix: &str,
    old_path: &str,
    new_path: &str,
    old_blob_text: &str,
    new_symlink_text: &str,
    suppress_blank_empty: bool,
    context_lines: usize,
    inter_hunk_context: usize,
    path_for_attrs: &str,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
) -> Result<()> {
    let quote_path_fully = algo_ctx.config.quote_path_fully();
    let path = entry.old_path.clone().or_else(|| entry.new_path.clone());
    let del = DiffEntry {
        status: DiffStatus::Deleted,
        old_path: path.clone(),
        new_path: None,
        old_mode: entry.old_mode.clone(),
        new_mode: "000000".to_owned(),
        old_oid: entry.old_oid,
        new_oid: zero_oid(),
        score: None,
    };
    write_diff_header_with_prefix(
        out,
        &del,
        use_color,
        abbrev_len,
        src_prefix,
        dst_prefix,
        quote_path_fully,
        None,
    )?;
    write_blob_to_blob_patch_fragment(
        out,
        old_blob_text,
        "",
        old_path,
        "/dev/null",
        suppress_blank_empty,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        path_for_attrs,
        algo_ctx,
        algo_cli,
        use_color,
        false,
    )?;

    let add = DiffEntry {
        status: DiffStatus::Added,
        old_path: None,
        new_path: path,
        old_mode: "000000".to_owned(),
        new_mode: entry.new_mode.clone(),
        old_oid: zero_oid(),
        new_oid: entry.new_oid,
        score: None,
    };
    write_diff_header_with_prefix(
        out,
        &add,
        use_color,
        abbrev_len,
        src_prefix,
        dst_prefix,
        quote_path_fully,
        None,
    )?;
    write_blob_to_blob_patch_fragment(
        out,
        "",
        new_symlink_text,
        "/dev/null",
        new_path,
        suppress_blank_empty,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        path_for_attrs,
        algo_ctx,
        algo_cli,
        use_color,
        true,
    )?;
    Ok(())
}

fn write_typechange_symlink_blob_split_patch(
    out: &mut impl Write,
    entry: &DiffEntry,
    use_color: bool,
    abbrev_len: usize,
    src_prefix: &str,
    dst_prefix: &str,
    old_path: &str,
    new_path: &str,
    old_symlink_text: &str,
    new_blob_text: &str,
    suppress_blank_empty: bool,
    context_lines: usize,
    inter_hunk_context: usize,
    path_for_attrs: &str,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
) -> Result<()> {
    let quote_path_fully = algo_ctx.config.quote_path_fully();
    let path = entry.old_path.clone().or_else(|| entry.new_path.clone());
    let del = DiffEntry {
        status: DiffStatus::Deleted,
        old_path: path.clone(),
        new_path: None,
        old_mode: entry.old_mode.clone(),
        new_mode: "000000".to_owned(),
        old_oid: entry.old_oid,
        new_oid: zero_oid(),
        score: None,
    };
    write_diff_header_with_prefix(
        out,
        &del,
        use_color,
        abbrev_len,
        src_prefix,
        dst_prefix,
        quote_path_fully,
        None,
    )?;
    write_blob_to_blob_patch_fragment(
        out,
        old_symlink_text,
        "",
        old_path,
        "/dev/null",
        suppress_blank_empty,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        path_for_attrs,
        algo_ctx,
        algo_cli,
        use_color,
        false,
    )?;

    let add = DiffEntry {
        status: DiffStatus::Added,
        old_path: None,
        new_path: path,
        old_mode: "000000".to_owned(),
        new_mode: entry.new_mode.clone(),
        old_oid: zero_oid(),
        new_oid: entry.new_oid,
        score: None,
    };
    write_diff_header_with_prefix(
        out,
        &add,
        use_color,
        abbrev_len,
        src_prefix,
        dst_prefix,
        quote_path_fully,
        None,
    )?;
    write_blob_to_blob_patch_fragment(
        out,
        "",
        new_blob_text,
        "/dev/null",
        new_path,
        suppress_blank_empty,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        path_for_attrs,
        algo_ctx,
        algo_cli,
        use_color,
        false,
    )?;
    Ok(())
}

fn write_blob_to_blob_patch_fragment(
    out: &mut impl Write,
    old_content: &str,
    new_content: &str,
    display_old: &str,
    display_new: &str,
    suppress_blank_empty: bool,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    path_for_attrs: &str,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
    use_color: bool,
    suppress_incomplete_highlight_after_plus: bool,
) -> Result<()> {
    let indent_heuristic = resolve_indent_heuristic(algo_ctx.config.as_ref(), false, false);
    let quote_path_fully = algo_ctx.config.quote_path_fully();
    let (algo, use_git_histogram) = diff_algorithm_for_path(path_for_attrs, algo_cli, algo_ctx);
    let func_matcher = matcher_for_path_parsed(
        algo_ctx.config.as_ref(),
        &algo_ctx.attrs.rules,
        &algo_ctx.attrs.macros,
        path_for_attrs,
        algo_ctx.ignore_case_attrs,
    )
    .unwrap_or(None);
    let patch = unified_diff_with_prefix_and_funcname_and_algorithm(
        old_content,
        new_content,
        display_old,
        display_new,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        func_matcher.as_ref(),
        algo,
        false,
        use_git_histogram,
        indent_heuristic,
        quote_path_fully,
    );
    let patch = if suppress_blank_empty {
        strip_blank_context_trailing_space(&patch)
    } else {
        patch
    };
    if use_color {
        let entry_mode = if suppress_incomplete_highlight_after_plus {
            "120000"
        } else {
            "100644"
        };
        let ws_ctx = build_ws_color_ctx(
            algo_ctx.config.as_ref(),
            &algo_ctx.attrs,
            path_for_attrs,
            entry_mode,
            algo_ctx.ignore_case_attrs,
            old_content.as_bytes(),
            new_content.as_bytes(),
        );
        match ws_ctx {
            Some(ctx) => write_colored_patch_ws(
                out,
                &patch,
                suppress_incomplete_highlight_after_plus,
                Some(&ctx),
            )?,
            None => write_colored_patch(out, &patch, suppress_incomplete_highlight_after_plus)?,
        }
    } else {
        write!(out, "{patch}")?;
    }
    Ok(())
}

/// Build the whitespace-error-highlight context for a colored patch fragment, or `None` when
/// no highlighting applies (rule has no checkable bits, or `ws_error_highlight` selects nothing).
fn build_ws_color_ctx(
    config: &ConfigSet,
    attrs: &ParsedGitAttributes,
    path_for_attrs: &str,
    entry_mode: &str,
    ignore_case_attrs: bool,
    old_content: &[u8],
    new_content: &[u8],
) -> Option<WsColorCtx> {
    let highlight = effective_ws_error_highlight(config);
    let ws_rule =
        effective_ws_rule_for_path(attrs, path_for_attrs, entry_mode, config, ignore_case_attrs)
            .ok()?;
    // Default whitespace color is GIT_COLOR_BG_RED (`\x1b[41m`), which has no simple color name.
    let ws_color = {
        let raw = config
            .get("color.diff.whitespace")
            .or_else(|| config.get("diff.color.whitespace"));
        match raw {
            Some(v) if !v.is_empty() => grit_lib::config::parse_color(&v).unwrap_or_default(),
            _ => BG_RED.to_owned(),
        }
    };
    let (blank_at_eof_pre, blank_at_eof_post) = if ws_rule & WS_BLANK_AT_EOF != 0 {
        check_blank_at_eof(old_content, new_content)
    } else {
        (0, 0)
    };
    Some(WsColorCtx {
        ws_rule,
        highlight,
        new_color: resolve_diff_color(config, "new", "green"),
        old_color: resolve_diff_color(config, "old", "red"),
        context_color: resolve_diff_color(config, "context", "normal"),
        ws_color,
        reset: RESET.to_owned(),
        blank_at_eof_pre,
        blank_at_eof_post,
    })
}

/// Port of Git `check_blank_at_eof` (`diff.c`): returns
/// `(blank_at_eof_in_preimage, blank_at_eof_in_postimage)`, or `(0, 0)` when not applicable.
fn check_blank_at_eof(pre: &[u8], post: &[u8]) -> (usize, usize) {
    let l1 = count_trailing_blank_lines(pre);
    let l2 = count_trailing_blank_lines(post);
    if l2 <= l1 {
        return (0, 0);
    }
    let at1 = count_lines_git(pre);
    let at2 = count_lines_git(post);
    (
        at1.saturating_sub(l1).saturating_add(1),
        at2.saturating_sub(l2).saturating_add(1),
    )
}

/// Rewrite the `--- a/<path>` / `+++ b/<path>` labels produced by
/// [`no_index_unified_patch_body`] (which always uses the `a/`,`b/` prefixes) so they use the
/// caller's `src_prefix`/`dst_prefix`. The body (hunks) is left untouched.
fn rewrite_patch_label_prefixes(
    body: &str,
    src_prefix: &str,
    dst_prefix: &str,
    old_path: &str,
    new_path: &str,
    quote_path_fully: bool,
) -> String {
    let mut out = String::with_capacity(body.len());
    let old_label = if old_path == "/dev/null" {
        "--- /dev/null".to_string()
    } else {
        format!(
            "--- {}",
            format_diff_path_with_prefix(src_prefix, old_path, quote_path_fully)
        )
    };
    let new_label = if new_path == "/dev/null" {
        "+++ /dev/null".to_string()
    } else {
        format!(
            "+++ {}",
            format_diff_path_with_prefix(dst_prefix, new_path, quote_path_fully)
        )
    };
    for (i, line) in body.split_inclusive('\n').enumerate() {
        match i {
            0 if line.starts_with("--- ") => {
                out.push_str(&old_label);
                out.push('\n');
            }
            1 if line.starts_with("+++ ") => {
                out.push_str(&new_label);
                out.push('\n');
            }
            _ => out.push_str(line),
        }
    }
    out
}

fn write_patch_with_prefix(
    out: &mut impl Write,
    repo: &Repository,
    entries: &[DiffEntry],
    odb: &Odb,
    git_dir: &Path,
    config: &grit_lib::config::ConfigSet,
    context_lines: usize,
    use_color: bool,
    word_diff: Option<&EffectiveWordDiff>,
    work_tree: Option<&Path>,
    suppress_blank_empty: bool,
    abbrev_len: usize,
    inter_hunk_context: usize,
    show_binary: bool,
    break_rewrites: bool,
    irreversible_delete: bool,
    use_textconv: bool,
    src_prefix: &str,
    dst_prefix: &str,
    quote_path_fully: bool,
    submodule_fmt: Option<&str>,
    submodule_ignore: SubmoduleIgnoreFlags,
    ignore_submodules_cli: Option<&str>,
    diff_cfg: &grit_lib::config::ConfigSet,
    path_to_sm_name: &HashMap<String, String>,
    gm_sub_ignore: &HashMap<String, String>,
    line_ignore: Option<&[Regex]>,
    ws_mode: &WhitespaceMode,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
    cached: bool,
    function_context: bool,
    no_ext_diff: bool,
    external_diff: Option<(&str, bool)>,
    want_external_output: bool,
    relative_prefix: Option<&str>,
    indent_heuristic: bool,
) -> Result<ExtDiffSummary> {
    let ignore_blank = ws_mode.ignore_blank_lines;
    // External-diff bookkeeping mirroring Git's diff_options found_changes / die.
    let mut ext_invoked = false;
    let mut ext_found_changes = false;
    let mut ext_died: Option<String> = None;
    let mut ext_counter: usize = 0;
    for entry in entries {
        let old_path = entry.old_path.as_deref().unwrap_or(RepoPath::from_str("/dev/null"));
        let new_path = entry.new_path.as_deref().unwrap_or(RepoPath::from_str("/dev/null"));
        let path_for_attrs = repo_path_for_diff_entry(entry, relative_prefix);

        // `git diff --cached` reports an unmerged (conflicted) index entry as a single
        // `* Unmerged path <name>` line — it has no stage-0 blob to diff against HEAD
        // (t3701 "patch mode ignores unmerged entries").
        if cached && entry.status == DiffStatus::Unmerged {
            writeln!(out, "* Unmerged path {}", entry.path())?;
            continue;
        }

        if let Some(fmt) = submodule_fmt {
            if entry.old_mode == "160000" || entry.new_mode == "160000" {
                // A blob→gitlink typechange against the work tree records the new gitlink as zero;
                // resolve the submodule's checked-out HEAD so `write_patch_entry` produces the
                // `Submodule … (new submodule)` summary with the real commit. See t4041/t4060 #17.
                let mut resolved;
                let entry: &DiffEntry = if entry.new_mode == "160000"
                    && entry.new_oid == zero_oid()
                    && entry.old_mode != "160000"
                {
                    let head = work_tree
                        .and_then(|wt| {
                            grit_lib::diff::read_submodule_head_oid(&entry.path().to_fs_path(wt))
                        })
                        .unwrap_or_else(zero_oid);
                    if head == zero_oid() {
                        entry
                    } else {
                        resolved = entry.clone();
                        resolved.new_oid = head;
                        &resolved
                    }
                } else {
                    entry
                };
                if fmt == "log" {
                    if entry.old_mode == "160000"
                        && entry.new_mode == "160000"
                        && matches!(
                            entry.status,
                            DiffStatus::Modified | DiffStatus::Renamed | DiffStatus::Copied
                        )
                        && entry.old_oid != entry.new_oid
                    {
                        write_submodule_log_lines(out, repo, entry, work_tree)?;
                    } else {
                        write_patch_entry(
                            out,
                            repo,
                            odb,
                            entry,
                            context_lines,
                            work_tree,
                            SubmodulePatchFormat::Log,
                            submodule_ignore,
                            &path_for_attrs,
                            indent_heuristic,
                        )?;
                    }
                    continue;
                }
                if fmt == "diff" {
                    write_patch_entry(
                        out,
                        repo,
                        odb,
                        entry,
                        context_lines,
                        work_tree,
                        SubmodulePatchFormat::Diff,
                        submodule_ignore,
                        &path_for_attrs,
                        indent_heuristic,
                    )?;
                    continue;
                }
            }
        }

        let old_wt_path = repo_path_for_diff_side(&old_path.to_string_lossy(), relative_prefix);
        let old_content_raw = if entry.old_oid == zero_oid() {
            read_content_raw_or_worktree(odb, &entry.old_oid, work_tree, RepoPath::from_str(&old_wt_path))
        } else {
            read_content_raw(odb, &entry.old_oid)
        };
        let wt_path = path_for_attrs.clone();
        let new_content_raw =
            read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, RepoPath::from_str(&wt_path));

        if !no_ext_diff && entry.status != DiffStatus::Unmerged {
            // Git precedence: a path's `diff=<name>` attribute driver
            // (`diff.<name>.command`) trumps `GIT_EXTERNAL_DIFF` / `diff.external`.
            let attr_driver = grit_lib::merge_diff::diff_attr_external_driver(
                git_dir,
                config,
                path_for_attrs.as_str(),
            );
            let resolved: Option<(&str, bool)> = if let Some((ref c, t)) = attr_driver {
                Some((c.as_str(), t))
            } else {
                external_diff.filter(|(s, _)| !s.is_empty())
            };
            if let Some((ext, trust)) = resolved {
                ext_invoked = true;
                let ext_total = entries.len();
                ext_counter += 1;
                // `other` = the destination path for renames/copies (Git's `two->path`).
                let display = entry.path();
                let other = match entry.status {
                    DiffStatus::Renamed | DiffStatus::Copied => {
                        entry.new_path.as_deref().filter(|n| *n != display)
                    }
                    _ => None,
                };
                // Git's `reuse_worktree_file`: the unstaged worktree side of a
                // (non-`--cached`) diff carries a null OID. A *regular* worktree
                // file is also borrowed in place — argv carries the real path
                // instead of a temp file (t4020 #2/#68). Symlinks (#6) still go
                // to a temp blob but keep the null OID. Gitlinks never borrow.
                let new_is_worktree = !cached
                    && work_tree.is_some()
                    && entry.new_mode != "000000"
                    && entry.new_mode != "160000";
                let new_zero_hex = new_is_worktree;
                let new_borrow: Option<String> =
                    if new_is_worktree && mode_is_regular_blob_mode_str(&entry.new_mode) {
                        let p = repo_path_for_diff_side(&new_path.to_string_lossy(), relative_prefix);
                        work_tree.and_then(|wt| {
                            let full = wt.join(&p);
                            full.symlink_metadata().ok().map(|_| p.clone())
                        })
                    } else {
                        None
                    };
                // Git's `prep_temp_blob` runs `convert_to_working_tree` on each
                // blob temp file, so e.g. `core.autocrlf` LF→CRLF applies to the
                // bytes the driver sees (t4020 #69). Borrowed worktree files are
                // already in working-tree form and are left untouched.
                let conv_cfg = grit_lib::crlf::ConversionConfig::from_config(config);
                let attr_rules = grit_lib::crlf::load_gitattributes(
                    work_tree.unwrap_or_else(|| git_dir.parent().unwrap_or(git_dir)),
                );
                let convert_blob = |raw: &[u8], rel: &str, oid: &ObjectId| -> Vec<u8> {
                    let fa = grit_lib::crlf::get_file_attrs(&attr_rules, rel, false, config);
                    let oid_hex = oid.to_hex();
                    grit_lib::crlf::convert_to_worktree_eager(
                        raw,
                        rel,
                        &conv_cfg,
                        &fa,
                        Some(&oid_hex),
                        None,
                    )
                    .unwrap_or_else(|_| raw.to_vec())
                };
                let old_conv: Vec<u8> = if entry.old_mode == "000000"
                    || entry.old_mode == "160000"
                    || entry.old_mode == "120000"
                {
                    old_content_raw.clone()
                } else {
                    convert_blob(&old_content_raw, path_for_attrs.as_str(), &entry.old_oid)
                };
                let new_conv: Vec<u8> = if new_borrow.is_some()
                    || entry.new_mode == "000000"
                    || entry.new_mode == "160000"
                    || entry.new_mode == "120000"
                {
                    new_content_raw.clone()
                } else {
                    convert_blob(&new_content_raw, path_for_attrs.as_str(), &entry.new_oid)
                };
                match run_external_diff_for_patch(
                    out,
                    ext,
                    &display.to_string_lossy(),
                    other.map(|o| o.to_string_lossy()).as_deref(),
                    &old_conv,
                    &new_conv,
                    &entry.old_oid,
                    &entry.new_oid,
                    &entry.old_mode,
                    &entry.new_mode,
                    None,
                    false,
                    new_borrow.as_deref(),
                    new_zero_hex,
                    ext_counter,
                    ext_total,
                    trust,
                    want_external_output,
                )? {
                    ExtDiffOutcome::Changed => ext_found_changes = true,
                    ExtDiffOutcome::NoChange => {}
                    ExtDiffOutcome::Died(p) => {
                        ext_died = Some(p);
                        break;
                    }
                }
                continue;
            }
        }

        let old_sy = mode_is_symlink_mode_str(&entry.old_mode);
        let new_sy = mode_is_symlink_mode_str(&entry.new_mode);
        let old_reg = mode_is_regular_blob_mode_str(&entry.old_mode);
        let new_reg = mode_is_regular_blob_mode_str(&entry.new_mode);
        let blob_symlink_type_flip = (old_reg && new_sy) || (old_sy && new_reg);
        if blob_symlink_type_flip
            && matches!(entry.status, DiffStatus::TypeChanged | DiffStatus::Modified)
        {
            let textconv_patch =
                use_textconv && diff_textconv_active(git_dir, config, path_for_attrs.as_str());
            let old_t = if entry.old_oid == zero_oid() {
                String::new()
            } else if textconv_patch {
                blob_text_for_diff_with_oid(
                    odb,
                    git_dir,
                    config,
                    path_for_attrs.as_str(),
                    &old_content_raw,
                    &entry.old_oid,
                    true,
                )
            } else {
                String::from_utf8_lossy(&old_content_raw).into_owned()
            };
            let new_t = if entry.new_oid == zero_oid() {
                String::new()
            } else if textconv_patch {
                blob_text_for_diff_with_oid(
                    odb,
                    git_dir,
                    config,
                    path_for_attrs.as_str(),
                    &new_content_raw,
                    &entry.new_oid,
                    true,
                )
            } else {
                String::from_utf8_lossy(&new_content_raw).into_owned()
            };
            if old_reg && new_sy {
                write_typechange_blob_symlink_split_patch(
                    out,
                    entry,
                    use_color,
                    abbrev_len,
                    src_prefix,
                    dst_prefix,
                    &old_path.to_string_lossy(),
                    &new_path.to_string_lossy(),
                    &old_t,
                    &new_t,
                    suppress_blank_empty,
                    context_lines,
                    inter_hunk_context,
                    path_for_attrs.as_str(),
                    algo_ctx,
                    algo_cli,
                )?;
            } else {
                write_typechange_symlink_blob_split_patch(
                    out,
                    entry,
                    use_color,
                    abbrev_len,
                    src_prefix,
                    dst_prefix,
                    &old_path.to_string_lossy(),
                    &new_path.to_string_lossy(),
                    &old_t,
                    &new_t,
                    suppress_blank_empty,
                    context_lines,
                    inter_hunk_context,
                    path_for_attrs.as_str(),
                    algo_ctx,
                    algo_cli,
                )?;
            }
            continue;
        }

        write_diff_header_with_prefix(
            out,
            entry,
            use_color,
            abbrev_len,
            src_prefix,
            dst_prefix,
            quote_path_fully,
            work_tree,
        )?;

        // Submodule gitlink (mode 160000): emit `Subproject commit` hunks like Git — the ODB
        // does not store these as blobs in the superproject (`git apply` / t4137 rely on this).
        if entry.old_mode == "160000" || entry.new_mode == "160000" {
            let (old_label, new_label) = match entry.status {
                DiffStatus::Added => (
                    "/dev/null".to_owned(),
                    format_diff_path_with_prefix(dst_prefix, &new_path.to_string_lossy(), quote_path_fully),
                ),
                DiffStatus::Deleted => (
                    format_diff_path_with_prefix(src_prefix, &old_path.to_string_lossy(), quote_path_fully),
                    "/dev/null".to_owned(),
                ),
                _ => (
                    format_diff_path_with_prefix(src_prefix, &old_path.to_string_lossy(), quote_path_fully),
                    format_diff_path_with_prefix(dst_prefix, &new_path.to_string_lossy(), quote_path_fully),
                ),
            };
            writeln!(out, "--- {old_label}")?;
            writeln!(out, "+++ {new_label}")?;
            match entry.status {
                DiffStatus::Added => {
                    writeln!(out, "@@ -0,0 +1 @@")?;
                    writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
                }
                DiffStatus::Deleted => {
                    writeln!(out, "@@ -1 +0,0 @@")?;
                    writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                }
                DiffStatus::Modified | DiffStatus::Renamed | DiffStatus::Copied => {
                    if entry.old_mode == "160000" && entry.new_mode == "160000" {
                        writeln!(out, "@@ -1 +1 @@")?;
                        writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                        let plus_oid = submodule_head_oid_for_gitlink_patch(entry, work_tree);
                        let plus_suffix = submodule_gitlink_patch_plus_suffix(
                            entry,
                            work_tree,
                            submodule_ignore,
                            ignore_submodules_cli,
                            diff_cfg,
                            path_to_sm_name,
                            gm_sub_ignore,
                        );
                        writeln!(out, "+Subproject commit {}{plus_suffix}", plus_oid.to_hex())?;
                    } else if entry.old_mode == "160000" {
                        writeln!(out, "@@ -1 +0,0 @@")?;
                        writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                    } else {
                        writeln!(out, "@@ -0,0 +1 @@")?;
                        writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
                    }
                }
                DiffStatus::TypeChanged => {
                    if entry.old_mode == "160000" {
                        writeln!(out, "@@ -1 +0,0 @@")?;
                        writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                    } else if entry.new_mode == "160000" {
                        writeln!(out, "@@ -0,0 +1 @@")?;
                        writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
                    }
                }
                DiffStatus::Unmerged => {}
            }
            continue;
        }

        // Check for binary content
        // `git diff --cached` for a newly staged empty file: header + index line only, no hunks
        // (t1501-work-tree `diff-TREE-cached.expected`).
        if cached && entry.status == DiffStatus::Added && new_content_raw.is_empty() {
            continue;
        }

        if entry.status == DiffStatus::Modified
            && entry.old_oid == entry.new_oid
            && entry.old_mode != entry.new_mode
        {
            continue;
        }

        if entry.status == DiffStatus::Deleted && irreversible_delete {
            continue;
        }

        let textconv_patch =
            use_textconv && diff_textconv_active(git_dir, config, path_for_attrs.as_str());
        let forced_binary = diff_forced_binary_by_driver(
            git_dir,
            config,
            path_for_attrs.as_str(),
            entry.old_mode.as_str(),
            entry.new_mode.as_str(),
        );
        // A `-diff` (binary) attribute forces `Binary files ... differ`, even on
        // text content (t4020 #18). Symlinks still get a textual target patch.
        let attr_unset_binary = entry.old_mode != "120000"
            && entry.new_mode != "120000"
            && grit_lib::merge_diff::diff_attr_forces_binary(git_dir, path_for_attrs.as_str());
        // A bare `diff` (set) attribute forces a textual diff even for NUL-bearing
        // content (t4020 #65), overriding the content-based binary heuristic.
        let attr_force_text =
            grit_lib::merge_diff::diff_attr_forces_text(git_dir, path_for_attrs.as_str());
        if !textconv_patch
            && !attr_force_text
            && (forced_binary
                || attr_unset_binary
                || is_large_for_diff(&old_content_raw, config)
                || is_large_for_diff(&new_content_raw, config)
                || is_binary(&old_content_raw)
                || is_binary(&new_content_raw))
        {
            if show_binary {
                // --binary: output a "GIT binary patch" block
                write_git_binary_patch(
                    out,
                    &old_content_raw,
                    &new_content_raw,
                    &old_path.to_string_lossy(),
                    &new_path.to_string_lossy(),
                )?;
            } else {
                let bo = if old_path == "/dev/null" {
                    "/dev/null".to_string()
                } else {
                    format_diff_path_with_prefix(src_prefix, &old_path.to_string_lossy(), quote_path_fully)
                };
                let bn = if new_path == "/dev/null" {
                    "/dev/null".to_string()
                } else {
                    format_diff_path_with_prefix(dst_prefix, &new_path.to_string_lossy(), quote_path_fully)
                };
                writeln!(out, "Binary files {bo} and {bn} differ")?;
            }
            continue;
        }

        let old_content = if entry.old_oid == zero_oid() {
            String::new()
        } else if use_textconv {
            blob_text_for_diff_with_oid(
                odb,
                git_dir,
                config,
                path_for_attrs.as_str(),
                &old_content_raw,
                &entry.old_oid,
                true,
            )
        } else {
            String::from_utf8_lossy(&old_content_raw).into_owned()
        };
        let new_content = if entry.new_oid == zero_oid() {
            String::new()
        } else if use_textconv {
            blob_text_for_diff_with_oid(
                odb,
                git_dir,
                config,
                path_for_attrs.as_str(),
                &new_content_raw,
                &entry.new_oid,
                true,
            )
        } else {
            String::from_utf8_lossy(&new_content_raw).into_owned()
        };
        // `-I` / `--ignore-blank-lines`: the diff is computed on the *full* content (so line
        // numbers are preserved) and then ignorable hunks are dropped from the rendered patch
        // below — never by pre-deleting lines here.

        // Pure rename/copy with no textual change to show: emit only the rename/copy header
        // (similarity + rename/copy from/to + index), not the `---`/`+++`/hunk body. Git's
        // `builtin_diff` skips the textual section when `found_changes` is false, which also
        // covers `-w`/`-b` renames whose content is identical once whitespace is normalised
        // (t4015 #99, #100). Added/Deleted/Modified always go through the body path.
        if matches!(entry.status, DiffStatus::Renamed | DiffStatus::Copied) {
            let bodies_equal = if ws_mode.any() {
                ws_mode.normalize(&old_content) == ws_mode.normalize(&new_content)
            } else {
                old_content == new_content
            };
            if bodies_equal {
                continue;
            }
        }

        // Intent-to-add empty file: header + `index 0000000..e69de29` only (t2203).
        if !cached
            && entry.status == DiffStatus::Added
            && entry.old_oid == zero_oid()
            && old_content.is_empty()
            && new_content.is_empty()
        {
            continue;
        }

        // Adding or deleting an empty file (both blobs empty) has no content hunk: git emits only
        // the `diff --git` / `new|deleted file mode` / `index` header, with no `--- / +++` lines
        // (t3701 "deleting an empty file" / "adding an empty file").
        if matches!(entry.status, DiffStatus::Added | DiffStatus::Deleted)
            && old_content.is_empty()
            && new_content.is_empty()
        {
            continue;
        }

        // For Added files, show --- /dev/null; for Deleted files, show +++ /dev/null
        let old_path_lossy = old_path.to_string_lossy();
        let new_path_lossy = new_path.to_string_lossy();
        let display_old: &str = if entry.status == DiffStatus::Added {
            "/dev/null"
        } else {
            &old_path_lossy
        };
        let display_new: &str = if entry.status == DiffStatus::Deleted {
            "/dev/null"
        } else {
            &new_path_lossy
        };

        if break_rewrites
            && irreversible_delete
            && entry.status == DiffStatus::Modified
            && entry.score.is_some()
        {
            let new_lc = count_git_lines(&new_content_raw);
            let new_start = if new_lc == 0 { 0 } else { 1 };
            writeln!(
                out,
                "--- {}",
                if display_old == "/dev/null" {
                    "/dev/null".to_owned()
                } else {
                    format_diff_path_with_prefix(src_prefix, display_old, quote_path_fully)
                }
            )?;
            writeln!(
                out,
                "+++ {}",
                if display_new == "/dev/null" {
                    "/dev/null".to_owned()
                } else {
                    format_diff_path_with_prefix(dst_prefix, display_new, quote_path_fully)
                }
            )?;
            writeln!(out, "@@ -?,? +{new_start},{new_lc} @@")?;
            for chunk in new_content.split_inclusive('\n') {
                if chunk.ends_with('\n') {
                    let body = chunk.strip_suffix('\n').unwrap_or(chunk);
                    writeln!(out, "+{body}")?;
                } else if !chunk.is_empty() {
                    writeln!(out, "+{chunk}")?;
                    writeln!(out, "\\ No newline at end of file")?;
                }
            }
            continue;
        }

        if let Some(wd) = word_diff {
            let word_re_opt = if let Some(ref o) = wd.regex_override {
                WordRe::build(o, false)
            } else {
                word_regex_pattern_for_path_parsed(
                    config,
                    &algo_ctx.attrs.rules,
                    &algo_ctx.attrs.macros,
                    path_for_attrs.as_str(),
                    algo_ctx.ignore_case_attrs,
                )
                .and_then(|(pat, ic)| WordRe::build(&pat, ic))
            };
            let patch = word_diff_generate_patch(
                config,
                &old_content,
                &new_content,
                display_old,
                display_new,
                context_lines,
                inter_hunk_context,
                src_prefix,
                dst_prefix,
                wd,
                use_color,
                word_re_opt.as_ref(),
                algo_ctx,
                algo_cli,
                indent_heuristic,
                quote_path_fully,
            );
            let patch = if suppress_blank_empty {
                strip_blank_context_trailing_space(&patch)
            } else {
                patch
            };
            write!(out, "{patch}")?;
        } else {
            let (algo, use_git_histogram) =
                diff_algorithm_for_path(path_for_attrs.as_str(), algo_cli, algo_ctx);
            let func_matcher = matcher_for_path_parsed(
                algo_ctx.config.as_ref(),
                &algo_ctx.attrs.rules,
                &algo_ctx.attrs.macros,
                path_for_attrs.as_str(),
                algo_ctx.ignore_case_attrs,
            )
            .unwrap_or(None);
            // When a whitespace-ignore mode other than (or in addition to) blank-line
            // ignoring is active, diff normalised line keys but emit the original lines
            // (Git's xdiff whitespace flags). `no_index_unified_patch_body` already does
            // exactly this; reuse it so `-b`, `--ignore-space-at-eol`, and
            // `--ignore-cr-at-eol` collapse whitespace-only hunks here too.
            let ws_aware = ws_mode.any()
                && (ws_mode.ignore_space_change
                    || ws_mode.ignore_all_space
                    || ws_mode.ignore_space_at_eol
                    || ws_mode.ignore_cr_at_eol);
            // `--ignore-blank-lines` (alone) and `-I` regex ignoring are handled at the
            // xdiff change-record level (Git's `xdl_mark_ignorable_lines` + `xdl_get_hunk`):
            // the diff is computed normally, then changes that are entirely ignorable are
            // dropped when far from substantive changes and otherwise shown, with hunk
            // boundaries recomputed so line numbers and surviving context match Git. This
            // only applies when no other whitespace flag and no function-context expansion
            // is active (those need the normalising / function-context paths below).
            let line_level_ignore = !ws_aware
                && !function_context
                && (ignore_blank || line_ignore.is_some_and(|i| !i.is_empty()));
            let patch = if line_level_ignore {
                let ign: &[Regex] = line_ignore.unwrap_or(&[]);
                let is_ignorable = |removed: &[&str], added: &[&str]| -> bool {
                    removed
                        .iter()
                        .chain(added.iter())
                        .all(|line| changed_line_is_ignorable(line, ign, ignore_blank))
                };
                grit_lib::diff::unified_diff_histogram_ignore_with_prefix_and_funcname(
                    &old_content,
                    &new_content,
                    display_old,
                    display_new,
                    context_lines,
                    inter_hunk_context,
                    src_prefix,
                    dst_prefix,
                    func_matcher.as_ref(),
                    quote_path_fully,
                    function_context,
                    is_ignorable,
                )
            } else if ws_aware {
                let body = no_index_unified_patch_body(
                    old_content.as_bytes(),
                    new_content.as_bytes(),
                    display_old,
                    display_new,
                    context_lines,
                    inter_hunk_context,
                    ws_mode,
                    algo,
                    false,
                    indent_heuristic,
                    quote_path_fully,
                );
                // `no_index_unified_patch_body` emits `--- a/<old>` / `+++ b/<new>` labels using a
                // fixed `a/`,`b/` prefix; rewrite them to the caller's prefixes.
                let body = rewrite_patch_label_prefixes(
                    &body,
                    src_prefix,
                    dst_prefix,
                    display_old,
                    display_new,
                    quote_path_fully,
                );
                // `-b`/`-w`/etc. combined with `--ignore-blank-lines` or `-I`: drop hunks
                // whose surviving changes are all ignorable (old post-hoc behaviour).
                match line_ignore {
                    Some(ign) if !ign.is_empty() || ignore_blank => {
                        suppress_ignored_hunks_in_patch(&body, ign, ignore_blank)
                    }
                    _ if ignore_blank => suppress_ignored_hunks_in_patch(&body, &[], ignore_blank),
                    _ => body,
                }
            } else {
                let patch = unified_diff_with_prefix_and_funcname_and_algorithm(
                    &old_content,
                    &new_content,
                    display_old,
                    display_new,
                    context_lines,
                    inter_hunk_context,
                    src_prefix,
                    dst_prefix,
                    func_matcher.as_ref(),
                    algo,
                    function_context,
                    use_git_histogram,
                    indent_heuristic,
                    quote_path_fully,
                );
                // function-context path: still apply the old post-hoc hunk suppression
                // for `--ignore-blank-lines` / `-I`.
                match line_ignore {
                    Some(ign) if !ign.is_empty() || ignore_blank => {
                        suppress_ignored_hunks_in_patch(&patch, ign, ignore_blank)
                    }
                    _ if ignore_blank => suppress_ignored_hunks_in_patch(&patch, &[], ignore_blank),
                    _ => patch,
                }
            };
            let patch = if suppress_blank_empty {
                strip_blank_context_trailing_space(&patch)
            } else {
                patch
            };

            if use_color {
                let ws_ctx = build_ws_color_ctx(
                    config,
                    &algo_ctx.attrs,
                    path_for_attrs.as_str(),
                    &entry.new_mode,
                    algo_ctx.ignore_case_attrs,
                    old_content.as_bytes(),
                    new_content.as_bytes(),
                );
                match ws_ctx {
                    Some(ctx) => write_colored_patch_ws(out, &patch, false, Some(&ctx))?,
                    None => write_colored_patch(out, &patch, false)?,
                }
            } else {
                write!(out, "{patch}")?;
            }
        }
    }
    Ok(ExtDiffSummary {
        invoked: ext_invoked,
        found_changes: ext_found_changes,
        died_path: ext_died,
    })
}

/// Summary of external-diff invocations across one patch run.
struct ExtDiffSummary {
    /// True if at least one external driver was actually invoked (or skipped via
    /// the untrusted-`--quiet` shortcut). When false, `found_changes` is
    /// meaningless and the caller falls back to the entry-derived `has_diff`.
    invoked: bool,
    /// True if any external driver reported a change (Git's `found_changes`).
    found_changes: bool,
    /// If `Some(path)`, an external driver "died" at this path → exit 128.
    died_path: Option<String>,
}

/// Strip trailing space from blank context lines in unified diff output.
///
/// When `diff.suppressBlankEmpty` is true, context lines that consist of
/// only a single space (empty context line) should become truly empty lines.
fn strip_blank_context_trailing_space(patch: &str) -> String {
    let mut result = String::with_capacity(patch.len());
    for line in patch.split_inclusive('\n') {
        if line == " \n" {
            result.push('\n');
        } else {
            result.push_str(line);
        }
    }
    result
}

/// Write a unified diff patch with ANSI color codes.
///
/// When `suppress_incomplete_red_after_plus` is true, the `\ No newline at end of file` marker that
/// follows a `+` line is not painted with the whitespace-error background (symlink post-image:
/// `t4015-diff-whitespace`).
/// Strip ANSI SGR escape sequences (`ESC [ ... m`) from a string, recovering the
/// plain text of an already-colored diff line.
fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // skip until 'm'
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' {
                j += 1;
            }
            i = if j < bytes.len() { j + 1 } else { j };
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Resolved color escapes for the eight `--color-moved` slots.
struct MovedColors {
    old_moved: String,
    old_moved_alt: String,
    old_moved_dim: String,
    old_moved_alt_dim: String,
    new_moved: String,
    new_moved_alt: String,
    new_moved_dim: String,
    new_moved_alt_dim: String,
    reset: String,
}

impl MovedColors {
    fn load(config: &ConfigSet) -> Self {
        // Git's default escapes for the moved slots.
        MovedColors {
            old_moved: resolve_diff_color(config, "oldMoved", "bold magenta"),
            old_moved_alt: resolve_diff_color(config, "oldMovedAlternative", "bold blue"),
            old_moved_dim: resolve_diff_color(config, "oldMovedDimmed", "dim"),
            old_moved_alt_dim: resolve_diff_color(
                config,
                "oldMovedAlternativeDimmed",
                "dim italic",
            ),
            new_moved: resolve_diff_color(config, "newMoved", "bold cyan"),
            new_moved_alt: resolve_diff_color(config, "newMovedAlternative", "bold yellow"),
            new_moved_dim: resolve_diff_color(config, "newMovedDimmed", "dim"),
            new_moved_alt_dim: resolve_diff_color(
                config,
                "newMovedAlternativeDimmed",
                "dim italic",
            ),
            reset: RESET.to_owned(),
        }
    }
}

/// Post-process an already-colored multi-file diff to color moved lines
/// (`--color-moved`). Recovers the plain text by stripping ANSI, runs move
/// detection, then re-emits moved `+`/`-` lines using the moved color slot for
/// the whole `<sign><body>` (matching Git's `<MOVED>-text<RESET>` output).
fn apply_color_moved(
    colored: &str,
    mode: grit_lib::diff_moved::ColorMovedMode,
    ws_flags: u32,
    colors: &MovedColors,
) -> String {
    use grit_lib::diff_moved::{detect_moved_lines, MovedClass};

    let colored_lines: Vec<&str> = colored.split_inclusive('\n').collect();
    // Reconstruct the plain patch (used only for move detection).
    let plain: String = colored_lines.iter().map(|l| strip_ansi(l)).collect();
    let classes = detect_moved_lines(&plain, mode, ws_flags);

    let mut out = String::with_capacity(colored.len());
    for (idx, &cline) in colored_lines.iter().enumerate() {
        let class = classes.get(idx).copied().unwrap_or(MovedClass::None);
        if class == MovedClass::None {
            out.push_str(cline);
            continue;
        }
        let plain_line = strip_ansi(cline);
        let is_plus = plain_line.starts_with('+');
        let color = match (is_plus, class) {
            (false, MovedClass::Moved) => &colors.old_moved,
            (false, MovedClass::MovedAlt) => &colors.old_moved_alt,
            (false, MovedClass::MovedDim) => &colors.old_moved_dim,
            (false, MovedClass::MovedAltDim) => &colors.old_moved_alt_dim,
            (true, MovedClass::Moved) => &colors.new_moved,
            (true, MovedClass::MovedAlt) => &colors.new_moved_alt,
            (true, MovedClass::MovedDim) => &colors.new_moved_dim,
            (true, MovedClass::MovedAltDim) => &colors.new_moved_alt_dim,
            (_, MovedClass::None) => unreachable!(),
        };
        // Replace every non-reset SGR escape in the original line with the moved
        // color, preserving the line's sign/body span structure (e.g. the
        // whitespace-error sign split on `+` lines: `<col>+<reset><col>body<reset>`).
        out.push_str(&recolor_sgr(cline, color, &colors.reset));
    }
    out
}

/// Replace every non-reset SGR escape (`ESC [ ... m`) in `line` with `color`,
/// leaving reset escapes (`ESC [ m` / `ESC [ 0 m`) and the text untouched.
fn recolor_sgr(line: &str, color: &str, reset: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len() + color.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' {
                j += 1;
            }
            let end = if j < bytes.len() { j + 1 } else { j };
            let seq = &line[i..end];
            // Reset sequences are `\x1b[m` or `\x1b[0m`.
            if seq == "\x1b[m" || seq == "\x1b[0m" {
                out.push_str(reset);
            } else if seq == BG_RED {
                // Whitespace-error highlight (`\x1b[41m`) is layered on top of the
                // diff color; Git keeps it on moved lines too, so preserve it.
                out.push_str(BG_RED);
            } else {
                out.push_str(color);
            }
            i = end;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Resolve the effective `--color-moved` rendering context, or `None` when move
/// detection is off (not requested, mode `no`, or color disabled).
///
/// Validates the mode and `--color-moved-ws` value, exiting with Git's exact
/// error messages on bad input (t4015 #132, #133). Precedence: CLI flag over
/// `diff.colorMoved` / `diff.colorMovedWS` config.
fn resolve_color_moved(
    args: &Args,
    config: &ConfigSet,
    use_color: bool,
    ws_mode: &WhitespaceMode,
) -> Option<(grit_lib::diff_moved::ColorMovedMode, u32, MovedColors)> {
    use grit_lib::diff_moved::{parse_color_moved_ws, ColorMovedMode, MOVED_WS_ERROR};

    // --- mode --- (Git validates `--color-moved` / `diff.colorMoved` whenever
    // present, independently of whether color is on.)
    let (mode_str, mode_from_cmdline_cfg) = if let Some(m) = args.color_moved.as_deref() {
        (Some(m.to_owned()), false)
    } else if let Some(entry) = config.get_last_entry("diff.colormoved") {
        (
            Some(entry.value.clone().unwrap_or_default()),
            entry.scope == grit_lib::config::ConfigScope::Command,
        )
    } else {
        (None, false)
    };

    let mode = match &mode_str {
        Some(s) => match ColorMovedMode::parse(s) {
            Some(m) => m,
            None => {
                // Git prints the detailed `error:` line in all cases (t4015 #132).
                eprintln!(
                    "error: color moved setting must be one of 'no', 'default', 'blocks', 'zebra', 'dimmed-zebra', 'plain'"
                );
                if mode_from_cmdline_cfg {
                    // From `git -c diff.colorMoved=...`: a fatal config-parse error (exit 128).
                    eprintln!("fatal: unable to parse 'diff.colormoved' from command-line config");
                    std::process::exit(128);
                } else {
                    // From the `--color-moved=<arg>` flag: a usage error (exit 129).
                    eprintln!("error: bad --color-moved argument: {s}");
                    std::process::exit(129);
                }
            }
        },
        // No mode requested → move detection is off, but `--color-moved-ws` is
        // still validated below (Git errors on a bogus ws value regardless).
        None => ColorMovedMode::No,
    };

    // --- ws handling --- (validated whenever present, independent of mode)
    let (ws_str, ws_from_cmdline_cfg) = if let Some(w) = args.color_moved_ws.as_deref() {
        (Some(w.to_owned()), false)
    } else if let Some(entry) = config.get_last_entry("diff.colormovedws") {
        (
            Some(entry.value.clone().unwrap_or_default()),
            entry.scope == grit_lib::config::ConfigScope::Command,
        )
    } else {
        (None, false)
    };

    let mut ws_flags = 0u32;
    if let Some(ws_str) = ws_str {
        if ws_str == "no" {
            ws_flags = 0;
        } else {
            ws_flags = parse_color_moved_ws(&ws_str);
            if ws_flags & MOVED_WS_ERROR != 0 {
                // Detailed `error:` line first (always), mirroring Git's
                // parse_color_moved_ws: the allow-indentation-change
                // incompatibility takes precedence over an unknown-mode token.
                let has_allow = ws_str
                    .split(',')
                    .any(|t| t.trim() == "allow-indentation-change");
                let has_other_ws = ws_str.split(',').any(|t| {
                    matches!(
                        t.trim(),
                        "ignore-space-change" | "ignore-space-at-eol" | "ignore-all-space"
                    )
                });
                if has_allow && has_other_ws {
                    eprintln!(
                        "error: color-moved-ws: allow-indentation-change cannot be combined with other whitespace modes"
                    );
                } else {
                    let bad = ws_str
                        .split(',')
                        .map(str::trim)
                        .find(|t| {
                            !matches!(
                                *t,
                                "no" | "ignore-space-change"
                                    | "ignore-space-at-eol"
                                    | "ignore-all-space"
                                    | "allow-indentation-change"
                                    | ""
                            )
                        })
                        .unwrap_or("");
                    eprintln!(
                        "error: unknown color-moved-ws mode '{bad}', possible values are 'ignore-space-change', 'ignore-space-at-eol', 'ignore-all-space', 'allow-indentation-change'"
                    );
                }
                if ws_from_cmdline_cfg {
                    eprintln!(
                        "fatal: unable to parse 'diff.colormovedws' from command-line config"
                    );
                    std::process::exit(128);
                } else {
                    eprintln!("error: invalid mode '{ws_str}' in --color-moved-ws");
                    std::process::exit(129);
                }
            }
        }
    }

    if mode == ColorMovedMode::No || !use_color {
        return None;
    }

    // `-w`/`-b`/etc. already normalise the diff content; the move-detection ws
    // flags add on top of that. (Git applies both independently.)
    let _ = ws_mode;

    Some((mode, ws_flags, MovedColors::load(config)))
}

/// Emit a colored `@@ ... @@[ funcname]` hunk header the way Git does: the
/// `@@ -a,b +c,d @@` range is painted with the fraginfo (cyan) color, and a
/// trailing function-context name is painted separately with `ctx_color`
/// (Git's context color, empty by default → `<RESET>name<RESET>`).
fn write_colored_hunk_header(out: &mut impl Write, line: &str, ctx_color: &str) -> Result<()> {
    // Find the closing `@@` of the range. The header is `@@ <ranges> @@`,
    // optionally followed by ` <funcname>`.
    if let Some(close) = line.get(2..).and_then(|s| s.find("@@")).map(|i| i + 2) {
        let range_end = close + 2; // index just past the second `@@`
        let range = &line[..range_end];
        let rest = &line[range_end..]; // may be "" or " funcname"
        if let Some(func) = rest.strip_prefix(' ') {
            if func.is_empty() {
                writeln!(out, "{CYAN}{range}{RESET} ")?;
            } else {
                // Git paints the funcname with the (empty by default) context
                // color, which still emits a leading reset: `<RESET> <RESET>name<RESET>`.
                let func_color = if ctx_color.is_empty() {
                    RESET
                } else {
                    ctx_color
                };
                writeln!(out, "{CYAN}{range}{RESET} {func_color}{func}{RESET}")?;
            }
        } else {
            // No funcname (rest should be empty).
            writeln!(out, "{CYAN}{range}{rest}{RESET}")?;
        }
    } else {
        writeln!(out, "{CYAN}{line}{RESET}")?;
    }
    Ok(())
}

fn write_colored_patch(
    out: &mut impl Write,
    patch: &str,
    suppress_incomplete_red_after_plus: bool,
) -> Result<()> {
    write_colored_patch_ws(out, patch, suppress_incomplete_red_after_plus, None)
}

fn write_colored_patch_ws(
    out: &mut impl Write,
    patch: &str,
    suppress_incomplete_red_after_plus: bool,
    ws: Option<&WsColorCtx>,
) -> Result<()> {
    let mut last_was_plus_hunk_line = false;
    // Running pre/post-image line numbers (Git starts both at 1), for blank-at-eof detection.
    let mut lno_pre: usize = 1;
    let mut lno_post: usize = 1;
    // The whitespace-error highlight bit for the last content line, for the `\ No newline` marker.
    let mut last_line_kind: u8 = 0; // 0 = none/context, 1 = '+', 2 = '-'
    for line in patch.split_inclusive('\n') {
        let line_no_nl = line.strip_suffix('\n').unwrap_or(line);
        let nl = if line.ends_with('\n') { "\n" } else { "" };
        if line_no_nl.starts_with('\\') {
            // `\ No newline at end of file` marker.
            if let Some(ctx) = ws {
                let kind_bit = match last_line_kind {
                    1 => WSEH_NEW,
                    2 => WSEH_OLD,
                    _ => WSEH_CONTEXT,
                };
                let highlight_incomplete = (ctx.ws_rule & WS_INCOMPLETE_LINE) != 0
                    && (kind_bit & ctx.highlight) != 0
                    && !ctx.ws_color.is_empty();
                let set = if highlight_incomplete {
                    ctx.ws_color.as_str()
                } else {
                    ctx.context_color.as_str()
                };
                write!(out, "{set}{line_no_nl}{}{nl}", ctx.reset)?;
            } else if last_was_plus_hunk_line && !suppress_incomplete_red_after_plus {
                write!(out, "{BG_RED}{line_no_nl}{RESET}{nl}")?;
            } else {
                write!(out, "{CYAN}{line_no_nl}{RESET}{nl}")?;
            }
            continue;
        }
        if line.starts_with("---") || line.starts_with("+++") {
            last_was_plus_hunk_line = false;
            last_line_kind = 0;
            writeln!(out, "{BOLD}{line_no_nl}{RESET}")?;
        } else if line.starts_with("@@") {
            last_was_plus_hunk_line = false;
            last_line_kind = 0;
            // Git colors only the `@@ ... @@` range with FRAGINFO (cyan); a
            // trailing funcname is emitted separately with the (empty) context
            // color: `<CYAN>@@ ... @@<RESET> <RESET>func<RESET>`.
            write_colored_hunk_header(out, line_no_nl, "")?;
        } else if let Some(ctx) = ws {
            // ws-aware content line emission.
            let mut buf = String::new();
            if let Some(rest) = line.strip_prefix('-') {
                last_was_plus_hunk_line = false;
                last_line_kind = 2;
                lno_pre += 1;
                let hi = (ctx.highlight & WSEH_OLD) != 0 && !ctx.ws_color.is_empty();
                emit_ws_markup_line(
                    &mut buf,
                    '-',
                    rest,
                    &ctx.old_color,
                    &ctx.reset,
                    if hi {
                        Some(ctx.ws_color.as_str())
                    } else {
                        None
                    },
                    ctx.ws_rule,
                    false,
                );
            } else if let Some(rest) = line.strip_prefix('+') {
                last_was_plus_hunk_line = true;
                last_line_kind = 1;
                lno_post += 1;
                let blank_at_eof = ctx.blank_at_eof_pre != 0
                    && ctx.blank_at_eof_post != 0
                    && ctx.blank_at_eof_pre <= lno_pre
                    && ctx.blank_at_eof_post <= lno_post
                    && ws::ws_blank_line(rest.strip_suffix('\n').unwrap_or(rest));
                let hi = (ctx.highlight & WSEH_NEW) != 0 && !ctx.ws_color.is_empty();
                emit_ws_markup_line(
                    &mut buf,
                    '+',
                    rest,
                    &ctx.new_color,
                    &ctx.reset,
                    if hi {
                        Some(ctx.ws_color.as_str())
                    } else {
                        None
                    },
                    ctx.ws_rule,
                    blank_at_eof,
                );
            } else {
                last_was_plus_hunk_line = false;
                last_line_kind = 0;
                lno_pre += 1;
                lno_post += 1;
                let hi = (ctx.highlight & WSEH_CONTEXT) != 0 && !ctx.ws_color.is_empty();
                // Context lines: strip leading space, emit with context color.
                let rest = line.strip_prefix(' ').unwrap_or(line);
                emit_ws_markup_line(
                    &mut buf,
                    ' ',
                    rest,
                    &ctx.context_color,
                    &ctx.reset,
                    if hi {
                        Some(ctx.ws_color.as_str())
                    } else {
                        None
                    },
                    ctx.ws_rule,
                    false,
                );
            }
            out.write_all(buf.as_bytes())?;
        } else if line.starts_with('-') {
            last_was_plus_hunk_line = false;
            last_line_kind = 2;
            writeln!(out, "{RED}{line_no_nl}{RESET}")?;
        } else if line.starts_with('+') {
            last_was_plus_hunk_line = true;
            last_line_kind = 1;
            writeln!(out, "{GREEN}{line_no_nl}{RESET}")?;
        } else {
            last_was_plus_hunk_line = false;
            last_line_kind = 0;
            writeln!(out, "{line_no_nl}{RESET}")?;
        }
    }
    Ok(())
}

/// Next word span from `pos`, matching Git `find_word_boundaries` (`diff.c`).
/// Word-diff tokenizer regex.
///
/// Git compiles the word-diff `wordRegex` with POSIX `regcomp`/`regexec`
/// (`REG_EXTENDED`), which yields *leftmost-longest* match semantics: at the
/// earliest start position, the longest possible match wins regardless of the
/// order of alternatives. The `regex` crate is leftmost-*first* (Perl-style
/// preference order), so for a pattern like cpp's
/// `[a-zA-Z_]...|[0-9][0-9.]*...|0[xXbB]...` it would match just `0` of `0xdead`
/// (second alternative) instead of the whole `0xdead` (third alternative).
///
/// We reproduce this with two engines: the `regex` crate (leftmost-first) to
/// locate the earliest match *start*, then a `regex-automata` dense DFA built
/// with `MatchKind::All`, searched *anchored* at that start, to take the longest
/// match there. (An unanchored `MatchKind::All` search does not give
/// leftmost-longest, so anchoring at the leftmost start is required.)
pub struct WordRe {
    start: Regex,
    longest: regex_automata::dfa::regex::Regex,
}

/// Translate a POSIX ERE (as used by Git's `wordRegex`) into a pattern the `regex` crate accepts.
///
/// POSIX bracket expressions allow `]` as the first member of a class (e.g. `[][)(]` or the
/// scheme driver's `[^][)(}{[ \t]`), but the `regex` crate requires it to be escaped. We rewrite
/// a `]` that immediately follows `[` or `[^` into `\]`. Backslash escapes are passed through so
/// existing `\]`, `\[`, etc. are untouched.
fn posix_ere_to_rust_regex(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len() + 4);
    let mut i = 0usize;
    let mut in_class = false;
    // Position (in `out`-equivalent terms) tracking where the current class body began, so we can
    // detect the "first member" slot (just after `[` or `[^`).
    let mut class_just_opened = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            // Pass through an escape pair verbatim.
            out.push('\\');
            out.push(bytes[i + 1] as char);
            i += 2;
            class_just_opened = false;
            continue;
        }
        if !in_class {
            if c == b'[' {
                in_class = true;
                class_just_opened = true;
                out.push('[');
                i += 1;
                // Handle a leading negation.
                if i < bytes.len() && bytes[i] == b'^' {
                    out.push('^');
                    i += 1;
                }
                continue;
            }
            out.push(c as char);
            i += 1;
            continue;
        }
        // Inside a character class.
        if class_just_opened && c == b']' {
            // `]` as the first member is a literal in POSIX; escape it for the regex crate.
            out.push_str("\\]");
            i += 1;
            class_just_opened = false;
            continue;
        }
        class_just_opened = false;
        if c == b']' {
            in_class = false;
            out.push(']');
            i += 1;
            continue;
        }
        if c == b'[' {
            // A POSIX bracket expression (`[:alpha:]`, `[.ch.]`, `[=a=]`) is passed through;
            // any other `[` is a literal member, which the `regex` crate would otherwise treat
            // as the start of a nested class, so escape it.
            if i + 1 < bytes.len() && matches!(bytes[i + 1], b':' | b'.' | b'=') {
                out.push('[');
            } else {
                out.push_str("\\[");
            }
            i += 1;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

impl WordRe {
    fn build(pattern: &str, case_insensitive: bool) -> Option<Self> {
        use regex_automata::{
            dfa::{dense, regex::Regex as DfaRegex},
            util::syntax,
            MatchKind,
        };
        let pattern = &posix_ere_to_rust_regex(pattern);
        let start = RegexBuilder::new(pattern)
            .multi_line(true)
            .case_insensitive(case_insensitive)
            .build()
            .ok()?;
        let longest = DfaRegex::builder()
            .syntax(
                syntax::Config::new()
                    .multi_line(true)
                    .case_insensitive(case_insensitive),
            )
            .dense(dense::Config::new().match_kind(MatchKind::All))
            .build(pattern)
            .ok()?;
        Some(WordRe { start, longest })
    }

    /// Leftmost-longest match at or after `pos`. Returns the byte range.
    fn find_at(&self, text: &str, pos: usize) -> Option<(usize, usize)> {
        use regex_automata::{Anchored, Input};
        let m = self.start.find_at(text, pos)?;
        let begin = m.start();
        // Take the longest match anchored at the leftmost start (POSIX semantics).
        let input = Input::new(text)
            .span(begin..text.len())
            .anchored(Anchored::Yes);
        match self.longest.find(input) {
            Some(lm) => Some((lm.start(), lm.end())),
            // Fall back to the leftmost-first match if the DFA search fails.
            None => Some((begin, m.end())),
        }
    }
}

fn next_git_word(text: &str, mut pos: usize, word_re: Option<&WordRe>) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    // git find_word_boundaries: when a word_regex is configured, words are *only* the
    // regex matches. If no further match is found, there is no word (return -1) — we do
    // NOT fall back to whitespace splitting.
    if let Some(re) = word_re {
        while pos < text.len() {
            let Some((m_start, m_end)) = re.find_at(text, pos) else {
                return None;
            };
            let begin = m_start;
            // git truncates the match at the first embedded newline.
            let matched = text.get(begin..m_end)?;
            let rel_nl = matched.find('\n');
            let end = rel_nl.map(|p| begin + p).unwrap_or(m_end);
            if begin == end {
                // empty match: advance one byte and retry.
                pos = begin.saturating_add(1);
                continue;
            }
            return Some((begin, end));
        }
        return None;
    }
    // No regex: whitespace-delimited words.
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if pos >= bytes.len() {
        return None;
    }
    let mut end = pos + 1;
    while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    Some((pos, end))
}

fn git_word_spans(text: &str, word_re: Option<&WordRe>) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        let Some((a, b)) = next_git_word(text, i, word_re) else {
            break;
        };
        spans.push((a, b));
        i = b;
    }
    spans
}

fn word_lines_from_spans(text: &str, spans: &[(usize, usize)]) -> String {
    let mut s = String::new();
    for (a, b) in spans {
        s.push_str(text.get(*a..*b).unwrap_or(""));
        s.push('\n');
    }
    s
}

/// A single output style for word-diff, matching git's `diff_words_style`.
struct GitWordStyle {
    new_prefix: &'static str,
    new_suffix: &'static str,
    new_color: String,
    old_prefix: &'static str,
    old_suffix: &'static str,
    old_color: String,
    ctx_prefix: &'static str,
    ctx_suffix: &'static str,
    ctx_color: String,
    newline: &'static str,
}

struct GitStyleElem<'a> {
    prefix: &'a str,
    suffix: &'a str,
    color: &'a str,
}

/// Faithful port of git's `fn_out_diff_words_write_helper` (diff.c).
///
/// Emits `buf` (a sub-slice of bytes) using the given style element, splitting on
/// newlines and inserting `newline` (e.g. `\n`, `~\n`) between physical lines.
fn git_words_write_helper(out: &mut String, st: &GitStyleElem, newline: &str, buf: &[u8]) {
    let mut buf = buf;
    let mut print = false;
    loop {
        // No graph/line prefix in these contexts; `print` only added the line prefix.
        let _ = print;
        let nl_pos = buf.iter().position(|&c| c == b'\n');
        let seg_end = nl_pos.unwrap_or(buf.len());
        if seg_end != 0 {
            let has_color = !st.color.is_empty();
            if has_color {
                out.push_str(st.color);
            }
            out.push_str(st.prefix);
            if let Ok(s) = std::str::from_utf8(&buf[..seg_end]) {
                out.push_str(s);
            }
            out.push_str(st.suffix);
            if has_color {
                out.push_str(RESET);
            }
        }
        match nl_pos {
            None => break,
            Some(p) => {
                out.push_str(newline);
                buf = &buf[p + 1..];
                print = true;
                if buf.is_empty() {
                    break;
                }
            }
        }
    }
}

/// Faithful port of git's `diff_words_show` / `fn_out_diff_words_aux` (diff.c).
///
/// `minus`/`plus` are the accumulated raw line bodies for one diff hunk (each physical
/// line terminated by `\n`). Produces the word-diff rendering for the configured style.
fn git_word_diff_show(
    out: &mut String,
    minus: &str,
    plus: &str,
    word_re: Option<&WordRe>,
    style: &GitWordStyle,
) {
    let minus_b = minus.as_bytes();
    let plus_b = plus.as_bytes();

    let new_el = GitStyleElem {
        prefix: style.new_prefix,
        suffix: style.new_suffix,
        color: &style.new_color,
    };
    let old_el = GitStyleElem {
        prefix: style.old_prefix,
        suffix: style.old_suffix,
        color: &style.old_color,
    };
    let ctx_el = GitStyleElem {
        prefix: style.ctx_prefix,
        suffix: style.ctx_suffix,
        color: &style.ctx_color,
    };

    // Special case: only removal (no plus text at all).
    if plus_b.is_empty() {
        git_words_write_helper(out, &old_el, style.newline, minus_b);
        return;
    }

    // Tokenize each side into word spans (git diff_words_fill).
    let minus_spans = git_word_spans(minus, word_re);
    let plus_spans = git_word_spans(plus, word_re);

    // Build the per-word "lines" buffers and diff them (xdiff, ctxlen=0).
    let mw: Vec<&str> = minus_spans
        .iter()
        .map(|(a, b)| minus.get(*a..*b).unwrap_or(""))
        .collect();
    let pw: Vec<&str> = plus_spans
        .iter()
        .map(|(a, b)| plus.get(*a..*b).unwrap_or(""))
        .collect();

    // Diff the word streams with imara's Myers (Git's default xdiff engine) followed by
    // `xdl_change_compact`, matching Git's word-level alignment and tie-breaking.
    let ops = word_diff_ops_imara(&mw, &pw);

    // current_plus tracks the byte offset into `plus` already emitted.
    let mut current_plus: usize = 0;

    // Walk the diff ops, grouping consecutive replace/insert/delete into single
    // callbacks like xdiff hunks. We emulate fn_out_diff_words_aux per changed region.
    use similar::DiffTag;
    let mut idx = 0usize;
    while idx < ops.len() {
        let op = ops[idx];
        let (tag, _o_range, _n_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            idx += 1;
            continue;
        }
        // Coalesce a maximal run of non-equal ops into one hunk.
        // minus_first/plus_first are the word indices at the hunk boundary in each
        // stream (matching git's xdiff hunk start), used for the zero-length case.
        let (_, first_o, first_n) = ops[idx].as_tag_tuple();
        let minus_first = first_o.start;
        let plus_first = first_n.start;
        let mut minus_count = 0usize;
        let mut plus_count = 0usize;
        let mut m_lo = usize::MAX;
        let mut m_hi = 0usize;
        let mut p_lo = usize::MAX;
        let mut p_hi = 0usize;
        while idx < ops.len() {
            let (t, orange, nrange) = ops[idx].as_tag_tuple();
            if t == DiffTag::Equal {
                break;
            }
            if orange.start < orange.end {
                m_lo = m_lo.min(orange.start);
                m_hi = m_hi.max(orange.end);
                minus_count += orange.end - orange.start;
            }
            if nrange.start < nrange.end {
                p_lo = p_lo.min(nrange.start);
                p_hi = p_hi.max(nrange.end);
                plus_count += nrange.end - nrange.start;
            }
            idx += 1;
        }

        // Compute byte ranges into minus/plus, mirroring fn_out_diff_words_aux.
        // For len>0: begin = orig[first].begin, end = orig[first+len-1].end.
        // For len==0: begin = end = orig[first].end (end of word before boundary).
        let (minus_begin, minus_end) = if minus_count > 0 {
            (
                minus_spans.get(m_lo).map(|s| s.0).unwrap_or(minus.len()),
                minus_spans
                    .get(m_hi - 1)
                    .map(|s| s.1)
                    .unwrap_or(minus.len()),
            )
        } else {
            let mb = word_end_at_orig(&minus_spans, minus_first);
            (mb, mb)
        };
        let (plus_begin, plus_end) = if plus_count > 0 {
            (
                plus_spans.get(p_lo).map(|s| s.0).unwrap_or(plus.len()),
                plus_spans.get(p_hi - 1).map(|s| s.1).unwrap_or(plus.len()),
            )
        } else {
            let pe = word_end_at_orig(&plus_spans, plus_first);
            (pe, pe)
        };

        // Emit context = plus[current_plus .. plus_begin].
        if current_plus != plus_begin && plus_begin <= plus.len() && current_plus <= plus_begin {
            git_words_write_helper(
                out,
                &ctx_el,
                style.newline,
                &plus_b[current_plus..plus_begin],
            );
        }
        if minus_begin != minus_end {
            git_words_write_helper(
                out,
                &old_el,
                style.newline,
                &minus_b[minus_begin..minus_end],
            );
        }
        if plus_begin != plus_end {
            git_words_write_helper(out, &new_el, style.newline, &plus_b[plus_begin..plus_end]);
        }
        current_plus = plus_end;
    }

    // Trailing context.
    if current_plus != plus_b.len() && current_plus <= plus_b.len() {
        git_words_write_helper(out, &ctx_el, style.newline, &plus_b[current_plus..]);
    }
}

/// Byte offset of `orig[k].end` for a zero-length word range at xdiff word position `k`.
///
/// git's `orig` array has a fake 0th word at offset 0, then real words at index 1..=nr.
/// A word-stream position `k` (0-based, as similar reports) corresponds to the boundary
/// *after* the previous emitted word: that is `spans[k-1].end`, or 0 at the start.
fn word_end_at_orig(spans: &[(usize, usize)], k: usize) -> usize {
    if k == 0 {
        0
    } else {
        spans
            .get(k - 1)
            .map(|s| s.1)
            .unwrap_or_else(|| spans.last().map(|s| s.1).unwrap_or(0))
    }
}

/// Strip one leading tab from a unified-diff context body for `--word-diff=porcelain`.
///
/// The harness expectations in `t4034-diff-words.sh` use `<<-EOF` lines with a single leading
/// space and file text without the tab that appears after the diff ` ` prefix in unified input.
fn porcelain_context_body(body: &str) -> &str {
    body.strip_prefix('\t').unwrap_or(body)
}

fn emit_plus_context_lines(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut start = 0usize;
    let mut pending_empty = false;
    for (idx, _) in text.match_indices('\n') {
        let raw = text.get(start..idx).unwrap_or("");
        let only_newlines = !raw.is_empty() && raw.chars().all(|c| c == '\n');
        if raw.is_empty() || only_newlines {
            if !pending_empty {
                pending_empty = true;
            }
        } else {
            if pending_empty {
                out.push(' ');
                out.push('\n');
                pending_empty = false;
            }
            out.push(' ');
            out.push_str(porcelain_context_body(raw));
            out.push('\n');
        }
        start = idx + 1;
    }
    let tail_raw = text.get(start..).unwrap_or("");
    if pending_empty {
        // A gap like `\n\t` is an empty `+` line followed by a line whose only prefix is
        // whitespace; Git emits one context row ` \t\n`, not ` \n` then ` \t\n`.
        if !tail_raw.is_empty()
            && !tail_raw.contains('\n')
            && tail_raw.chars().all(|c| matches!(c, ' ' | '\t'))
        {
            out.push(' ');
            out.push_str(tail_raw);
            out.push('\n');
        } else {
            out.push(' ');
            out.push('\n');
            if !tail_raw.is_empty() {
                out.push(' ');
                out.push_str(porcelain_context_body(tail_raw));
                out.push('\n');
            }
        }
    } else if !tail_raw.is_empty() {
        out.push(' ');
        out.push_str(porcelain_context_body(tail_raw));
        out.push('\n');
    }
}

fn emit_plus_context_lines_porcelain(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut start = 0usize;
    let mut pending_empty = false;
    for (idx, _) in text.match_indices('\n') {
        let raw = text.get(start..idx).unwrap_or("");
        let only_newlines = !raw.is_empty() && raw.chars().all(|c| c == '\n');
        if raw.is_empty() || only_newlines {
            if !pending_empty {
                pending_empty = true;
            }
        } else {
            if pending_empty {
                out.push(' ');
                out.push('\n');
                out.push_str("~\n");
                pending_empty = false;
            }
            out.push(' ');
            out.push_str(porcelain_context_body(raw));
            out.push('\n');
            out.push_str("~\n");
        }
        start = idx + 1;
    }
    let tail_raw = text.get(start..).unwrap_or("");
    if pending_empty {
        if !tail_raw.is_empty()
            && !tail_raw.contains('\n')
            && tail_raw.chars().all(|c| matches!(c, ' ' | '\t'))
        {
            out.push(' ');
            out.push_str(tail_raw);
            out.push('\n');
            out.push_str("~\n");
        } else {
            out.push(' ');
            out.push('\n');
            out.push_str("~\n");
            if !tail_raw.is_empty() {
                out.push(' ');
                out.push_str(porcelain_context_body(tail_raw));
                out.push('\n');
                out.push_str("~\n");
            }
        }
    } else if !tail_raw.is_empty() {
        out.push(' ');
        out.push_str(porcelain_context_body(tail_raw));
        out.push('\n');
        out.push_str("~\n");
    }
}

fn emit_minus_raw_lines(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut start = 0usize;
    for (idx, _) in text.match_indices('\n') {
        let body = text.get(start..idx).unwrap_or("");
        out.push('-');
        out.push_str(body);
        out.push('\n');
        start = idx + 1;
    }
    let tail = text.get(start..).unwrap_or("");
    if !tail.is_empty() {
        out.push('-');
        out.push_str(tail);
        out.push('\n');
    }
}

fn emit_plus_raw_lines(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut start = 0usize;
    for (idx, _) in text.match_indices('\n') {
        let body = text.get(start..idx).unwrap_or("");
        out.push('+');
        out.push_str(body);
        out.push('\n');
        start = idx + 1;
    }
    let tail = text.get(start..).unwrap_or("");
    if !tail.is_empty() {
        out.push('+');
        out.push_str(tail);
        out.push('\n');
    }
}

fn emit_minus_raw_lines_porcelain(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut start = 0usize;
    for (idx, _) in text.match_indices('\n') {
        let body = porcelain_context_body(text.get(start..idx).unwrap_or(""));
        out.push('-');
        out.push_str(body);
        out.push('\n');
        start = idx + 1;
    }
    let tail = porcelain_context_body(text.get(start..).unwrap_or(""));
    if !tail.is_empty() {
        out.push('-');
        out.push_str(tail);
        out.push('\n');
    }
}

fn emit_plus_raw_lines_porcelain(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut start = 0usize;
    for (idx, _) in text.match_indices('\n') {
        let body = porcelain_context_body(text.get(start..idx).unwrap_or(""));
        out.push('+');
        out.push_str(body);
        out.push('\n');
        start = idx + 1;
    }
    let tail = porcelain_context_body(text.get(start..).unwrap_or(""));
    if !tail.is_empty() {
        out.push('+');
        out.push_str(tail);
        out.push('\n');
    }
}

fn word_diff_emit_context_line_plain(
    out: &mut String,
    cfg: &ConfigSet,
    use_color: bool,
    line_without_prefix: &str,
) {
    let ctx_c = ansi_diff_color(cfg, "diff.color.context", "");
    let reset = if use_color { RESET } else { "" };
    if use_color && !ctx_c.is_empty() {
        out.push_str(&ctx_c);
        out.push_str(line_without_prefix);
        out.push_str(reset);
        out.push('\n');
        return;
    }
    if use_color && ctx_c.is_empty() {
        out.push_str(line_without_prefix);
        if line_without_prefix.chars().any(|c| !c.is_whitespace()) {
            out.push_str(reset);
        }
        out.push('\n');
        return;
    }
    out.push_str(line_without_prefix);
    out.push('\n');
}

#[derive(Clone, Copy)]
enum WordDiffBodyStyle {
    Color,
    Porcelain,
    PlainMarkers,
}

fn word_diff_emit_body(
    out: &mut String,
    minus: &str,
    plus: &str,
    minus_spans: &[(usize, usize)],
    plus_spans: &[(usize, usize)],
    style: WordDiffBodyStyle,
    cfg: &ConfigSet,
    use_color: bool,
) {
    use similar::{ChangeTag, TextDiff};

    let mw: Vec<&str> = minus_spans
        .iter()
        .map(|(a, b)| minus.get(*a..*b).unwrap_or(""))
        .collect();
    let pw: Vec<&str> = plus_spans
        .iter()
        .map(|(a, b)| plus.get(*a..*b).unwrap_or(""))
        .collect();

    if pw.is_empty() && !mw.is_empty() {
        match style {
            WordDiffBodyStyle::Color => {
                let old_c = ansi_diff_color(cfg, "diff.color.old", RED);
                let reset = if use_color { RESET } else { "" };
                for (a, b) in minus_spans {
                    let piece = minus.get(*a..*b).unwrap_or("");
                    if use_color && !old_c.is_empty() {
                        out.push_str(&old_c);
                        out.push_str(piece);
                        out.push_str(reset);
                    } else {
                        out.push_str(piece);
                    }
                }
            }
            WordDiffBodyStyle::Porcelain => {
                for (a, b) in minus_spans {
                    emit_minus_raw_lines_porcelain(out, minus.get(*a..*b).unwrap_or(""));
                    out.push_str("~\n");
                }
            }
            WordDiffBodyStyle::PlainMarkers => {
                for (a, b) in minus_spans {
                    let piece = minus.get(*a..*b).unwrap_or("");
                    out.push_str(&format!("[-{piece}-]"));
                }
            }
        }
        return;
    }

    let diff = TextDiff::from_slices(&mw, &pw);
    let old_c = ansi_diff_color(cfg, "diff.color.old", RED);
    let new_c = ansi_diff_color(cfg, "diff.color.new", GREEN);
    let reset = if use_color { RESET } else { "" };

    let porcelain_saw_word_line = Cell::new(false);
    let emit_gap = |out: &mut String, gap: &str| {
        if gap.is_empty() {
            return;
        }
        match style {
            WordDiffBodyStyle::Color => word_diff_emit_gap_color(out, cfg, use_color, gap),
            WordDiffBodyStyle::Porcelain => {
                if !porcelain_saw_word_line.get() && gap.chars().all(|c| c.is_whitespace()) {
                    // Harness `t4034` omits leading all-whitespace plus-side gaps before the first
                    // `-`/`+` pair (e.g. the indent line before `h(4)`).
                    return;
                }
                if porcelain_saw_word_line.get() {
                    // Line breaks before the next `-`/`+` chunk: Git prints one `~\n` per newline in
                    // the plus-side file. Trailing spaces/tabs after those newlines belong to the
                    // next added line (merged into the following `+` line), not ` \t\n~\n` context.
                    let bytes = gap.as_bytes();
                    let mut nl_run = 0usize;
                    while nl_run < bytes.len() && bytes[nl_run] == b'\n' {
                        nl_run += 1;
                    }
                    if nl_run > 0 {
                        let after = gap.get(nl_run..).unwrap_or("");
                        if after.is_empty() || after.chars().all(|c| matches!(c, ' ' | '\t')) {
                            let count = if out.ends_with("~\n") {
                                nl_run.saturating_sub(1)
                            } else {
                                nl_run
                            };
                            for _ in 0..count {
                                out.push_str("~\n");
                            }
                            return;
                        }
                    }
                    emit_plus_context_lines_porcelain(out, gap);
                } else {
                    emit_plus_context_lines(out, gap);
                }
            }
            WordDiffBodyStyle::PlainMarkers => out.push_str(gap),
        }
    };

    let mut cur_plus = 0usize;
    let mut pi = 0usize;
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let mut i = 0usize;
    let mut first_insert_merged_emit = true;
    while i < changes.len() {
        match changes[i].tag() {
            ChangeTag::Equal => {
                let mut j = i;
                let mut first_a = None;
                let mut last_b = None;
                let mut k = pi;
                while j < changes.len() && changes[j].tag() == ChangeTag::Equal {
                    if let Some((a, b)) = plus_spans.get(k) {
                        first_a.get_or_insert(*a);
                        last_b = Some(*b);
                        k += 1;
                    }
                    j += 1;
                }
                if let (Some(_fa), Some(lb)) = (first_a, last_b) {
                    if cur_plus < lb {
                        emit_gap(out, plus.get(cur_plus..lb).unwrap_or(""));
                    }
                    cur_plus = lb;
                    pi = k;
                }
                i = j;
            }
            ChangeTag::Delete => {
                let del_piece = changes[i].value();
                if i + 1 < changes.len() && changes[i + 1].tag() == ChangeTag::Insert {
                    if let Some((pa, pb)) = plus_spans.get(pi) {
                        if *pa > cur_plus {
                            emit_gap(out, plus.get(cur_plus..*pa).unwrap_or(""));
                        }
                        let new_piece = plus.get(*pa..*pb).unwrap_or("");
                        match style {
                            WordDiffBodyStyle::Color => {
                                if use_color && !old_c.is_empty() {
                                    out.push_str(&old_c);
                                    out.push_str(del_piece);
                                    out.push_str(reset);
                                } else {
                                    out.push_str(del_piece);
                                }
                                if use_color && !new_c.is_empty() {
                                    out.push_str(&new_c);
                                    out.push_str(new_piece);
                                    out.push_str(reset);
                                } else {
                                    out.push_str(new_piece);
                                }
                            }
                            WordDiffBodyStyle::Porcelain => {
                                emit_minus_raw_lines_porcelain(out, del_piece);
                                emit_plus_raw_lines_porcelain(out, new_piece);
                                out.push_str("~\n");
                                porcelain_saw_word_line.set(true);
                            }
                            WordDiffBodyStyle::PlainMarkers => {
                                out.push_str(&format!("[-{del_piece}-]{{+{new_piece}+}}"));
                            }
                        }
                        cur_plus = *pb;
                        pi += 1;
                    }
                    i += 2;
                } else {
                    let mut j = i;
                    let mut del_text = String::new();
                    while j < changes.len() && changes[j].tag() == ChangeTag::Delete {
                        del_text.push_str(changes[j].value());
                        j += 1;
                    }
                    let ctx_end = plus_spans
                        .get(pi)
                        .map(|(start, _)| *start)
                        .unwrap_or(plus.len());
                    if ctx_end > cur_plus {
                        emit_gap(out, plus.get(cur_plus..ctx_end).unwrap_or(""));
                    }
                    match style {
                        WordDiffBodyStyle::Color => {
                            if use_color && !old_c.is_empty() {
                                out.push_str(&old_c);
                                out.push_str(&del_text);
                                out.push_str(reset);
                            } else {
                                out.push_str(&del_text);
                            }
                        }
                        WordDiffBodyStyle::Porcelain => {
                            emit_minus_raw_lines_porcelain(out, &del_text);
                            out.push_str("~\n");
                            porcelain_saw_word_line.set(true);
                        }
                        WordDiffBodyStyle::PlainMarkers => {
                            out.push_str(&format!("[-{del_text}-]"));
                        }
                    }
                    i = j;
                }
            }
            ChangeTag::Insert => {
                let mut j = i;
                let mut k = pi;
                let mut run_fa: Option<usize> = None;
                let mut run_lb: Option<usize> = None;

                let emit_merged = |out: &mut String,
                                   cur_plus: &mut usize,
                                   fa: usize,
                                   lb: usize,
                                   first_run: &mut bool| {
                    let line_start = plus[..fa].rfind('\n').map(|n| n + 1).unwrap_or(0);
                    let prefix = plus.get(line_start..fa).unwrap_or("");
                    let merge_fa = if *first_run {
                        fa
                    } else if !prefix.contains('\n') && prefix.chars().all(|c| c.is_whitespace()) {
                        line_start
                    } else {
                        fa
                    };
                    if merge_fa > *cur_plus {
                        emit_gap(out, plus.get(*cur_plus..merge_fa).unwrap_or(""));
                    }
                    let merged = plus.get(merge_fa..lb).unwrap_or("");
                    match style {
                        WordDiffBodyStyle::Color => {
                            if use_color && !new_c.is_empty() {
                                out.push_str(&new_c);
                                out.push_str(merged);
                                out.push_str(reset);
                            } else {
                                out.push_str(merged);
                            }
                        }
                        WordDiffBodyStyle::Porcelain => {
                            emit_plus_raw_lines_porcelain(out, merged);
                            out.push_str("~\n");
                            porcelain_saw_word_line.set(true);
                        }
                        WordDiffBodyStyle::PlainMarkers => {
                            out.push_str(&format!("{{+{merged}+}}"));
                        }
                    }
                    *cur_plus = lb;
                    *first_run = false;
                };

                while j < changes.len() && changes[j].tag() == ChangeTag::Insert {
                    let Some((a, b)) = plus_spans.get(k) else {
                        break;
                    };
                    if let (Some(prev_lb), Some(fa)) = (run_lb, run_fa) {
                        let between = plus.get(prev_lb..*a).unwrap_or("");
                        if between.contains('\n') {
                            emit_merged(
                                out,
                                &mut cur_plus,
                                fa,
                                prev_lb,
                                &mut first_insert_merged_emit,
                            );
                            run_fa = Some(*a);
                            run_lb = Some(*b);
                        } else {
                            run_lb = Some(*b);
                        }
                    } else {
                        run_fa = Some(*a);
                        run_lb = Some(*b);
                    }
                    j += 1;
                    k += 1;
                }
                if let (Some(fa), Some(lb)) = (run_fa, run_lb) {
                    emit_merged(out, &mut cur_plus, fa, lb, &mut first_insert_merged_emit);
                }
                pi = k;
                i = j;
            }
        }
    }
    if cur_plus < plus.len() {
        let tail = plus.get(cur_plus..).unwrap_or("");
        match style {
            WordDiffBodyStyle::Color => word_diff_emit_gap_color(out, cfg, use_color, tail),
            WordDiffBodyStyle::Porcelain => {
                if porcelain_saw_word_line.get() {
                    // Remaining `plus` bytes after the last word are only the line terminators for
                    // lines already emitted as `-`/`+` lines. Git prints the following blank/context
                    // via unified ` ` lines (` \n~\n`), not via another porcelain context emission
                    // here — doing so duplicates a trailing ` \n~\n` at EOF.
                    if !tail.is_empty() && tail.chars().all(|c| c == '\n') {
                        // skip
                    } else {
                        emit_plus_context_lines_porcelain(out, tail);
                    }
                } else {
                    emit_plus_context_lines(out, tail);
                }
            }
            WordDiffBodyStyle::PlainMarkers => out.push_str(tail),
        }
    }
}

fn word_diff_emit_gap_color(out: &mut String, cfg: &ConfigSet, use_color: bool, gap: &str) {
    if gap.is_empty() {
        return;
    }
    let ctx_c = ansi_diff_color(cfg, "diff.color.context", "");
    let reset = if use_color { RESET } else { "" };
    if use_color && !ctx_c.is_empty() {
        out.push_str(&ctx_c);
        out.push_str(gap);
        out.push_str(reset);
        return;
    }
    if use_color && ctx_c.is_empty() {
        let mut line_start = 0usize;
        for (idx, _) in gap.match_indices('\n') {
            let line = gap.get(line_start..idx).unwrap_or("");
            out.push_str(line);
            if line.chars().any(|c| !c.is_whitespace()) {
                out.push_str(reset);
            }
            out.push('\n');
            line_start = idx + 1;
        }
        let tail = gap.get(line_start..).unwrap_or("");
        out.push_str(tail);
        if !tail.is_empty() && tail.chars().any(|c| !c.is_whitespace()) {
            out.push_str(reset);
        }
        return;
    }
    out.push_str(gap);
}

fn word_diff_emit_plain_color(
    out: &mut String,
    minus: &str,
    plus: &str,
    minus_spans: &[(usize, usize)],
    plus_spans: &[(usize, usize)],
    cfg: &ConfigSet,
    use_color: bool,
) {
    word_diff_emit_body(
        out,
        minus,
        plus,
        minus_spans,
        plus_spans,
        WordDiffBodyStyle::Color,
        cfg,
        use_color,
    );
}

fn word_diff_emit_porcelain(
    out: &mut String,
    minus: &str,
    plus: &str,
    minus_spans: &[(usize, usize)],
    plus_spans: &[(usize, usize)],
) {
    let cfg = ConfigSet::new();
    word_diff_emit_body(
        out,
        minus,
        plus,
        minus_spans,
        plus_spans,
        WordDiffBodyStyle::Porcelain,
        &cfg,
        false,
    );
}

fn word_diff_emit_plain_markers(
    out: &mut String,
    minus: &str,
    plus: &str,
    word_re: Option<&WordRe>,
) {
    let ms = git_word_spans(minus, word_re);
    let ps = git_word_spans(plus, word_re);
    let cfg = ConfigSet::new();
    word_diff_emit_body(
        out,
        minus,
        plus,
        &ms,
        &ps,
        WordDiffBodyStyle::PlainMarkers,
        &cfg,
        false,
    );
}

fn word_diff_generate_patch(
    config: &ConfigSet,
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    wd: &EffectiveWordDiff,
    header_use_color: bool,
    word_re: Option<&WordRe>,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    let path_for_algo = if old_path == "/dev/null" {
        new_path
    } else {
        old_path
    };
    let (algo, use_git_histogram) = diff_algorithm_for_path(path_for_algo, algo_cli, algo_ctx);
    let func_matcher = matcher_for_path_parsed(
        algo_ctx.config.as_ref(),
        &algo_ctx.attrs.rules,
        &algo_ctx.attrs.macros,
        path_for_algo,
        algo_ctx.ignore_case_attrs,
    )
    .unwrap_or(None);

    let unified = unified_diff_with_prefix_and_funcname_and_algorithm(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        func_matcher.as_ref(),
        algo,
        false,
        use_git_histogram,
        indent_heuristic,
        quote_path_fully,
    );

    let mut out = String::new();
    let (hb, hr) = if header_use_color {
        (BOLD, RESET)
    } else {
        ("", "")
    };
    let (cb, cr) = if header_use_color && matches!(wd.mode, WordDiffModeCli::Color) {
        (CYAN, RESET)
    } else if header_use_color && wd.force_color && matches!(wd.mode, WordDiffModeCli::Plain) {
        (CYAN, RESET)
    } else {
        ("", "")
    };
    let (fb, fr) =
        if header_use_color && matches!(wd.mode, WordDiffModeCli::Color) && wd.force_color {
            (MAGENTA, RESET)
        } else {
            ("", "")
        };

    let lines: Vec<&str> = unified.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("--- ") {
            writeln!(out, "{hb}{line}{hr}").ok();
            i += 1;
            continue;
        }
        if line.starts_with("+++ ") {
            writeln!(out, "{hb}{line}{hr}").ok();
            i += 1;
            continue;
        }
        if line.starts_with("@@") {
            let header = line.to_owned();
            let func_line = if header_use_color && wd.force_color {
                color_hunk_header_word_diff(&header, cb, cr, fb, fr)
            } else {
                header.clone()
            };
            writeln!(out, "{func_line}").ok();
            i += 1;

            let mut minus_buf = String::new();
            let mut plus_buf = String::new();
            while i < lines.len() {
                let ln = lines[i];
                if ln.starts_with("@@") {
                    word_diff_flush_hunk(
                        &mut out,
                        config,
                        wd,
                        header_use_color,
                        word_re,
                        &minus_buf,
                        &plus_buf,
                    );
                    minus_buf.clear();
                    plus_buf.clear();
                    break;
                }
                if ln.starts_with('-') {
                    // Inside the hunk body the `--- a/file` header has already been
                    // consumed by the outer loop, so any `-` line is a deletion — even
                    // one whose content itself begins with `-` (e.g. `---a==--b`).
                    let body = ln.strip_prefix('-').unwrap_or(ln);
                    minus_buf.push_str(body);
                    let no_nl = lines.get(i + 1).is_some_and(|n| n.starts_with('\\'));
                    if !no_nl {
                        minus_buf.push('\n');
                    }
                    i += 1;
                } else if ln.starts_with('+') {
                    let body = ln.strip_prefix('+').unwrap_or(ln);
                    plus_buf.push_str(body);
                    let no_nl = lines.get(i + 1).is_some_and(|n| n.starts_with('\\'));
                    if !no_nl {
                        plus_buf.push('\n');
                    }
                    i += 1;
                } else if ln.starts_with('\\') {
                    i += 1;
                } else if let Some(rest) = ln.strip_prefix(' ') {
                    word_diff_flush_hunk(
                        &mut out,
                        config,
                        wd,
                        header_use_color,
                        word_re,
                        &minus_buf,
                        &plus_buf,
                    );
                    minus_buf.clear();
                    plus_buf.clear();
                    if matches!(wd.mode, WordDiffModeCli::Porcelain) {
                        // git DIFF_SYMBOL_WORDS_PORCELAIN: emit_line(context, line) then "~\n".
                        writeln!(out, " {rest}").ok();
                        out.push_str("~\n");
                    } else if wd.force_color && header_use_color {
                        // git DIFF_SYMBOL_WORDS: context line painted with diff context color.
                        word_diff_emit_context_line_plain(&mut out, config, header_use_color, rest);
                    } else {
                        writeln!(out, "{rest}").ok();
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
            word_diff_flush_hunk(
                &mut out,
                config,
                wd,
                header_use_color,
                word_re,
                &minus_buf,
                &plus_buf,
            );
            continue;
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }

    out
}

fn color_hunk_header_word_diff(header: &str, cb: &str, cr: &str, fb: &str, fr: &str) -> String {
    let Some(at2) = header.find("@@").map(|i| i + 2) else {
        return format!("{cb}{header}{cr}");
    };
    let after = &header[at2..];
    let Some(end) = after[2..].find("@@").map(|i| i + 4) else {
        return format!("{cb}{header}{cr}");
    };
    let frag_end = at2 + end;
    let frag = &header[..frag_end];
    let rest = &header[frag_end..];
    let trimmed = rest.trim_start_matches([' ', '\t']);
    let (space_part, func_part) = if rest.len() > trimmed.len() {
        let sp = &rest[..rest.len() - trimmed.len()];
        (sp, trimmed)
    } else {
        ("", rest)
    };
    if func_part.is_empty() {
        format!("{cb}{frag}{cr}{rest}")
    } else {
        // Git `emit_hunk_header`: the blank before the func header is emitted in
        // the (empty) context color and closed with a reset, then the func part.
        let space_reset = if space_part.is_empty() { "" } else { cr };
        format!("{cb}{frag}{cr}{space_part}{space_reset}{fb}{func_part}{fr}")
    }
}

fn word_diff_flush_hunk(
    out: &mut String,
    config: &ConfigSet,
    wd: &EffectiveWordDiff,
    use_color: bool,
    word_re: Option<&WordRe>,
    minus: &str,
    plus: &str,
) {
    if minus.is_empty() && plus.is_empty() {
        return;
    }
    let color_on = use_color;
    let (old_c, new_c, ctx_c) = if color_on {
        (
            ansi_diff_color(config, "diff.color.old", RED),
            ansi_diff_color(config, "diff.color.new", GREEN),
            ansi_diff_color(config, "diff.color.context", ""),
        )
    } else {
        (String::new(), String::new(), String::new())
    };
    match wd.mode {
        WordDiffModeCli::Porcelain => {
            let style = GitWordStyle {
                new_prefix: "+",
                new_suffix: "\n",
                new_color: String::new(),
                old_prefix: "-",
                old_suffix: "\n",
                old_color: String::new(),
                ctx_prefix: " ",
                ctx_suffix: "\n",
                ctx_color: String::new(),
                newline: "~\n",
            };
            git_word_diff_show(out, minus, plus, word_re, &style);
        }
        WordDiffModeCli::Plain => {
            // git `--word-diff=plain` keeps the `[-`/`-]`/`{+`/`+}` markers; `--color`
            // merely fills in the color fields (DIFF_WORDS_PLAIN style + colors).
            let style = if wd.force_color && color_on {
                GitWordStyle {
                    new_prefix: "{+",
                    new_suffix: "+}",
                    new_color: new_c,
                    old_prefix: "[-",
                    old_suffix: "-]",
                    old_color: old_c,
                    ctx_prefix: "",
                    ctx_suffix: "",
                    ctx_color: ctx_c,
                    newline: "\n",
                }
            } else {
                GitWordStyle {
                    new_prefix: "{+",
                    new_suffix: "+}",
                    new_color: String::new(),
                    old_prefix: "[-",
                    old_suffix: "-]",
                    old_color: String::new(),
                    ctx_prefix: "",
                    ctx_suffix: "",
                    ctx_color: String::new(),
                    newline: "\n",
                }
            };
            git_word_diff_show(out, minus, plus, word_re, &style);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        WordDiffModeCli::Color => {
            let style = GitWordStyle {
                new_prefix: "",
                new_suffix: "",
                new_color: new_c,
                old_prefix: "",
                old_suffix: "",
                old_color: old_c,
                ctx_prefix: "",
                ctx_suffix: "",
                ctx_color: ctx_c,
                newline: "\n",
            };
            git_word_diff_show(out, minus, plus, word_re, &style);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        WordDiffModeCli::None => {}
    }
}

fn mode_str_has_executable_bit(mode_str: &str) -> bool {
    u32::from_str_radix(mode_str, 8)
        .map(|m| m & 0o111 != 0)
        .unwrap_or(false)
}

fn compact_summary_display_path(entry: &DiffEntry, quote_path_fully: bool) -> String {
    let path = grit_lib::quote_path::quote_c_style(&entry.path().to_string_lossy(), quote_path_fully);
    match entry.status {
        DiffStatus::Added => format!("{path} (new)"),
        DiffStatus::Deleted => format!("{path} (gone)"),
        _ => {
            let old_x = mode_str_has_executable_bit(&entry.old_mode);
            let new_x = mode_str_has_executable_bit(&entry.new_mode);
            if new_x != old_x {
                if new_x {
                    format!("{path} (mode +x)")
                } else {
                    format!("{path} (mode -x)")
                }
            } else {
                path
            }
        }
    }
}

/// Per-file compact summary plus totals line (matches `git diff --compact-summary`).
fn write_compact_summary(
    out: &mut impl Write,
    entries: &[DiffEntry],
    odb: &Odb,
    work_tree: Option<&Path>,
    break_rewrites: bool,
    line_ignore: Option<&[Regex]>,
    ws_mode: &WhitespaceMode,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
    git_dir: &Path,
    quote_path_fully: bool,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut files: Vec<FileStatInput> = Vec::with_capacity(entries.len());
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for entry in entries {
        if entry.status == DiffStatus::Unmerged {
            // git lists unmerged paths as ` <name> | Unmerged` (not counted as changed).
            files.push(FileStatInput {
                path_display: compact_summary_display_path(entry, quote_path_fully),
                insertions: 0,
                deletions: 0,
                is_binary: false,
                is_unmerged: true,
            });
            continue;
        }
        let old_raw = read_content_raw(odb, &entry.old_oid);
        let new_raw = read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, entry.path());
        let binary = is_binary_or_large_for_diff(&old_raw, git_dir)
            || is_binary_or_large_for_diff(&new_raw, git_dir);
        let mode_only = entry.status == DiffStatus::Modified
            && entry.old_mode != entry.new_mode
            && old_raw == new_raw;
        let (ins, del) = if binary {
            let deleted = if mode_only || entry.old_oid == zero_oid() {
                0
            } else {
                old_raw.len()
            };
            let added = if mode_only || entry.new_oid == zero_oid() {
                0
            } else {
                new_raw.len()
            };
            (added, deleted)
        } else {
            if mode_only {
                (0, 0)
            } else {
                stat_ins_del_for_entry(
                    odb,
                    entry,
                    work_tree,
                    break_rewrites,
                    line_ignore,
                    ws_mode,
                    algo_ctx,
                    algo_cli,
                )
            }
        };
        total_ins += ins;
        total_del += del;
        files.push(FileStatInput {
            path_display: compact_summary_display_path(entry, quote_path_fully),
            insertions: ins,
            deletions: del,
            is_binary: binary,
            is_unmerged: false,
        });
    }

    let cfg = ConfigSet::load(Some(git_dir), false).unwrap_or_else(|_| ConfigSet::new());
    let stat_name_width = cfg
        .get("diff.statNameWidth")
        .and_then(|v| v.parse::<usize>().ok());
    let stat_graph_width = cfg
        .get("diff.statGraphWidth")
        .and_then(|v| v.parse::<usize>().ok());
    let opts = DiffstatOptions {
        total_width: terminal_columns(),
        line_prefix: "",
        width_prefix: "",
        subtract_prefix_from_terminal: false,
        stat_name_width,
        stat_graph_width,
        stat_count: None,
        color_add: "",
        color_del: "",
        color_reset: "",
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };
    write_diffstat_block(out, &files, &opts)?;

    let n = files.len();
    let mut summary = format!(" {} file{} changed", n, if n == 1 { "" } else { "s" });
    append_stat_counts(&mut summary, total_ins, total_del);
    writeln!(out, "{summary}")?;

    Ok(())
}

/// Write only the summary line: `N files changed, N insertions(+), N deletions(-)`.
fn write_shortstat(
    out: &mut impl Write,
    entries: &[DiffEntry],
    odb: &Odb,
    work_tree: Option<&Path>,
    break_rewrites: bool,
    line_ignore: Option<&[Regex]>,
    ws_mode: &WhitespaceMode,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    let mut files_changed = 0usize;

    for entry in entries {
        // Binary files count as a changed file but contribute no line counts
        // (t4012: `diff --shortstat` on binary change is "0 insertions, 0 deletions").
        let old_raw = read_content_raw(odb, &entry.old_oid);
        let new_raw = read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, entry.path());
        if is_binary(&old_raw) || is_binary(&new_raw) {
            files_changed += 1;
            continue;
        }
        let (ins, del) = stat_ins_del_for_entry(
            odb,
            entry,
            work_tree,
            break_rewrites,
            line_ignore,
            ws_mode,
            algo_ctx,
            algo_cli,
        );
        total_ins += ins;
        total_del += del;
        files_changed += 1;
    }

    if files_changed == 0 {
        return Ok(());
    }

    let mut summary = format!(
        " {} file{} changed",
        files_changed,
        if files_changed == 1 { "" } else { "s" }
    );
    append_stat_counts(&mut summary, total_ins, total_del);
    writeln!(out, "{summary}")?;

    Ok(())
}

/// Append insertions/deletions counts to a summary string.
/// Git only shows insertions/deletions when they are non-zero,
/// except when both are zero (e.g. mode-only changes).
fn append_stat_counts(summary: &mut String, total_ins: usize, total_del: usize) {
    if total_ins > 0 {
        summary.push_str(&format!(
            ", {} insertion{}(+)",
            total_ins,
            if total_ins == 1 { "" } else { "s" }
        ));
    }
    if total_del > 0 {
        summary.push_str(&format!(
            ", {} deletion{}(-)",
            total_del,
            if total_del == 1 { "" } else { "s" }
        ));
    }
    if total_ins == 0 && total_del == 0 {
        summary.push_str(", 0 insertions(+), 0 deletions(-)");
    }
}

/// Write a stat summary for each entry, followed by a totals line.
fn write_stat(
    out: &mut impl Write,
    entries: &[DiffEntry],
    odb: &Odb,
    work_tree: Option<&Path>,
    stat_count: Option<usize>,
    stat_width: Option<usize>,
    stat_name_width: Option<usize>,
    break_rewrites: bool,
    stat_graph_width: Option<usize>,
    line_prefix: &str,
    git_dir: &Path,
    line_ignore: Option<&[Regex]>,
    ws_mode: &WhitespaceMode,
    quote_path_fully: bool,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let display_paths: Vec<String> = entries
        .iter()
        .map(|e| match e.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let old = e.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let new = e.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                format_rename_display(&old, &new, quote_path_fully)
            }
            _ => grit_lib::quote_path::quote_c_style(&e.path().to_string_lossy(), quote_path_fully),
        })
        .collect();

    let mut files: Vec<FileStatInput> = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        if entry.status == DiffStatus::Unmerged {
            // git lists unmerged paths as ` <name> | Unmerged` (not counted).
            files.push(FileStatInput {
                path_display: display_paths[i].clone(),
                insertions: 0,
                deletions: 0,
                is_binary: false,
                is_unmerged: true,
            });
            continue;
        }
        let old_raw = read_content_raw(odb, &entry.old_oid);
        let new_raw = read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, entry.path());
        let binary = is_binary_or_large_for_diff(&old_raw, git_dir)
            || is_binary_or_large_for_diff(&new_raw, git_dir);
        let mode_only = entry.status == DiffStatus::Modified
            && entry.old_mode != entry.new_mode
            && old_raw == new_raw;
        if binary {
            let deleted = if mode_only || entry.old_oid == zero_oid() {
                0
            } else {
                old_raw.len()
            };
            let added = if mode_only || entry.new_oid == zero_oid() {
                0
            } else {
                new_raw.len()
            };
            files.push(FileStatInput {
                path_display: display_paths[i].clone(),
                insertions: added,
                deletions: deleted,
                is_binary: true,
                is_unmerged: false,
            });
        } else {
            let (ins, del) = if mode_only {
                (0, 0)
            } else {
                stat_ins_del_for_entry(
                    odb,
                    entry,
                    work_tree,
                    break_rewrites,
                    line_ignore,
                    ws_mode,
                    algo_ctx,
                    algo_cli,
                )
            };
            files.push(FileStatInput {
                path_display: display_paths[i].clone(),
                insertions: ins,
                deletions: del,
                is_binary: false,
                is_unmerged: false,
            });
        }
    }

    let total_w = stat_width.unwrap_or_else(terminal_columns);
    let cfg = grit_lib::config::ConfigSet::load(Some(git_dir), false)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let eff_name = stat_name_width.or_else(|| {
        cfg.get("diff.statNameWidth")
            .and_then(|s| s.parse::<usize>().ok())
    });
    let eff_graph = stat_graph_width.or_else(|| {
        cfg.get("diff.statGraphWidth")
            .and_then(|s| s.parse::<usize>().ok())
    });

    let opts = DiffstatOptions {
        total_width: total_w,
        line_prefix,
        width_prefix: "",
        subtract_prefix_from_terminal: stat_width.is_none() && !line_prefix.is_empty(),
        stat_name_width: eff_name,
        stat_graph_width: eff_graph,
        stat_count,
        color_add: "",
        color_del: "",
        color_reset: "",
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };
    write_diffstat_block(out, &files, &opts)?;
    Ok(())
}

/// Format a rename/copy path for numstat: `{old_quoted}\t{new_quoted}` or
/// `{old_quoted} => {new_quoted}` depending on format.
fn format_rename_display(old: &str, new: &str, quote_path_fully: bool) -> String {
    // Git pprint_rename: when either side needs C-style quoting there is no
    // `{a => b}` common-prefix compaction; each side is quoted independently
    // (t4016: `"with\tHT" => Rpathname with SP.1`).
    let old_q = grit_lib::quote_path::quote_c_style(old, quote_path_fully);
    let new_q = grit_lib::quote_path::quote_c_style(new, quote_path_fully);
    if old_q != old || new_q != new {
        return format!("{old_q} => {new_q}");
    }
    // Use the pretty-print format with common prefix/suffix like c/{b/a => d/e}
    grit_lib::diff::format_rename_path(old, new)
}

/// Write machine-readable numstat output: `{insertions}\t{deletions}\t{path}`.
fn write_numstat(
    out: &mut impl Write,
    entries: &[DiffEntry],
    odb: &Odb,
    work_tree: Option<&Path>,
    break_rewrites: bool,
    line_ignore: Option<&[Regex]>,
    ws_mode: &WhitespaceMode,
    algo_ctx: &DiffAlgoContext,
    algo_cli: Option<CliDiffAlgo>,
) -> Result<()> {
    for entry in entries {
        let old_raw = read_content_raw(odb, &entry.old_oid);
        let new_raw = read_content_raw_or_worktree(odb, &entry.new_oid, work_tree, entry.path());
        let binary_rewrite_numstat = break_rewrites
            && entry.status == DiffStatus::Modified
            && entry.score.is_some()
            && (is_binary(&old_raw) || is_binary(&new_raw));
        match entry.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let (ins, del) = stat_ins_del_for_entry(
                    odb,
                    entry,
                    work_tree,
                    break_rewrites,
                    line_ignore,
                    ws_mode,
                    algo_ctx,
                    algo_cli,
                );
                let old = entry.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let new = entry.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let display = format_rename_display(&old, &new, false);
                writeln!(out, "{ins}\t{del}\t{display}")?;
            }
            _ if binary_rewrite_numstat => {
                writeln!(out, "-\t-\t{}", entry.path())?;
            }
            _ => {
                let (ins, del) = stat_ins_del_for_entry(
                    odb,
                    entry,
                    work_tree,
                    break_rewrites,
                    line_ignore,
                    ws_mode,
                    algo_ctx,
                    algo_cli,
                );
                writeln!(out, "{ins}\t{del}\t{}", entry.path())?;
            }
        }
    }
    Ok(())
}

/// Write only the names of changed files.
/// Write `--summary` output for rename/copy/mode-change entries.
pub(crate) fn write_diff_summary(
    out: &mut impl Write,
    entries: &[DiffEntry],
    break_rewrites: bool,
    quote_path_fully: bool,
) -> Result<()> {
    for entry in entries {
        match entry.status {
            DiffStatus::Renamed => {
                let old = entry.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let new = entry.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let display = format_rename_display(&old, &new, quote_path_fully);
                let sim = entry.score.unwrap_or(100);
                writeln!(out, " rename {display} ({sim}%)")?;
            }
            DiffStatus::Copied => {
                let old = entry.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let new = entry.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let display = format_rename_display(&old, &new, quote_path_fully);
                let sim = entry.score.unwrap_or(100);
                writeln!(out, " copy {display} ({sim}%)")?;
            }
            DiffStatus::Added => {
                writeln!(
                    out,
                    " create mode {} {}",
                    entry.new_mode,
                    grit_lib::quote_path::quote_c_style(&entry.path().to_string_lossy(), quote_path_fully)
                )?;
            }
            DiffStatus::Deleted => {
                writeln!(
                    out,
                    " delete mode {} {}",
                    entry.old_mode,
                    grit_lib::quote_path::quote_c_style(&entry.path().to_string_lossy(), quote_path_fully)
                )?;
            }
            DiffStatus::Modified => {
                if break_rewrites {
                    if let Some(pct) = entry.score {
                        writeln!(
                            out,
                            " rewrite {} ({pct}%)",
                            grit_lib::quote_path::quote_c_style(&entry.path().to_string_lossy(), quote_path_fully)
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn write_name_only(
    out: &mut impl Write,
    entries: &[DiffEntry],
    quote_path_fully: bool,
) -> Result<()> {
    for entry in entries {
        writeln!(
            out,
            "{}",
            grit_lib::quote_path::quote_c_style(&entry.path().to_string_lossy(), quote_path_fully)
        )?;
    }
    Ok(())
}

/// Write `{status_letter}\t{path}` for each entry.
/// For renames/copies, output `R100\told_path\tnew_path`.
/// Write raw diff format: `:old-mode new-mode old-oid new-oid status\tpath`
fn write_raw(
    out: &mut impl Write,
    entries: &[DiffEntry],
    abbrev_len: usize,
    worktree_new_side: bool,
) -> Result<()> {
    // When OIDs are abbreviated (abbrev_len < 40), git appends `...` if
    // GIT_PRINT_SHA1_ELLIPSIS=yes (the default ellipsis behaviour for `--raw`).
    let ell = if abbrev_len < 40
        && std::env::var("GIT_PRINT_SHA1_ELLIPSIS").ok().as_deref() == Some("yes")
    {
        "..."
    } else {
        ""
    };
    for entry in entries {
        let old_mode = &entry.old_mode;
        let new_mode = &entry.new_mode;
        // A null OID is stored at SHA-1 width regardless of repository algorithm;
        // render it at the width of the non-null side so `--no-abbrev`/`--full-index`
        // output is consistent (64 zeros in a sha256 repo, not 40).
        let entry_hex_width = if !entry.old_oid.is_zero() {
            entry.old_oid.algo().hex_len()
        } else if !entry.new_oid.is_zero() {
            entry.new_oid.algo().hex_len()
        } else {
            entry.old_oid.algo().hex_len()
        };
        let old_oid_hex = if entry.old_oid.is_zero() {
            "0".repeat(entry_hex_width)
        } else {
            entry.old_oid.to_hex()
        };
        let new_oid_hex = if entry.new_oid.is_zero() {
            "0".repeat(entry_hex_width)
        } else {
            entry.new_oid.to_hex()
        };
        let olen = abbrev_len.min(old_oid_hex.len());
        let nlen = abbrev_len.min(new_oid_hex.len());
        let old_oid = format!("{}{ell}", &old_oid_hex[..olen]);
        // A gitlink whose new side is the work tree (e.g. a blob→gitlink typechange or a
        // worktree-side submodule bump) prints the all-zero OID in raw plumbing, even though the
        // `--submodule` patch shows the real HEAD. See t4041/t4060.
        let zero_wt_gitlink = worktree_new_side && new_mode == "160000";
        let zeroed = format!("{}{ell}", "0".repeat(nlen));
        let new_oid = if zero_wt_gitlink {
            zeroed.clone()
        } else {
            format!("{}{ell}", &new_oid_hex[..nlen])
        };
        let status = entry.status.letter();
        match entry.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let score = entry.score.unwrap_or(100);
                let old_path = entry.old_path.as_deref().unwrap_or(RepoPath::from_str(""));
                let new_path = entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""));
                writeln!(out, ":{old_mode} {new_mode} {old_oid} {new_oid} {status}{score:03}\t{old_path}\t{new_path}")?;
            }
            DiffStatus::Modified => {
                let zero_new = (old_mode == "160000"
                    && new_mode == "160000"
                    && entry.old_oid == entry.new_oid)
                    .then(|| format!("{}{ell}", "0".repeat(abbrev_len)));
                let rn = zero_new.as_deref().unwrap_or(new_oid.as_str());
                if let Some(pct) = entry.score {
                    writeln!(
                        out,
                        ":{old_mode} {new_mode} {old_oid} {rn} {status}{pct:03}\t{}",
                        entry.path()
                    )?;
                } else {
                    writeln!(
                        out,
                        ":{old_mode} {new_mode} {old_oid} {rn} {status}\t{}",
                        entry.path()
                    )?;
                }
            }
            _ => {
                writeln!(
                    out,
                    ":{old_mode} {new_mode} {old_oid} {new_oid} {status}\t{}",
                    entry.path()
                )?;
            }
        }
    }
    Ok(())
}

fn write_name_status(
    out: &mut impl Write,
    entries: &[DiffEntry],
    quote_path_fully: bool,
) -> Result<()> {
    for entry in entries {
        match entry.status {
            DiffStatus::Renamed => {
                let s = entry.score.unwrap_or(100);
                writeln!(
                    out,
                    "R{:03}\t{}\t{}",
                    s,
                    grit_lib::quote_path::quote_c_style(
                        &entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")).to_string_lossy(),
                        quote_path_fully,
                    ),
                    grit_lib::quote_path::quote_c_style(
                        &entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")).to_string_lossy(),
                        quote_path_fully,
                    ),
                )?;
            }
            DiffStatus::Copied => {
                let s = entry.score.unwrap_or(100);
                writeln!(
                    out,
                    "C{:03}\t{}\t{}",
                    s,
                    grit_lib::quote_path::quote_c_style(
                        &entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")).to_string_lossy(),
                        quote_path_fully,
                    ),
                    grit_lib::quote_path::quote_c_style(
                        &entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")).to_string_lossy(),
                        quote_path_fully,
                    ),
                )?;
            }
            _ => {
                writeln!(
                    out,
                    "{}\t{}",
                    entry.status.letter(),
                    grit_lib::quote_path::quote_c_style(&entry.path().to_string_lossy(), quote_path_fully)
                )?;
            }
        }
    }
    Ok(())
}

/// Git default conflict marker width (`DEFAULT_CONFLICT_MARKER_SIZE` in upstream).
const DEFAULT_CONFLICT_MARKER_SIZE: usize = 7;

fn conflict_marker_size_for_path(
    attrs: &ParsedGitAttributes,
    rel_path: &str,
    ignore_case_attrs: bool,
) -> usize {
    let mut size = DEFAULT_CONFLICT_MARKER_SIZE;
    let map = collect_attrs_for_path(&attrs.rules, &attrs.macros, rel_path, ignore_case_attrs);
    if let Some(AttrValue::Value(s)) = map.get("conflict-marker-size") {
        if let Ok(n) = s.trim().parse::<i32>() {
            if n > 0 {
                size = n as usize;
            }
        }
    }
    size
}

fn load_cached_diff_gitattributes(repo: &Repository, index: &Index) -> Result<ParsedGitAttributes> {
    let mut merged = load_gitattributes_for_diff(repo)?;
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(merged);
    };

    let mut missing_attr_index = Index::new();
    missing_attr_index.entries = index
        .entries
        .iter()
        .filter(|entry| {
            entry.stage() == 0
                && entry.path.ends_with(b".gitattributes")
                && std::str::from_utf8(&entry.path)
                    .map(|path| !work_tree.join(path).exists())
                    .unwrap_or(false)
        })
        .cloned()
        .collect();

    if missing_attr_index.entries.is_empty() {
        return Ok(merged);
    }

    let mut from_index = load_gitattributes_from_index(&missing_attr_index, &repo.odb, work_tree)?;
    merged.rules.append(&mut from_index.rules);
    for (name, defs) in from_index.macros.defs.drain() {
        merged.macros.defs.insert(name, defs);
    }
    merged.warnings.append(&mut from_index.warnings);
    Ok(merged)
}

fn config_whitespace_rule_bits(config: &ConfigSet) -> Result<u32, ws::WhitespaceRuleError> {
    let value = config
        .get("core.whitespace")
        .unwrap_or_else(|| "".to_owned());
    ws::parse_whitespace_rule(&value)
}

fn effective_ws_rule_for_path(
    attrs: &ParsedGitAttributes,
    rel_path: &str,
    entry_mode: &str,
    config: &ConfigSet,
    ignore_case_attrs: bool,
) -> Result<u32, ws::WhitespaceRuleError> {
    let cfg_rule = config_whitespace_rule_bits(config)?;
    let map = collect_attrs_for_path(&attrs.rules, &attrs.macros, rel_path, ignore_case_attrs);
    let git_attr = match map.get("whitespace") {
        None | Some(AttrValue::Clear) => WhitespaceGitAttr::Unspecified,
        Some(AttrValue::Unset) => WhitespaceGitAttr::False,
        Some(AttrValue::Set) => WhitespaceGitAttr::True,
        Some(AttrValue::Value(s)) => WhitespaceGitAttr::String(s.clone()),
    };
    let mut rule = git_attr.merge_with_config(cfg_rule)?;
    if entry_mode == "120000" {
        rule &= !WS_INCOMPLETE_LINE;
    }
    Ok(rule)
}

/// Git `count_lines` from `diff.c`: counts logical lines including a final non-newline-terminated line.
fn count_lines_git(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut nl_just_seen = false;
    let mut completely_empty = true;
    for &ch in data {
        if ch == b'\n' {
            count += 1;
            nl_just_seen = true;
            completely_empty = false;
        } else {
            nl_just_seen = false;
            completely_empty = false;
        }
    }
    if completely_empty {
        return 0;
    }
    if !nl_just_seen {
        count += 1;
    }
    count
}

/// Port of Git `count_trailing_blank` (`diff.c`).
fn count_trailing_blank_lines(data: &[u8]) -> usize {
    let size = data.len();
    if size == 0 {
        return 0;
    }
    let mut ptr = size - 1;
    if data[ptr] != b'\n' {
        return 0;
    }
    if ptr == 0 {
        return 0;
    }
    ptr -= 1;
    let mut cnt = 0usize;
    loop {
        let mut prev_eol = ptr;
        loop {
            if data[prev_eol] == b'\n' {
                break;
            }
            if prev_eol == 0 {
                let body = &data[..=ptr];
                if ws::ws_blank_line(&String::from_utf8_lossy(body)) {
                    cnt += 1;
                }
                return cnt;
            }
            prev_eol -= 1;
        }
        let body = &data[prev_eol + 1..=ptr];
        if !ws::ws_blank_line(&String::from_utf8_lossy(body)) {
            break;
        }
        cnt += 1;
        if prev_eol == 0 {
            break;
        }
        ptr = prev_eol - 1;
    }
    cnt
}

/// Line number (1-based) of the first new trailing blank line at EOF, if `post` has more than `pre`.
fn blank_at_eof_error_line(pre: &[u8], post: &[u8], ws_rule: u32) -> Option<usize> {
    if ws_rule & WS_BLANK_AT_EOF == 0 {
        return None;
    }
    let l1 = count_trailing_blank_lines(pre);
    let l2 = count_trailing_blank_lines(post);
    if l2 <= l1 {
        return None;
    }
    let total = count_lines_git(post);
    let line = total.saturating_sub(l2).saturating_add(1);
    Some(line)
}

fn parse_udiff_range(part: &str) -> Option<(i32, i32)> {
    let part = part.trim();
    let mut it = part.split(',');
    let start: i32 = it.next()?.parse().ok()?;
    let count = if let Some(c) = it.next() {
        c.parse().ok()?
    } else {
        1
    };
    Some((start, count))
}

/// Parse `@@ -old_start,old_count +new_start,new_count @@` header; returns `(new_start, new_count)`.
fn parse_unified_hunk_header(line: &str) -> Option<(i32, i32)> {
    let s = line.strip_prefix("@@")?.trim_start();
    let (old_raw, new_raw) = s.split_once(" +")?;
    let _ = old_raw.strip_prefix('-')?;
    let new_range = new_raw.split("@@").next()?.trim();
    parse_udiff_range(new_range)
}

/// Reconstruct a `+` line body for `ws_check` after splitting a patch on `\n`.
///
/// `str::lines()` strips the newline that terminates each complete patch line; Git's checkdiff
/// passes the line *including* that newline to `ws_check`, except when the unified diff uses
/// `\ No newline at end of file` (then the `+` line has no trailing `\n`).
fn patch_line_body_with_newline_for_ws_check(
    patch_lines: &[&str],
    idx: usize,
    body: &str,
) -> String {
    let incomplete_eof = patch_lines
        .get(idx + 1)
        .is_some_and(|l| l.starts_with('\\'));
    if incomplete_eof {
        body.to_owned()
    } else {
        format!("{body}\n")
    }
}

/// Match upstream `is_conflict_marker` in `git/diff.c`.
fn is_conflict_marker_line(body: &str, marker_size: usize) -> bool {
    let line = body.trim_end_matches(['\n', '\r']);
    let len = line.len();
    if len < marker_size {
        return false;
    }
    let first = match line.as_bytes().first().copied() {
        Some(b) => b,
        None => return false,
    };
    if !matches!(first, b'=' | b'>' | b'<' | b'|') {
        return false;
    }
    for i in 1..marker_size {
        if line.as_bytes().get(i).copied() != Some(first) {
            return false;
        }
    }
    // Middle conflict line is exactly `marker_size` '=' characters (no trailing label).
    if first == b'=' && len == marker_size {
        return true;
    }
    if len < marker_size + 1 {
        return false;
    }
    line.as_bytes()
        .get(marker_size)
        .is_some_and(|b| b.is_ascii_whitespace())
}

/// Check for whitespace errors in added/modified lines (`git diff --check` / `diff-index --check`).
///
/// Returns true if any errors were found.
pub(crate) fn check_whitespace_errors(
    out: &mut impl Write,
    entries: &[DiffEntry],
    odb: &Odb,
    work_tree: Option<&Path>,
    merged_attrs: &ParsedGitAttributes,
    ignore_case_attrs: bool,
    config: &ConfigSet,
) -> Result<bool> {
    use grit_lib::diff::zero_oid;
    if let Err(ws::WhitespaceRuleError::ConflictingTabAndIndentRules) =
        config_whitespace_rule_bits(config)
    {
        eprintln!("fatal: cannot enforce both tab-in-indent and indent-with-non-tab");
        std::process::exit(128);
    }
    let mut has_errors = false;
    let indent_heuristic = resolve_indent_heuristic(config, false, false);
    let quote_path_fully = config.quote_path_fully();

    for entry in entries {
        if entry.status == DiffStatus::Deleted {
            continue;
        }
        let path = entry.path();
        let path_str = path.to_string_lossy();
        let marker_size = conflict_marker_size_for_path(merged_attrs, &path_str, ignore_case_attrs);
        let ws_rule = match effective_ws_rule_for_path(
            merged_attrs,
            &path_str,
            &entry.new_mode,
            config,
            ignore_case_attrs,
        ) {
            Ok(r) => r,
            Err(ws::WhitespaceRuleError::ConflictingTabAndIndentRules) => {
                eprintln!("fatal: cannot enforce both tab-in-indent and indent-with-non-tab");
                std::process::exit(128);
            }
        };

        let old_path = entry.old_path.as_deref().unwrap_or(path);
        let old_content = if entry.old_oid == zero_oid() {
            String::new()
        } else {
            read_content(odb, &entry.old_oid, work_tree, old_path)
        };
        let new_content = if entry.new_oid == zero_oid() {
            String::new()
        } else {
            read_content(odb, &entry.new_oid, work_tree, path)
        };

        if is_binary(&read_content_raw(odb, &entry.old_oid))
            || is_binary(&read_content_raw_or_worktree(
                odb,
                &entry.new_oid,
                work_tree,
                path,
            ))
        {
            continue;
        }

        let patch = unified_diff_with_prefix_and_funcname_and_algorithm(
            &old_content,
            &new_content,
            &path_str,
            &path_str,
            1,
            0,
            "",
            "",
            None,
            similar::Algorithm::Myers,
            false,
            false,
            indent_heuristic,
            quote_path_fully,
        );

        let patch_lines: Vec<&str> = patch.lines().collect();
        let mut lineno = 0i32;
        for (idx, raw_line) in patch_lines.iter().enumerate() {
            if raw_line.starts_with("@@") {
                if let Some((start, _)) = parse_unified_hunk_header(raw_line) {
                    lineno = start.saturating_sub(1);
                }
                continue;
            }
            if raw_line.is_empty() {
                continue;
            }
            let kind = raw_line.as_bytes().first().copied().unwrap_or(b' ');
            if kind == b'+' {
                lineno += 1;
                let body = &raw_line[1..];
                let body_for_check =
                    patch_line_body_with_newline_for_ws_check(&patch_lines, idx, body);
                let bare = body_for_check.trim_end_matches(['\n', '\r']);
                if is_conflict_marker_line(bare, marker_size) {
                    writeln!(out, "{path}:{lineno}: leftover conflict marker")?;
                    write!(out, "+{body}")?;
                    if !body.ends_with('\n') {
                        writeln!(out)?;
                    }
                    has_errors = true;
                }
                let bad = ws::ws_check(&body_for_check, ws_rule);
                if bad != 0 {
                    let msg = ws::whitespace_error_string(bad);
                    writeln!(out, "{path}:{lineno}: {msg}.")?;
                    write!(out, "+{body}")?;
                    if !body.ends_with('\n') {
                        writeln!(out)?;
                    }
                    has_errors = true;
                }
            } else if kind == b' ' {
                lineno += 1;
            } else if kind == b'\\' {
                // `\ No newline at end of file` — incomplete-line errors come from `ws_check`
                // on the preceding `+` line (Git `checkdiff_consume`).
            }
        }

        if let Some(ln) =
            blank_at_eof_error_line(old_content.as_bytes(), new_content.as_bytes(), ws_rule)
        {
            writeln!(
                out,
                "{path}:{ln}: {}.",
                ws::whitespace_error_string(WS_BLANK_AT_EOF)
            )?;
            has_errors = true;
        }
    }
    Ok(has_errors)
}

/// Resolve the source and destination prefixes for diff output, considering
/// command-line flags and config options.
fn resolve_diff_prefixes(args: &Args, repo: &Repository, cached: bool) -> (String, String) {
    if args.default_prefix {
        return ("a/".to_owned(), "b/".to_owned());
    }
    if args.no_prefix {
        return (String::new(), String::new());
    }
    let explicit_src = args.src_prefix.is_some();
    let explicit_dst = args.dst_prefix.is_some();
    if explicit_src && explicit_dst {
        return (
            args.src_prefix.clone().unwrap_or_default(),
            args.dst_prefix.clone().unwrap_or_default(),
        );
    }

    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();

    if let Some(ref cfg) = config {
        if let Some(ref val) = cfg.get("diff.noprefix") {
            if val == "true" || val == "yes" || val == "on" || val == "1" {
                return (String::new(), String::new());
            }
        }
        if let Some(ref val) = cfg.get("diff.mnemonicprefix") {
            if val == "true" || val == "yes" || val == "on" || val == "1" {
                // Matches Git: cached diff is commit vs index (`c/` vs `i/`); otherwise index vs worktree (`i/` vs `w/`).
                if cached {
                    return ("c/".to_owned(), "i/".to_owned());
                }
                return ("i/".to_owned(), "w/".to_owned());
            }
        }
    }

    let src = if explicit_src {
        args.src_prefix.clone().unwrap_or_default()
    } else if let Some(ref cfg) = config {
        cfg.get("diff.srcprefix").unwrap_or_else(|| "a/".to_owned())
    } else {
        "a/".to_owned()
    };

    let dst = if explicit_dst {
        args.dst_prefix.clone().unwrap_or_default()
    } else if let Some(ref cfg) = config {
        cfg.get("diff.dstprefix").unwrap_or_else(|| "b/".to_owned())
    } else {
        "b/".to_owned()
    };

    (src, dst)
}

/// Write unified diff output for a list of DiffEntry pairs.
pub fn write_patch_from_pairs(
    out: &mut dyn std::io::Write,
    entries: &[DiffEntry],
    repo: &Repository,
) -> anyhow::Result<()> {
    // Match `git diff-tree -p` defaults: 3 context lines, 7-char abbrev,
    // config-driven path quoting, indent heuristic on (t4070 diff-pairs).
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let quote_fully = config.quote_path_fully();
    let indent_heuristic = grit_lib::diff::resolve_indent_heuristic(&config, false, false);
    let mut out = out;
    for entry in entries {
        crate::commands::diff_tree::write_patch_entry(
            &mut out,
            &repo.odb,
            entry,
            3,
            Some(7),
            false,
            false,
            false,
            &repo.git_dir,
            quote_fully,
            indent_heuristic,
        )?;
    }
    Ok(())
}
