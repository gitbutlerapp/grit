//! `grit diff-index` command.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::diff::{
    detect_renames, diff_trees, empty_blob_oid, parse_diff_rename_score_token,
    parse_indent_heuristic_cli_flags, read_submodule_head_oid, resolve_indent_heuristic,
    rewrite_dissimilarity_index_percent, rewrite_merge_score, should_break_rewrite_pair,
    stat_matches, submodule_commit_subject_line, zero_oid, DiffEntry, DiffStatus,
    GIT_DIFF_DEFAULT_MERGE_SCORE_AFTER_BREAK,
};
use grit_lib::index::{
    Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK,
};
use grit_lib::merge_base::{merge_base_for_diff_index, MergeBaseForDiffError};
use grit_lib::repo_path::{RepoPath, RepoPathBuf};
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pathspec::context_from_mode_bits;
use grit_lib::quote_path::{format_diff_path_with_prefix, quote_c_style};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{merge_bases, rev_list, RevListOptions};
use grit_lib::rev_parse::{abbreviate_object_id, resolve_revision, resolve_revision_as_commit};

use crate::commands::diff::check_whitespace_errors;
use grit_lib::attributes::load_gitattributes_for_diff;
use grit_lib::config::ConfigSet;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Arguments for `grit diff-index`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Raw command arguments forwarded by the CLI parser.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

/// Run `grit diff-index`.
pub fn run(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    if grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir)) {
        crate::precompose::precompose_plumbing_argv(&mut args.args);
    }
    let options = parse_options(&args.args)?;
    let quote_fully = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_default()
        .quote_path_fully();
    let tree_oid = if options.merge_base {
        let head_oid = resolve_revision_as_commit(&repo, "HEAD")
            .map_err(|e| anyhow::anyhow!("unable to get HEAD: {e}"))?;
        let other_oid =
            resolve_revision(&repo, &options.tree_ish).map_err(|e| anyhow::anyhow!("{e}"))?;
        let other_oid = peel_tag_chain_to_oid(&repo, other_oid)?;
        let obj = repo.odb.read(&other_oid)?;
        if obj.kind != ObjectKind::Commit {
            bail!(
                "fatal: {} is a {}, not a commit",
                options.tree_ish,
                object_kind_phrase(obj.kind)
            );
        }
        let mb = match merge_base_for_diff_index(&repo, head_oid, other_oid) {
            Ok(oid) => oid,
            Err(MergeBaseForDiffError::None) => {
                bail!("fatal: no merge base found");
            }
            Err(MergeBaseForDiffError::Multiple) => {
                bail!("fatal: multiple merge bases found");
            }
            Err(MergeBaseForDiffError::Other(msg)) => {
                bail!("{msg}");
            }
        };
        resolve_tree_ish_from_commit_oid(&repo, mb)?
    } else {
        resolve_tree_ish(&repo, &options.tree_ish)?
    };

    let mut tree_map = BTreeMap::new();
    collect_tree_entries(&repo, &tree_oid, "", &mut tree_map)?;

    let index_path = effective_index_path(&repo)?;
    let index = repo.load_index_at(&index_path).context("loading index")?;
    let mut index_map = BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        if let Ok(path) = String::from_utf8(entry.path.clone()) {
            let ctx = context_from_mode_bits(entry.mode);
            if entry_matches_pathspecs(&path, &options.pathspecs, ctx) {
                index_map.insert(path, Snapshot::from_index_entry(entry.mode, entry.oid));
            }
        }
    }
    tree_map.retain(|path, snap| {
        entry_matches_pathspecs(path, &options.pathspecs, context_from_mode_bits(snap.mode))
    });

    let changes = if options.cached {
        diff_tree_vs_index(&tree_map, &index_map)
    } else {
        diff_tree_vs_worktree(
            &repo,
            &tree_map,
            &index_map,
            &index,
            options.match_missing,
            SubmoduleIgnoreFlags {
                ignore_all: options.ignore_submodules_all,
                ignore_untracked: options.ignore_untracked_in_submodules,
                ignore_dirty: options.ignore_dirty_submodules,
            },
        )?
    };

    // Convert to DiffEntry for rename detection and output.
    let mut diff_entries: Vec<DiffEntry> = changes.iter().map(raw_change_to_diff_entry).collect();

    if options.ignore_submodules_all {
        diff_entries.retain(|e| e.old_mode != "160000" && e.new_mode != "160000");
    }

    if options.patch
        && options.submodule_format != SubmodulePatchFormat::Short
        && !options.cached
        && !options.ignore_submodules_all
        && !options.ignore_dirty_submodules
    {
        if let Some(wt) = repo.work_tree.as_deref() {
            let sm_ignore = SubmoduleIgnoreFlags {
                ignore_all: options.ignore_submodules_all,
                ignore_untracked: options.ignore_untracked_in_submodules,
                ignore_dirty: options.ignore_dirty_submodules,
            };
            for (path, snap) in &index_map {
                if snap.mode != MODE_GITLINK {
                    continue;
                }
                if !entry_matches_pathspecs(
                    path,
                    &options.pathspecs,
                    context_from_mode_bits(snap.mode),
                ) {
                    continue;
                }
                let tree_snap = tree_map.get(path);
                let same_recorded =
                    tree_snap.is_some_and(|t| t.mode == MODE_GITLINK && t.oid == snap.oid);
                if !same_recorded {
                    continue;
                }
                let dirty = submodule_dirty_flags(
                    wt,
                    path,
                    &snap.oid,
                    sm_ignore.ignore_untracked,
                    sm_ignore.ignore_dirty,
                );
                if !dirty.untracked && !dirty.modified {
                    continue;
                }
                if diff_entries.iter().any(|e| {
                    e.path() == path.as_str() && (e.old_mode == "160000" || e.new_mode == "160000")
                }) {
                    continue;
                }
                diff_entries.push(DiffEntry {
                    status: DiffStatus::Modified,
                    // TODO(byte-paths): lossy until index_map keys migrate (Phase 2/4)
                    old_path: Some(RepoPathBuf::from_string(path.clone())),
                    new_path: Some(RepoPathBuf::from_string(path.clone())),
                    old_mode: "160000".to_owned(),
                    new_mode: "160000".to_owned(),
                    old_oid: snap.oid,
                    new_oid: snap.oid,
                    score: None,
                });
            }
        }
    }

    let rename_work_root = if options.cached {
        None
    } else {
        repo.work_tree.as_deref()
    };

    let (mut diff_entries, broken_for_merge) = if options.break_rewrites {
        apply_diffcore_break_rewrites_split(
            &repo.odb,
            rename_work_root,
            options.cached,
            diff_entries,
            options.break_score,
            options.merge_after_break_score,
        )
    } else {
        (diff_entries, HashSet::new())
    };

    let source_tree_entries: Vec<(String, String, ObjectId)> = tree_map
        .iter()
        .map(|(path, snap)| (path.clone(), format!("{:06o}", snap.mode), snap.oid))
        .collect();

    diff_entries = if options.find_copies {
        let threshold = options.find_renames.unwrap_or(50);
        grit_lib::diff::detect_copies(
            &repo.odb,
            rename_work_root,
            diff_entries,
            threshold,
            options.find_copies_harder,
            &source_tree_entries,
        )
    } else if let Some(threshold) = options.find_renames {
        let mut d = detect_renames(&repo.odb, rename_work_root, diff_entries, threshold);
        // Git runs copy detection after rename when `-B` and `-M` are combined (e.g.
        // `t4008-diff-break-rewrite` #6): a type-changed path can be the copy source for a modified
        // sibling even without `-C`.
        if options.break_rewrites {
            d = grit_lib::diff::detect_copies(
                &repo.odb,
                rename_work_root,
                d,
                threshold,
                false,
                &source_tree_entries,
            );
        }
        d
    } else {
        diff_entries
    };

    if options.break_rewrites && !broken_for_merge.is_empty() {
        diff_entries = merge_broken_rewrite_pairs(diff_entries, &broken_for_merge);
    }

    if options.break_rewrites && options.find_renames.is_some() {
        diff_entries = drop_break_delete_superseded_by_rename_dest(diff_entries);
    }

    if options.break_rewrites {
        for e in &mut diff_entries {
            if e.status != DiffStatus::TypeChanged {
                continue;
            }
            if e.score.is_some() {
                continue;
            }
            if e.old_oid == zero_oid() || e.new_oid == zero_oid() {
                continue;
            }
            let Ok(old_obj) = repo.odb.read(&e.old_oid) else {
                continue;
            };
            let Ok(new_obj) = repo.odb.read(&e.new_oid) else {
                continue;
            };
            if grit_lib::merge_file::is_binary(&old_obj.data)
                || grit_lib::merge_file::is_binary(&new_obj.data)
            {
                continue;
            }
            if let Some(pct) = rewrite_dissimilarity_index_percent(&old_obj.data, &new_obj.data) {
                e.score = Some(pct);
            }
        }
    }

    let diff_entries = if options.ignore_all_space {
        filter_entries_ignore_all_space(&repo, diff_entries)
    } else if options.ignore_space_change {
        filter_entries_ignore_space_change(&repo, diff_entries)
    } else {
        diff_entries
    };

    let mut diff_entries = diff_entries;
    if options.cached {
        let unmerged_paths = collect_unmerged_index_paths(&index);
        if !unmerged_paths.is_empty() {
            // Git `diff-index --cached`: rename/copy detection runs on the full index↔tree diff
            // (including `D` on unmerged paths), then unmerged paths are emitted as `U` — not as
            // `D`/`R`. Copy detection may still attribute new blobs to an unmerged source (`-C`).
            let unmerged_ref = &unmerged_paths;
            diff_entries.retain(|e| {
                if unmerged_ref.contains(&*e.path().to_string_lossy())
                    && e.status == DiffStatus::Deleted
                {
                    return false;
                }
                true
            });
            for e in &mut diff_entries {
                let src = e.old_path.as_deref();
                if src.is_some_and(|p| unmerged_ref.contains(&*p.to_string_lossy())) {
                    if options.find_copies {
                        // Git `-C`: paths still show as copies from the unmerged source (not renames).
                        if e.status == DiffStatus::Renamed {
                            e.status = DiffStatus::Copied;
                        }
                    } else if e.status == DiffStatus::Renamed || e.status == DiffStatus::Copied {
                        e.status = DiffStatus::Added;
                        e.old_path = None;
                        e.score = None;
                        e.old_mode = "000000".to_owned();
                        e.old_oid = zero_oid();
                    }
                }
            }
            for path in unmerged_ref {
                diff_entries.push(diff_entry_unmerged(path));
            }
            diff_entries.sort_by(|a, b| a.path().cmp(b.path()));
        }
    }

    // Compute cwd-relative prefix for --relative
    let rel_prefix = if options.relative {
        if let Some(wt) = &repo.work_tree {
            let cwd = std::env::current_dir().unwrap_or_default();
            if let Ok(rel) = cwd.strip_prefix(wt) {
                let s = rel.to_string_lossy().to_string();
                if s.is_empty() {
                    String::new()
                } else {
                    format!("{s}/")
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Apply --relative: filter and strip prefix
    let diff_entries: Vec<DiffEntry> = if !rel_prefix.is_empty() {
        diff_entries
            .into_iter()
            .filter_map(|mut e| {
                let path = e.path().to_owned();
                if !path.starts_with_bytes(rel_prefix.as_bytes()) {
                    return None;
                }
                let stripped =
                    RepoPathBuf::from_bytes(path.as_bytes()[rel_prefix.len()..].to_vec());
                if e.old_path.is_some() {
                    e.old_path = Some(stripped.clone());
                }
                if e.new_path.is_some() {
                    e.new_path = Some(stripped);
                }
                Some(e)
            })
            .collect()
    } else {
        diff_entries
    };

    // `--diff-filter=<letters>`: include uppercase status letters, exclude lowercase ones
    // (e.g. `u` excludes unmerged `U`). Mirrors `git diff`'s diff-filter (t3701 reset -p check).
    let diff_entries: Vec<DiffEntry> = if let Some(ref df) = options.diff_filter {
        apply_diff_index_diff_filter(diff_entries, df)
    } else {
        diff_entries
    };

    if options.check {
        let merged_attrs = match load_gitattributes_for_diff(&repo) {
            Ok(a) => a,
            Err(grit_lib::error::Error::InvalidRef(msg))
                if msg.starts_with("bad --attr-source") =>
            {
                eprintln!("fatal: bad --attr-source or GIT_ATTR_SOURCE");
                std::process::exit(128);
            }
            Err(e) => return Err(e.into()),
        };
        let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let ignore_case = config
            .get("core.ignorecase")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
        let wt_for_attrs = if options.cached {
            None
        } else {
            repo.work_tree.as_deref()
        };
        let mut stdout = std::io::stdout().lock();
        let has_ws = check_whitespace_errors(
            &mut stdout,
            &diff_entries,
            &repo.odb,
            wt_for_attrs,
            &merged_attrs,
            ignore_case,
            &config,
        )?;
        if has_ws {
            if options.exit_code {
                std::process::exit(3);
            }
            std::process::exit(2);
        }
        if options.exit_code || options.quiet {
            if !diff_entries.is_empty() {
                std::process::exit(1);
            }
            return Ok(());
        }
        return Ok(());
    }

    if !options.quiet {
        if options.stat {
            write_diff_index_stat(&diff_entries, &repo.odb)?;
        } else if options.numstat {
            write_diff_index_numstat(&diff_entries, &repo.odb)?;
        } else if options.name_only {
            let term = if options.nul_terminated { b'\0' } else { b'\n' };
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for entry in &diff_entries {
                if options.nul_terminated {
                    out.write_all(entry.path().as_bytes())?;
                } else {
                    write!(
                        out,
                        "{}",
                        quote_c_style(&entry.path().to_string_lossy(), quote_fully)
                    )?;
                }
                out.write_all(&[term])?;
            }
        } else if options.patch {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let wt = repo.work_tree.as_deref();
            let sm_ignore = SubmoduleIgnoreFlags {
                ignore_all: options.ignore_submodules_all,
                ignore_untracked: options.ignore_untracked_in_submodules,
                ignore_dirty: options.ignore_dirty_submodules,
            };
            for entry in &diff_entries {
                let p = entry.path().to_string_lossy();
                write_patch_entry(
                    &mut out,
                    &repo,
                    &repo.odb,
                    entry,
                    options.context_lines,
                    wt,
                    options.submodule_format,
                    sm_ignore,
                    &p,
                    options.indent_heuristic,
                )?;
            }
        } else if options.name_status {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            write_diff_index_name_status(
                &mut out,
                &diff_entries,
                quote_fully,
                options.nul_terminated,
            )?;
        } else {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for entry in &diff_entries {
                if options.nul_terminated {
                    write_raw_diff_entry_z(
                        &mut out,
                        entry,
                        &repo,
                        &index,
                        options.abbrev,
                        !options.cached,
                    )?;
                } else {
                    let line = render_raw_diff_entry(
                        entry,
                        &repo,
                        &index,
                        options.abbrev,
                        !options.cached,
                        quote_fully,
                    )?;
                    writeln!(out, "{line}")?;
                }
            }
        }
    }

    if (options.exit_code || options.quiet) && !diff_entries.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn filter_entries_ignore_space_change(
    repo: &Repository,
    entries: Vec<DiffEntry>,
) -> Vec<DiffEntry> {
    entries
        .into_iter()
        .filter(|entry| {
            if entry.status == DiffStatus::Added
                || entry.status == DiffStatus::Deleted
                || entry.old_mode != entry.new_mode
            {
                return true;
            }
            let (old_raw, new_raw) = read_entry_raw_contents(repo, entry);
            if is_binary_content(&old_raw) || is_binary_content(&new_raw) {
                return true;
            }
            let old = String::from_utf8_lossy(&old_raw).into_owned();
            let new = String::from_utf8_lossy(&new_raw).into_owned();
            normalize_ignore_space_change(&old) != normalize_ignore_space_change(&new)
        })
        .collect()
}

fn filter_entries_ignore_all_space(repo: &Repository, entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
    entries
        .into_iter()
        .filter(|entry| {
            if entry.status == DiffStatus::Added
                || entry.status == DiffStatus::Deleted
                || entry.old_mode != entry.new_mode
            {
                return true;
            }
            let (old_raw, new_raw) = read_entry_raw_contents(repo, entry);
            if is_binary_content(&old_raw) || is_binary_content(&new_raw) {
                return true;
            }
            let old = String::from_utf8_lossy(&old_raw).into_owned();
            let new = String::from_utf8_lossy(&new_raw).into_owned();
            normalize_ignore_all_space(&old) != normalize_ignore_all_space(&new)
        })
        .collect()
}

fn read_entry_raw_contents(repo: &Repository, entry: &DiffEntry) -> (Vec<u8>, Vec<u8>) {
    let old_raw = read_blob_raw(&repo.odb, &entry.old_oid);
    let new_raw = if entry.new_oid == zero_oid()
        && entry.status != DiffStatus::Deleted
        && worktree_side_is_placeholder(repo, entry)
    {
        if let Some(wt) = repo.work_tree.as_ref() {
            let path = entry.new_path.as_deref().unwrap_or(entry.path());
            read_worktree_path_raw(&path.to_fs_path(wt))
        } else {
            Vec::new()
        }
    } else {
        read_blob_raw(&repo.odb, &entry.new_oid)
    };
    (old_raw, new_raw)
}

fn worktree_side_is_placeholder(repo: &Repository, entry: &DiffEntry) -> bool {
    if !matches!(entry.status, DiffStatus::Modified | DiffStatus::TypeChanged) {
        return false;
    }

    let Some(wt) = repo.work_tree.as_ref() else {
        return false;
    };
    let path = entry.new_path.as_deref().unwrap_or(entry.path());
    let abs = path.to_fs_path(wt);
    match fs::symlink_metadata(&abs) {
        Ok(meta) => {
            let mode = canonicalize_mode(meta.permissions().mode());
            mode == u32::from_str_radix(&entry.new_mode, 8).unwrap_or(MODE_REGULAR)
        }
        Err(_) => false,
    }
}

fn is_binary_content(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

fn normalize_ignore_space_change(content: &str) -> String {
    content
        .lines()
        .map(normalize_ignore_space_change_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_ignore_all_space(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_ignore_space_change_line(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut in_space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !in_space {
                normalized.push(' ');
                in_space = true;
            }
        } else {
            normalized.push(c);
            in_space = false;
        }
    }
    normalized.trim_end().to_owned()
}

/// Apply `--diff-filter` like `git diff` (`apply_diff_filter`): uppercase letters select a status,
/// lowercase letters exclude it. With at least one uppercase letter the filter is an allow-list;
/// otherwise everything not excluded passes.
fn apply_diff_index_diff_filter(entries: Vec<DiffEntry>, filter: &str) -> Vec<DiffEntry> {
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

#[derive(Debug, Clone)]
struct Options {
    tree_ish: String,
    merge_base: bool,
    pathspecs: Vec<String>,
    cached: bool,
    match_missing: bool,
    /// When true, omit gitlink paths entirely (like Git `ignore_submodules`).
    ignore_submodules_all: bool,
    /// When true, do not report untracked-only dirtiness inside submodules.
    ignore_untracked_in_submodules: bool,
    /// When true, skip submodule dirty detection (no "contains …" lines; no worktree diff inside submodule).
    ignore_dirty_submodules: bool,
    /// Submodule patch format when `-p` and `--submodule` are used.
    submodule_format: SubmodulePatchFormat,
    quiet: bool,
    exit_code: bool,
    abbrev: Option<usize>,
    find_renames: Option<u32>,
    find_copies: bool,
    find_copies_harder: bool,
    patch: bool,
    name_status: bool,
    name_only: bool,
    stat: bool,
    numstat: bool,
    context_lines: usize,
    ignore_space_change: bool,
    ignore_all_space: bool,
    nul_terminated: bool,
    relative: bool,
    check: bool,
    /// `--diff-filter=<letters>`: include uppercase status letters, exclude lowercase (Git's
    /// `diff_filter`). `u`/`U` selects unmerged entries.
    diff_filter: Option<String>,
    indent_heuristic: bool,
    /// Git `-B` / `--break-rewrites`: split large in-place edits before rename/copy, then merge
    /// surviving pairs (see `diffcore-break.c`).
    break_rewrites: bool,
    /// Internal break threshold (0–[`GIT_DIFF_MAX_SCORE`]); default matches Git `DEFAULT_BREAK_SCORE`.
    break_score: u64,
    /// Internal merge-back threshold after break (high 16 bits of Git's `break_opt`).
    merge_after_break_score: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    mode: u32,
    oid: ObjectId,
}

impl Snapshot {
    fn from_index_entry(mode: u32, oid: ObjectId) -> Self {
        Self {
            mode: canonicalize_mode(mode),
            oid,
        }
    }
}

fn blobish_object_kind(mode: u32) -> bool {
    let t = mode & 0o170_000;
    // Regular and executable files share the `0o100000` object type; symlinks use `0o120000`.
    t == 0o100_000 || t == 0o120_000
}

fn typechange_between_snapshots(old: Snapshot, new_mode: u32) -> bool {
    (old.mode & 0o170_000) != (new_mode & 0o170_000)
}

#[derive(Debug, Clone)]
struct RawChange {
    path: String,
    status: char,
    old: Option<Snapshot>,
    new: Option<Snapshot>,
}

/// Submodule ignore flags mirroring Git's `handle_ignore_submodules_arg`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubmoduleIgnoreFlags {
    pub(crate) ignore_all: bool,
    pub(crate) ignore_untracked: bool,
    pub(crate) ignore_dirty: bool,
}

/// How `diff-index -p` formats gitlink (submodule) entries when `--submodule` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SubmodulePatchFormat {
    /// Default: `Subproject commit` unified hunks only (same as no `--submodule`).
    #[default]
    Short,
    /// `Submodule path old..new:` with commit subjects (`git diff --submodule=log`).
    Log,
    /// Recursive unified diff inside the submodule (`--submodule=diff`).
    Diff,
}

fn parse_options(argv: &[String]) -> Result<Options> {
    let mut cached = false;
    let mut match_missing = false;
    let mut ignore_submodules_all = false;
    // Match Git: `diff-index` defaults to ignoring untracked files inside submodules
    // unless `--ignore-submodules=none` is passed (see t4060).
    let mut ignore_untracked_in_submodules = true;
    let mut ignore_dirty_submodules = false;
    let mut submodule_format = SubmodulePatchFormat::Short;
    let mut quiet = false;
    let mut exit_code = false;
    let mut abbrev: Option<usize> = None;
    let mut tree_ish: Option<String> = None;
    let mut merge_base = false;
    let mut pathspecs = Vec::new();
    let mut end_of_options = false;
    let mut find_renames: Option<u32> = None;
    let mut find_copies = false;
    let mut find_copies_harder = false;
    let mut c_count = 0u32;
    let mut patch = false;
    let mut name_status = false;
    let mut name_only = false;
    let mut stat = false;
    let mut numstat = false;
    let mut context_lines: usize = diff_context_from_env().unwrap_or(3);
    let mut ignore_space_change = false;
    let mut ignore_all_space = false;
    let mut nul_terminated = false;
    let mut relative = false;
    let mut check = false;
    let mut diff_filter: Option<String> = None;
    let mut break_rewrites = false;
    let mut break_score: u64 = grit_lib::diff::GIT_DIFF_DEFAULT_BREAK_SCORE;
    let mut merge_after_break_score: u64 = GIT_DIFF_DEFAULT_MERGE_SCORE_AFTER_BREAK;

    let mut idx = 0usize;
    while idx < argv.len() {
        let arg = &argv[idx];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            idx += 1;
            continue;
        }
        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "--cached" => cached = true,
                "--merge-base" => merge_base = true,
                "-m" => match_missing = true,
                "--quiet" => quiet = true,
                "--exit-code" => exit_code = true,
                "--raw" => {}
                "--abbrev" => abbrev = Some(7),
                "-p" | "--patch" | "-u" => {
                    patch = true;
                }
                "--name-status" => {
                    name_status = true;
                }
                "--name-only" => {
                    name_only = true;
                }
                "--stat" => {
                    stat = true;
                }
                "--numstat" => {
                    numstat = true;
                }
                "-B" => {
                    break_rewrites = true;
                }
                "--break-rewrites" => {
                    break_rewrites = true;
                }
                _ if arg.starts_with("--break-rewrites=") => {
                    break_rewrites = true;
                    let rest = arg.trim_start_matches("--break-rewrites=");
                    if !rest.is_empty() {
                        let (b_part, m_part) = rest
                            .split_once('/')
                            .map(|(a, b)| (a, Some(b)))
                            .unwrap_or((rest, None));
                        break_score = parse_diff_rename_score_token(b_part)
                            .with_context(|| format!("invalid --break-rewrites value: `{rest}`"))?;
                        if let Some(m) = m_part {
                            if m.is_empty() {
                                bail!("invalid --break-rewrites value: `{rest}`");
                            }
                            merge_after_break_score = parse_diff_rename_score_token(m)
                                .with_context(|| {
                                    format!("invalid --break-rewrites value: `{rest}`")
                                })?;
                        }
                    }
                }
                _ if arg.starts_with("-B") && arg.len() > 2 => {
                    break_rewrites = true;
                    let rest = &arg[2..];
                    let (b_part, m_part) = rest
                        .split_once('/')
                        .map(|(a, b)| (a, Some(b)))
                        .unwrap_or((rest, None));
                    break_score = parse_diff_rename_score_token(b_part)
                        .with_context(|| format!("invalid -B value: `{arg}`"))?;
                    if let Some(m) = m_part {
                        if m.is_empty() {
                            bail!("invalid -B value: `{arg}`");
                        }
                        merge_after_break_score = parse_diff_rename_score_token(m)
                            .with_context(|| format!("invalid -B value: `{arg}`"))?;
                    }
                }
                "-M" | "--find-renames" => {
                    find_renames = Some(50);
                }
                "--no-renames" => {
                    find_renames = None;
                }
                _ if arg.starts_with("-M") => {
                    let val = &arg[2..];
                    let pct = if val.ends_with('%') {
                        val[..val.len() - 1].parse::<u32>().unwrap_or(50)
                    } else {
                        val.parse::<u32>().unwrap_or(50)
                    };
                    find_renames = Some(pct);
                }
                _ if arg.starts_with("--find-renames=") => {
                    let val = &arg["--find-renames=".len()..];
                    let pct = if val.ends_with('%') {
                        val[..val.len() - 1].parse::<u32>().unwrap_or(50)
                    } else {
                        val.parse::<u32>().unwrap_or(50)
                    };
                    find_renames = Some(pct);
                }
                _ if arg.starts_with("-l") && arg[2..].parse::<usize>().is_ok() => {
                    // rename limit - accept and ignore for now
                }
                "-r" => {
                    // recursive - default behavior for diff-index
                }
                _ if arg.starts_with("--max-depth=") => {
                    let val = &arg["--max-depth=".len()..];
                    let parsed = val
                        .parse::<i32>()
                        .with_context(|| format!("invalid --max-depth value: `{val}`"))?;
                    if parsed != -1 {
                        bail!("unsupported option: {arg}");
                    }
                }
                _ if arg.starts_with("-U") && arg[2..].parse::<usize>().is_ok() => {
                    if let Ok(n) = arg[2..].parse::<usize>() {
                        context_lines = n;
                    }
                }
                "-b" | "--ignore-space-change" => {
                    ignore_space_change = true;
                }
                "-w" | "--ignore-all-space" => {
                    ignore_all_space = true;
                }
                _ if arg.starts_with("--unified=") => {
                    context_lines = arg["--unified=".len()..].parse::<usize>().unwrap_or(3);
                }
                "-z" => {
                    nul_terminated = true;
                }
                "--relative" => {
                    relative = true;
                }
                _ if arg.starts_with("--relative=") => {
                    relative = true;
                    // Ignore the =<path> variant for now
                }
                "-C" | "--find-copies" => {
                    c_count += 1;
                    find_copies = true;
                    if c_count >= 2 {
                        find_copies_harder = true;
                    }
                    if find_renames.is_none() {
                        find_renames = Some(50);
                    }
                }
                "--find-copies-harder" => {
                    find_copies = true;
                    find_copies_harder = true;
                    if find_renames.is_none() {
                        find_renames = Some(50);
                    }
                }
                _ if arg.starts_with("--abbrev=") => {
                    let value = arg.trim_start_matches("--abbrev=");
                    let parsed = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid --abbrev value: `{value}`"))?;
                    abbrev = Some(parsed);
                }
                "--check" => {
                    check = true;
                }
                _ if arg.starts_with("--diff-filter=") => {
                    diff_filter = Some(arg.trim_start_matches("--diff-filter=").to_owned());
                }
                "--ignore-submodules" => {
                    ignore_submodules_all = true;
                }
                _ if arg.starts_with("--ignore-submodules=") => {
                    let val = arg.trim_start_matches("--ignore-submodules=");
                    match val {
                        "all" => {
                            ignore_submodules_all = true;
                            ignore_untracked_in_submodules = false;
                            ignore_dirty_submodules = false;
                        }
                        "untracked" => {
                            ignore_untracked_in_submodules = true;
                            ignore_submodules_all = false;
                        }
                        "dirty" => {
                            ignore_dirty_submodules = true;
                            ignore_submodules_all = false;
                        }
                        "none" => {
                            ignore_submodules_all = false;
                            ignore_untracked_in_submodules = false;
                            ignore_dirty_submodules = false;
                        }
                        _ => bail!("unsupported option: {arg}"),
                    };
                }
                "--submodule" => {
                    submodule_format = SubmodulePatchFormat::Log;
                }
                _ if arg.starts_with("--submodule=") => {
                    let val = arg.trim_start_matches("--submodule=");
                    submodule_format = match val {
                        "short" => SubmodulePatchFormat::Short,
                        "log" => SubmodulePatchFormat::Log,
                        "diff" => SubmodulePatchFormat::Diff,
                        _ => bail!("unsupported option: {arg}"),
                    };
                }
                "--indent-heuristic" | "--no-indent-heuristic" => {}
                _ => bail!("unsupported option: {arg}"),
            }
            idx += 1;
            continue;
        }

        if tree_ish.is_none() {
            tree_ish = Some(arg.clone());
        } else {
            pathspecs.push(arg.clone());
        }
        idx += 1;
    }

    let Some(tree_ish) = tree_ish else {
        bail!("usage: grit diff-index [-m] [--cached] [--raw] [--quiet] [--exit-code] [--abbrev[=<n>]] <tree-ish> [<path>...]");
    };

    let (cli_ind, cli_no) = parse_indent_heuristic_cli_flags(argv);
    let indent_heuristic = resolve_indent_heuristic(
        &ConfigSet::load(None, true).unwrap_or_default(),
        cli_ind,
        cli_no,
    );

    Ok(Options {
        tree_ish,
        merge_base,
        pathspecs,
        cached,
        match_missing,
        ignore_submodules_all,
        ignore_untracked_in_submodules,
        ignore_dirty_submodules,
        submodule_format,
        quiet,
        exit_code,
        abbrev,
        find_renames,
        find_copies,
        find_copies_harder,
        patch,
        name_status,
        name_only,
        stat,
        numstat,
        context_lines,
        ignore_space_change,
        ignore_all_space,
        nul_terminated,
        relative,
        check,
        diff_filter,
        indent_heuristic,
        break_rewrites,
        break_score,
        merge_after_break_score,
    })
}

/// Read diff context lines from `GIT_DIFF_OPTS` when provided.
///
/// We currently honor `--unified=<n>`, `-U<n>`, and `-u<n>` forms.
/// Non-context tokens are ignored.
fn diff_context_from_env() -> Option<usize> {
    let opts = std::env::var("GIT_DIFF_OPTS").ok()?;
    let mut result = None;
    for token in opts.split_whitespace() {
        if let Some(v) = token.strip_prefix("--unified=") {
            if let Ok(parsed) = v.parse::<usize>() {
                result = Some(parsed);
            }
            continue;
        }
        if let Some(v) = token.strip_prefix("-U") {
            if !v.is_empty() {
                if let Ok(parsed) = v.parse::<usize>() {
                    result = Some(parsed);
                }
            }
            continue;
        }
        if let Some(v) = token.strip_prefix("-u") {
            if !v.is_empty() {
                if let Ok(parsed) = v.parse::<usize>() {
                    result = Some(parsed);
                }
            }
        }
    }
    result
}

/// Resolve a revision to a tree OID without redundantly reading the tree object.
/// Returns the tree OID which can be passed directly to collect_tree_entries.
fn resolve_tree_ish(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision(repo, spec)?;
    resolve_tree_ish_from_oid(repo, oid)
}

fn resolve_tree_ish_from_commit_oid(repo: &Repository, commit_oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(&commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        bail!(
            "fatal: {} is a {}, not a commit",
            commit_oid.to_hex(),
            obj.kind.as_str()
        );
    }
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

fn peel_tag_chain_to_oid(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Tag {
            return Ok(oid);
        }
        let tag = parse_tag(&obj.data)?;
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

fn resolve_tree_ish_from_oid(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    oid = peel_tag_chain_to_oid(repo, oid)?;
    let obj = repo.odb.read(&oid)?;
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let commit = parse_commit(&obj.data)?;
            Ok(commit.tree)
        }
        _ => bail!(
            "fatal: {} is a {}, not a commit",
            oid.to_hex(),
            obj.kind.as_str()
        ),
    }
}

fn collect_tree_entries(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut BTreeMap<String, Snapshot>,
) -> Result<()> {
    let obj = repo.odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        bail!("expected tree object");
    }
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8(entry.name)
            .map_err(|_| anyhow::anyhow!("tree contains non-UTF-8 path"))?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            collect_tree_entries(repo, &entry.oid, &path, out)?;
        } else {
            out.insert(path, Snapshot::from_index_entry(entry.mode, entry.oid));
        }
    }
    Ok(())
}

/// Returns true when the work tree at `path` matches `index_snapshot` (stat cache or content hash).
fn worktree_matches_index_snapshot(
    repo: &Repository,
    work_tree: &Path,
    path: &str,
    index_snapshot: Snapshot,
    index_entries: &BTreeMap<&[u8], &IndexEntry>,
) -> Result<bool> {
    if index_snapshot.mode == MODE_GITLINK {
        return Ok(true);
    }
    let abs = work_tree.join(path);
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if let Some(ie) = index_entries.get(path.as_bytes()) {
        if stat_matches(ie, &meta) {
            return Ok(true);
        }
    }
    let Some(wt_snapshot) = read_worktree_snapshot_from_meta(repo, &abs, &meta)? else {
        return Ok(false);
    };
    Ok(wt_snapshot == index_snapshot)
}

fn collect_unmerged_index_paths(index: &Index) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in &index.entries {
        if e.stage() != 0 {
            out.insert(String::from_utf8_lossy(&e.path).into_owned());
        }
    }
    out
}

fn diff_entry_unmerged(path: &str) -> DiffEntry {
    DiffEntry {
        status: DiffStatus::Unmerged,
        // TODO(byte-paths): caller passes a lossy &str (Phase 2/4)
        old_path: Some(RepoPathBuf::from_string(path.to_owned())),
        new_path: Some(RepoPathBuf::from_string(path.to_owned())),
        old_mode: "000000".to_owned(),
        new_mode: "000000".to_owned(),
        old_oid: zero_oid(),
        new_oid: zero_oid(),
        score: None,
    }
}

fn diff_tree_vs_index(
    tree_map: &BTreeMap<String, Snapshot>,
    index_map: &BTreeMap<String, Snapshot>,
) -> Vec<RawChange> {
    let mut all_paths = BTreeSet::new();
    all_paths.extend(tree_map.keys().cloned());
    all_paths.extend(index_map.keys().cloned());

    let mut changes = Vec::new();
    for path in all_paths {
        let old = tree_map.get(&path).copied();
        let new = index_map.get(&path).copied();
        match (old, new) {
            (Some(old), Some(new)) if old == new => {}
            (Some(old), Some(new)) => {
                let old_type = old.mode & 0o170_000;
                let new_type = new.mode & 0o170_000;
                let is_blob_pair = blobish_object_kind(old.mode) && blobish_object_kind(new.mode);
                let status = if is_blob_pair && old_type != new_type {
                    'T'
                } else {
                    'M'
                };
                changes.push(RawChange {
                    path,
                    status,
                    old: Some(old),
                    new: Some(new),
                });
            }
            (Some(old), None) => changes.push(RawChange {
                path,
                status: 'D',
                old: Some(old),
                new: None,
            }),
            (None, Some(new)) => changes.push(RawChange {
                path,
                status: 'A',
                old: None,
                new: Some(new),
            }),
            (None, None) => {}
        }
    }
    changes
}

fn diff_tree_vs_worktree(
    repo: &Repository,
    tree_map: &BTreeMap<String, Snapshot>,
    index_map: &BTreeMap<String, Snapshot>,
    index: &Index,
    match_missing: bool,
    submodule_ignore: SubmoduleIgnoreFlags,
) -> Result<Vec<RawChange>> {
    let Some(work_tree) = &repo.work_tree else {
        bail!("this operation must be run in a work tree");
    };

    // Build a lookup from path → index entry for stat cache checks
    let index_entries: BTreeMap<&[u8], &IndexEntry> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| (e.path.as_slice(), e))
        .collect();

    let mut merged = BTreeMap::new();
    for change in diff_tree_vs_index(tree_map, index_map) {
        merged.insert(change.path.clone(), change);
    }

    // Git `diff-index` without `--cached` compares the tree to the **working tree** for patch/raw
    // purposes, while still classifying tree↔index conflicts. When the index differs from the tree,
    // the recorded "new" side is the index blob if the work tree still matches the index; if the
    // work tree has diverged further, the new side is the work tree (placeholder zero OID in raw,
    // content read from disk for `-p`). See t9231-diff-index-patch and `git diff-index -p HEAD`.
    let mut remove_merged_paths = Vec::new();
    for change in merged.values_mut() {
        if !matches!(change.status, 'A' | 'M') {
            continue;
        }
        let Some(index_snapshot) = change.new else {
            continue;
        };
        if index_snapshot.mode == MODE_GITLINK {
            continue;
        }
        let path = change.path.as_str();
        // Skip-worktree (and assume-unchanged): never refresh the recorded "new" side from disk.
        // Git keeps the index blob OID in raw output even when the work tree differs (t7011).
        if let Some(ie) = index_entries.get(path.as_bytes()) {
            if ie.skip_worktree() || ie.assume_unchanged() {
                continue;
            }
        }
        if worktree_matches_index_snapshot(repo, work_tree, path, index_snapshot, &index_entries)? {
            continue;
        }
        let abs = work_tree.join(path);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let Some(wt_snapshot) = read_worktree_snapshot_from_meta(repo, &abs, &meta)? else {
            if change.status == 'A' && meta.is_dir() {
                remove_merged_paths.push(change.path.clone());
            }
            continue;
        };
        if let Some(tree_side) = change.old {
            if typechange_between_snapshots(tree_side, wt_snapshot.mode) {
                change.status = 'T';
            }
        }
        // For a gitlink the resolved submodule HEAD is the right side (used by `--submodule`
        // patch rendering); raw plumbing re-zeros it. Regular blobs keep the zero placeholder and
        // read their worktree content lazily at patch time.
        let new_oid = if wt_snapshot.mode == MODE_GITLINK {
            wt_snapshot.oid
        } else {
            zero_oid()
        };
        change.new = Some(Snapshot {
            mode: wt_snapshot.mode,
            oid: new_oid,
        });
    }
    for path in remove_merged_paths {
        merged.remove(&path);
    }

    for (path, index_snapshot) in index_map {
        // Match Git: `diff-index` without `--cached` reports tree↔index differences first.
        // Compare index to worktree when:
        // - Tree and index agree for this path (t1501: tree≠index stays as tree↔index only), or
        // - The path is not in the tree but is in the index (staged add): chmod/content drift in
        //   the work tree must update the recorded snapshot so rename output shows `new mode`
        //   (t3300-funny-names).
        let tree_snap = tree_map.get(path).copied();
        let compare_worktree = match tree_snap {
            Some(ts) => ts == *index_snapshot,
            None => true,
        };
        if !compare_worktree {
            continue;
        }

        let abs = work_tree.join(path);
        if path_has_symlink_parent(work_tree, &abs) {
            if match_missing {
                continue;
            }
            let old = tree_map.get(path).copied().or(Some(*index_snapshot));
            merged.insert(
                path.clone(),
                RawChange {
                    path: path.clone(),
                    status: 'D',
                    old,
                    new: None,
                },
            );
            continue;
        }

        // Git `diff-lib.c:do_oneway_diff`: do not examine the work tree for skip-worktree or
        // assume-unchanged entries.
        if let Some(ie) = index_entries.get(path.as_bytes()) {
            if ie.skip_worktree() || ie.assume_unchanged() {
                continue;
            }
        }

        if index_snapshot.mode == MODE_GITLINK {
            if submodule_ignore.ignore_all {
                continue;
            }
            let sub_head = read_submodule_head_oid(&abs);
            // A gitlink whose worktree directory no longer exists is a deleted submodule: Git's
            // `diff-lib.c` reports it as a deletion (`D`, new mode 000000). `--submodule=log`/`diff`
            // still render the `(submodule deleted)` summary from the deleted gitlink; `--submodule=
            // short` renders the `deleted file mode 160000` / `-Subproject commit` block (t4041 #44).
            if sub_head.is_none() && !abs.exists() {
                let old = tree_map.get(path).copied().or(Some(*index_snapshot));
                merged.insert(
                    path.clone(),
                    RawChange {
                        path: path.clone(),
                        status: 'D',
                        old,
                        new: None,
                    },
                );
                continue;
            }
            // Gitlink in the index, but a regular file/symlink now sits at the path: this is a
            // submodule→blob typechange. Report `T` with the worktree blob on the new side so
            // `--submodule` rendering emits the `(submodule deleted)` summary (with inner deleted
            // files via the absorbed gitdir) followed by the blob add (t4060 #46).
            if sub_head.is_none() {
                if let Ok(meta) = fs::symlink_metadata(&abs) {
                    if meta.file_type().is_file() || meta.file_type().is_symlink() {
                        if let Some(wt_snapshot) =
                            read_worktree_snapshot_from_meta(repo, &abs, &meta)?
                        {
                            let old = tree_map.get(path).copied().or(Some(*index_snapshot));
                            // Keep the real worktree blob hash on the new side so the typechange's
                            // blob-add half shows `index 0000000..<hash>`; the content itself is read
                            // lazily from the work tree at patch time (blob not in the ODB).
                            merged.insert(
                                path.clone(),
                                RawChange {
                                    path: path.clone(),
                                    status: 'T',
                                    old,
                                    new: Some(wt_snapshot),
                                },
                            );
                        }
                    }
                }
                continue;
            }
            let tree_snap = tree_map.get(path).copied();
            let tree_matches_index = tree_snap.is_some_and(|t| t == *index_snapshot);
            let head_differs_from_index = sub_head.as_ref() != Some(&index_snapshot.oid);
            let dirty = submodule_dirty_flags(
                work_tree,
                path,
                &index_snapshot.oid,
                submodule_ignore.ignore_untracked,
                submodule_ignore.ignore_dirty,
            );
            let report_dirty_aligned = tree_matches_index
                && !head_differs_from_index
                && (dirty.modified || dirty.untracked);
            if head_differs_from_index || report_dirty_aligned {
                let old = tree_map.get(path).copied().or(Some(*index_snapshot));
                let new_oid = sub_head.unwrap_or_else(zero_oid);
                merged.insert(
                    path.clone(),
                    RawChange {
                        path: path.clone(),
                        status: 'M',
                        old,
                        new: Some(Snapshot {
                            mode: MODE_GITLINK,
                            oid: new_oid,
                        }),
                    },
                );
            }
            continue;
        }

        // Fast path: use stat cache to skip unchanged files
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if match_missing {
                    continue;
                }
                let old = tree_map.get(path).copied().or(Some(*index_snapshot));
                merged.insert(
                    path.clone(),
                    RawChange {
                        path: path.clone(),
                        status: 'D',
                        old,
                        new: None,
                    },
                );
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        // Check stat cache — if stat matches index entry, file is unchanged
        if let Some(ie) = index_entries.get(path.as_bytes()) {
            if stat_matches(ie, &meta) {
                continue; // Fast path: stat data matches, skip hashing
            }
        }

        // Stat differs — must read and hash the file
        match read_worktree_snapshot_from_meta(repo, &abs, &meta)? {
            Some(worktree_snapshot) => {
                if worktree_snapshot == *index_snapshot {
                    if worktree_snapshot.mode == MODE_SYMLINK {
                        continue;
                    }
                    // Index stat cache is stale (e.g. after `read-tree` zeroed stat fields) while
                    // tree, index OID, and work tree content still agree — Git reports `M` with a
                    // zero OID on the work-tree side (`t3700-add.sh` refresh tests).
                    if let Some(ts) = tree_snap {
                        if ts == *index_snapshot {
                            let wt_placeholder = Snapshot {
                                mode: worktree_snapshot.mode,
                                oid: zero_oid(),
                            };
                            merged.insert(
                                path.clone(),
                                RawChange {
                                    path: path.clone(),
                                    status: 'M',
                                    old: Some(ts),
                                    new: Some(wt_placeholder),
                                },
                            );
                        }
                    }
                } else if worktree_snapshot != *index_snapshot {
                    if tree_snap.is_none() {
                        // Staged add only in the index: refresh the `new` snapshot from the work
                        // tree (e.g. `chmod` after `git add`) while keeping status `A` so rename
                        // detection still pairs the delete with this add (t3300-funny-names).
                        merged.insert(
                            path.clone(),
                            RawChange {
                                path: path.clone(),
                                status: 'A',
                                old: None,
                                new: Some(worktree_snapshot),
                            },
                        );
                    } else {
                        let old = tree_map.get(path).copied().or(Some(*index_snapshot));
                        let old_tree_side = tree_snap.unwrap_or(*index_snapshot);
                        let status = if typechange_between_snapshots(
                            old_tree_side,
                            worktree_snapshot.mode,
                        ) {
                            'T'
                        } else {
                            'M'
                        };
                        // Use zero OID for worktree side — the blob is not
                        // in the object database, matching git's behaviour.
                        // Exception: a gitlink (submodule directory checked out where the index
                        // records a blob) keeps its resolved submodule HEAD so `--submodule`
                        // patch rendering shows the real right-side commit (blob→submodule
                        // typechange, t4060 #17). Raw plumbing re-zeros it elsewhere.
                        let wt_placeholder = Snapshot {
                            mode: worktree_snapshot.mode,
                            oid: if worktree_snapshot.mode == MODE_GITLINK {
                                worktree_snapshot.oid
                            } else {
                                zero_oid()
                            },
                        };
                        merged.insert(
                            path.clone(),
                            RawChange {
                                path: path.clone(),
                                status,
                                old,
                                new: Some(wt_placeholder),
                            },
                        );
                    }
                }
            }
            None => {
                // Not a regular file or symlink — treat as missing
                if !match_missing {
                    if let Some(old) = tree_map.get(path).copied() {
                        merged.insert(
                            path.clone(),
                            RawChange {
                                path: path.clone(),
                                status: 'D',
                                old: Some(old),
                                new: None,
                            },
                        );
                    }
                }
            }
        }
    }

    Ok(merged.into_values().collect())
}

fn read_worktree_snapshot_from_meta(
    _repo: &Repository,
    abs_path: &Path,
    metadata: &fs::Metadata,
) -> Result<Option<Snapshot>> {
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(abs_path)?;
        let oid = Odb::hash_object_data(ObjectKind::Blob, target.as_os_str().as_bytes());
        return Ok(Some(Snapshot {
            mode: MODE_SYMLINK,
            oid,
        }));
    }

    if metadata.file_type().is_file() {
        let mode = canonicalize_mode(metadata.permissions().mode());
        let data = fs::read(abs_path)?;
        let oid = Odb::hash_object_data(ObjectKind::Blob, &data);
        return Ok(Some(Snapshot { mode, oid }));
    }

    // Directory where the index expects a file (e.g. submodule checkout at `path/` while index
    // still records a blob from a parent commit). Treat as mismatch without reading the path as a
    // file (`EISDIR`); matches Git and unblocks `merge` pre-checks (`t6437`).
    if metadata.is_dir() {
        if abs_path.join(".git").exists() {
            // A populated submodule checkout: resolve its HEAD so `--submodule` patch output shows
            // the real right-side commit. Raw plumbing re-zeros this via
            // `raw_gitlink_new_is_worktree_zero`, matching Git.
            let oid = read_submodule_head_oid(abs_path).unwrap_or_else(zero_oid);
            return Ok(Some(Snapshot {
                mode: MODE_GITLINK,
                oid,
            }));
        }
        return Ok(None);
    }

    Ok(None)
}

fn path_has_symlink_parent(work_tree: &Path, abs_path: &Path) -> bool {
    let Ok(rel) = abs_path.strip_prefix(work_tree) else {
        return false;
    };
    let mut cur = work_tree.to_path_buf();
    let mut comps = rel.components().peekable();
    while let Some(component) = comps.next() {
        if comps.peek().is_none() {
            break;
        }
        cur.push(component.as_os_str());
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn canonicalize_mode(raw_mode: u32) -> u32 {
    match raw_mode & 0o170000 {
        0o120000 => MODE_SYMLINK,
        0o160000 => MODE_GITLINK,
        0o100000 => {
            if raw_mode & 0o111 != 0 {
                MODE_EXECUTABLE
            } else {
                MODE_REGULAR
            }
        }
        _ => MODE_REGULAR,
    }
}

fn entry_matches_pathspecs(
    path: &str,
    pathspecs: &[String],
    ctx: grit_lib::pathspec::PathspecMatchContext,
) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list_with_context(path, pathspecs, ctx)
}

fn parse_mode_octal(mode: &str) -> Option<u32> {
    u32::from_str_radix(mode, 8).ok()
}

fn is_break_eligible_blob_mode(mode: u32) -> bool {
    mode == MODE_REGULAR || mode == MODE_EXECUTABLE
}

/// Split in-place edits that qualify for `diffcore-break` into delete+add pairs.
///
/// Returns the expanded entry list and the set of paths that were split (for `diffcore_merge_broken`).
fn apply_diffcore_break_rewrites_split(
    odb: &Odb,
    work_root: Option<&Path>,
    cached: bool,
    entries: Vec<DiffEntry>,
    break_score: u64,
    merge_after_break_score: u64,
) -> (Vec<DiffEntry>, HashSet<String>) {
    let mut out = Vec::with_capacity(entries.len() + 8);
    let mut broken_paths = HashSet::new();
    for e in entries {
        if e.status != DiffStatus::Modified {
            out.push(e);
            continue;
        }
        if e.old_oid == zero_oid() || e.new_oid == zero_oid() {
            out.push(e);
            continue;
        }
        let Some(old_mode) = parse_mode_octal(&e.old_mode) else {
            out.push(e);
            continue;
        };
        let Some(new_mode) = parse_mode_octal(&e.new_mode) else {
            out.push(e);
            continue;
        };
        if !is_break_eligible_blob_mode(old_mode) || !is_break_eligible_blob_mode(new_mode) {
            out.push(e);
            continue;
        }
        let Ok(old_obj) = odb.read(&e.old_oid) else {
            out.push(e);
            continue;
        };
        let new_data = if e.new_oid != zero_oid() {
            match odb.read(&e.new_oid) {
                Ok(obj) => obj.data,
                Err(_) => {
                    out.push(e);
                    continue;
                }
            }
        } else if !cached {
            let Some(wt) = work_root else {
                out.push(e);
                continue;
            };
            let Some(path) = e.new_path.as_deref().or(e.old_path.as_deref()) else {
                out.push(e);
                continue;
            };
            match fs::read(path.to_fs_path(wt)) {
                Ok(b) => b,
                Err(_) => {
                    out.push(e);
                    continue;
                }
            }
        } else {
            out.push(e);
            continue;
        };
        let old_data = old_obj.data;
        if grit_lib::merge_file::is_binary(&old_data) || grit_lib::merge_file::is_binary(&new_data)
        {
            out.push(e);
            continue;
        }
        if !should_break_rewrite_pair(&old_data, &new_data, break_score) {
            out.push(e);
            continue;
        }
        let merge_ms = rewrite_merge_score(&old_data, &new_data).unwrap_or(0);
        let dissim_pct = if merge_ms < merge_after_break_score {
            None
        } else {
            rewrite_dissimilarity_index_percent(&old_data, &new_data)
        };
        let Some(path) = e.old_path.clone().or_else(|| e.new_path.clone()) else {
            out.push(e);
            continue;
        };
        broken_paths.insert(path.to_string_lossy().into_owned());
        out.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.clone()),
            new_path: None,
            old_mode: e.old_mode.clone(),
            new_mode: "000000".to_owned(),
            old_oid: e.old_oid,
            new_oid: zero_oid(),
            score: dissim_pct,
        });
        out.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(path),
            old_mode: "000000".to_owned(),
            new_mode: e.new_mode.clone(),
            old_oid: zero_oid(),
            new_oid: e.new_oid,
            score: dissim_pct,
        });
    }
    (out, broken_paths)
}

/// After `-B` + rename detection, drop the leftover delete at a path that is already the
/// destination of a rename/copy (consumed break half); see `t4008-diff-break-rewrite` #4.
fn drop_break_delete_superseded_by_rename_dest(entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
    let mut targets: HashSet<String> = HashSet::new();
    for e in &entries {
        if matches!(e.status, DiffStatus::Renamed | DiffStatus::Copied) {
            if let Some(p) = e.new_path.as_deref() {
                targets.insert(p.to_string_lossy().into_owned());
            }
        }
    }
    let mut out: Vec<DiffEntry> = entries
        .into_iter()
        .filter(|e| {
            if e.status != DiffStatus::Deleted {
                return true;
            }
            let Some(p) = e.old_path.as_deref() else {
                return true;
            };
            !targets.contains(&*p.to_string_lossy())
        })
        .collect();
    out.sort_by(|a, b| a.path().cmp(b.path()));
    out
}

/// Merge delete+add pairs that survived rename/copy back into a single modification (Git
/// `diffcore_merge_broken`).
fn merge_broken_rewrite_pairs(
    entries: Vec<DiffEntry>,
    broken_paths: &HashSet<String>,
) -> Vec<DiffEntry> {
    let mut slots: HashMap<String, (Option<DiffEntry>, Option<DiffEntry>)> = HashMap::new();
    let mut others = Vec::new();
    for e in entries {
        if e.status == DiffStatus::Deleted {
            if let Some(p) = e.old_path.as_deref() {
                let key = p.to_string_lossy().into_owned();
                if broken_paths.contains(&key) {
                    slots.entry(key).or_default().0 = Some(e);
                    continue;
                }
            }
        }
        if e.status == DiffStatus::Added {
            if let Some(p) = e.new_path.as_deref() {
                let key = p.to_string_lossy().into_owned();
                if broken_paths.contains(&key) {
                    slots.entry(key).or_default().1 = Some(e);
                    continue;
                }
            }
        }
        others.push(e);
    }
    let mut merged = Vec::new();
    for (path, (d_opt, a_opt)) in slots {
        match (d_opt, a_opt) {
            (Some(d), Some(a)) => {
                merged.push(DiffEntry {
                    status: DiffStatus::Modified,
                    // TODO(byte-paths): slots keyed by lossy String (Phase 2/4)
                    old_path: Some(RepoPathBuf::from_string(path.clone())),
                    new_path: Some(RepoPathBuf::from_string(path)),
                    old_mode: d.old_mode,
                    new_mode: a.new_mode,
                    old_oid: d.old_oid,
                    new_oid: a.new_oid,
                    score: d.score.or(a.score),
                });
            }
            (Some(d), None) => merged.push(d),
            (None, Some(a)) => merged.push(a),
            (None, None) => {}
        }
    }
    others.extend(merged);
    others.sort_by(|a, b| a.path().cmp(b.path()));
    others
}

fn effective_index_path(repo: &Repository) -> Result<PathBuf> {
    if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Ok(path);
        }
        let cwd = std::env::current_dir().context("resolving GIT_INDEX_FILE")?;
        return Ok(cwd.join(path));
    }
    Ok(repo.index_path())
}

/// Convert a RawChange to a DiffEntry for rename detection.
fn raw_change_to_diff_entry(change: &RawChange) -> DiffEntry {
    let status = match change.status {
        'A' => DiffStatus::Added,
        'D' => DiffStatus::Deleted,
        'M' => DiffStatus::Modified,
        'R' => DiffStatus::Renamed,
        'C' => DiffStatus::Copied,
        'T' => DiffStatus::TypeChanged,
        'U' => DiffStatus::Unmerged,
        _ => DiffStatus::Modified,
    };

    let old_mode = change.old.map_or(0, |s| s.mode);
    let new_mode = change.new.map_or(0, |s| s.mode);

    DiffEntry {
        status,
        // TODO(byte-paths): RawChange.path is a lossy String (Phase 2/4)
        old_path: if change.status == 'A' {
            None
        } else {
            Some(RepoPathBuf::from_string(change.path.clone()))
        },
        new_path: if change.status == 'D' {
            None
        } else {
            Some(RepoPathBuf::from_string(change.path.clone()))
        },
        old_mode: format!("{old_mode:06o}"),
        new_mode: format!("{new_mode:06o}"),
        old_oid: change.old.map_or_else(zero_oid, |s| s.oid),
        new_oid: change.new.map_or_else(zero_oid, |s| s.oid),
        score: None,
    }
}

fn old_path_bytes(e: &DiffEntry) -> &[u8] {
    e.old_path.as_deref().map(RepoPath::as_bytes).unwrap_or(b"")
}
fn new_path_bytes(e: &DiffEntry) -> &[u8] {
    e.new_path.as_deref().map(RepoPath::as_bytes).unwrap_or(b"")
}
fn old_path_lossy(e: &DiffEntry) -> std::borrow::Cow<'_, str> {
    e.old_path
        .as_deref()
        .map_or(std::borrow::Cow::Borrowed(""), RepoPath::to_string_lossy)
}
fn new_path_lossy(e: &DiffEntry) -> std::borrow::Cow<'_, str> {
    e.new_path
        .as_deref()
        .map_or(std::borrow::Cow::Borrowed(""), RepoPath::to_string_lossy)
}

pub(crate) fn write_diff_index_name_status(
    out: &mut impl std::io::Write,
    entries: &[DiffEntry],
    quote_fully: bool,
    nul: bool,
) -> Result<()> {
    for entry in entries {
        match (entry.status, entry.score) {
            (DiffStatus::Renamed, Some(s)) => {
                if nul {
                    write!(out, "R{s:03}\0")?;
                    out.write_all(old_path_bytes(entry))?;
                    out.write_all(b"\0")?;
                    out.write_all(new_path_bytes(entry))?;
                    out.write_all(b"\0")?;
                } else {
                    writeln!(
                        out,
                        "R{s:03}\t{}\t{}",
                        quote_c_style(&old_path_lossy(entry), quote_fully),
                        quote_c_style(&new_path_lossy(entry), quote_fully),
                    )?;
                }
            }
            (DiffStatus::Copied, Some(s)) => {
                if nul {
                    write!(out, "C{s:03}\0")?;
                    out.write_all(old_path_bytes(entry))?;
                    out.write_all(b"\0")?;
                    out.write_all(new_path_bytes(entry))?;
                    out.write_all(b"\0")?;
                } else {
                    writeln!(
                        out,
                        "C{s:03}\t{}\t{}",
                        quote_c_style(&old_path_lossy(entry), quote_fully),
                        quote_c_style(&new_path_lossy(entry), quote_fully),
                    )?;
                }
            }
            // A broken (`-B`) in-place rewrite carries a similarity score on the
            // surviving Modified/TypeChanged entry (e.g. `T100`).
            (DiffStatus::Modified | DiffStatus::TypeChanged, Some(s)) => {
                let letter = entry.status.letter();
                if nul {
                    write!(out, "{letter}{s:03}\0")?;
                    out.write_all(entry.path().as_bytes())?;
                    out.write_all(b"\0")?;
                } else {
                    writeln!(
                        out,
                        "{letter}{s:03}\t{}",
                        quote_c_style(&entry.path().to_string_lossy(), quote_fully)
                    )?;
                }
            }
            _ => {
                if nul {
                    write!(out, "{}\0", entry.status.letter())?;
                    out.write_all(entry.path().as_bytes())?;
                    out.write_all(b"\0")?;
                } else {
                    writeln!(
                        out,
                        "{}\t{}",
                        entry.status.letter(),
                        quote_c_style(&entry.path().to_string_lossy(), quote_fully)
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Write one `diff-index` raw record in Git's `-z` format.
///
/// The colon-prefixed status line ends with a NUL byte (no tab before paths).
/// For renames/copies, old and new paths are each NUL-terminated. For other
/// statuses, a single path is NUL-terminated.
/// True when a gitlink raw entry's new side must print the all-zero OID.
///
/// For uncached `diff-index --raw`, Git reports a worktree-side submodule change (the recorded
/// gitlink in the index still equals the tree gitlink, only the submodule's checked-out HEAD has
/// moved) with the null OID on the new side, exactly like `diff-files`. When the index gitlink
/// itself differs from the tree (a staged submodule bump), Git prints the real index OID. The
/// `--submodule` patch path keeps the resolved HEAD regardless; this only adjusts raw output.
/// See `t4027` #3.
fn raw_gitlink_new_is_worktree_zero(
    index: &Index,
    entry: &DiffEntry,
    diff_index_uncached: bool,
) -> bool {
    if !diff_index_uncached || entry.new_mode != "160000" || entry.new_oid == zero_oid() {
        return false;
    }
    // Git shows the recorded index gitlink OID only when the index itself carries that gitlink
    // commit (a staged submodule bump). When the new side instead came from the live worktree
    // submodule HEAD (the index entry is the tree gitlink, a blob, or absent), Git prints the
    // all-zero OID in raw plumbing output even though the `--submodule` patch uses the real HEAD.
    let index_carries_new_gitlink = index
        .get(entry.path().as_bytes(), 0)
        .is_some_and(|e| canonicalize_mode(e.mode) == MODE_GITLINK && e.oid == entry.new_oid);
    !index_carries_new_gitlink
}

/// True when an uncached raw added entry's new side came from the work tree, not the index.
///
/// `diff_tree_vs_worktree` may refresh staged additions from the work tree so rename detection and
/// patch rendering can use the live mode/content. Raw `git diff-index` still prints an all-zero
/// object ID for that live worktree side; it prints the real OID only when the index carries it.
fn raw_added_new_is_worktree_zero(
    index: &Index,
    entry: &DiffEntry,
    diff_index_uncached: bool,
) -> bool {
    if !diff_index_uncached || entry.status != DiffStatus::Added || entry.new_oid == zero_oid() {
        return false;
    }
    index
        .get(entry.path().as_bytes(), 0)
        .is_some_and(|index_entry| {
            !index_entry.skip_worktree()
                && !index_entry.assume_unchanged()
                && index_entry.oid != entry.new_oid
        })
}

fn write_raw_diff_entry_z(
    out: &mut impl Write,
    entry: &DiffEntry,
    repo: &Repository,
    index: &Index,
    abbrev: Option<usize>,
    diff_index_uncached: bool,
) -> Result<()> {
    let full = repo.odb.hash_algo().hex_len();
    let width = abbrev.unwrap_or(full).clamp(4, full);

    let (old_oid_disp, new_oid_disp) = {
        let old_oid = if entry.old_oid == zero_oid() {
            "0".repeat(width)
        } else {
            match abbrev {
                Some(min_len) => abbreviate_object_id(repo, entry.old_oid, min_len)?,
                None => entry.old_oid.to_hex(),
            }
        };
        let new_oid = if entry.new_oid == zero_oid()
            || raw_gitlink_new_is_worktree_zero(index, entry, diff_index_uncached)
            || raw_added_new_is_worktree_zero(index, entry, diff_index_uncached)
        {
            "0".repeat(width)
        } else {
            match abbrev {
                Some(min_len) => abbreviate_object_id(repo, entry.new_oid, min_len)?,
                None => entry.new_oid.to_hex(),
            }
        };
        (old_oid, new_oid)
    };

    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{s:03}"),
        (DiffStatus::Copied, Some(s)) => format!("C{s:03}"),
        (DiffStatus::TypeChanged, Some(s)) => format!("T{s:03}"),
        _ => entry.status.letter().to_string(),
    };

    write!(
        out,
        ":{} {} {} {} {}",
        entry.old_mode, entry.new_mode, old_oid_disp, new_oid_disp, status_str
    )?;
    out.write_all(b"\0")?;

    match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            out.write_all(old_path_bytes(entry))?;
            out.write_all(b"\0")?;
            out.write_all(new_path_bytes(entry))?;
            out.write_all(b"\0")?;
        }
        _ => {
            out.write_all(entry.path().as_bytes())?;
            out.write_all(b"\0")?;
        }
    }
    Ok(())
}

/// Render a DiffEntry in raw format.
fn render_raw_diff_entry(
    entry: &DiffEntry,
    repo: &Repository,
    index: &Index,
    abbrev: Option<usize>,
    diff_index_uncached: bool,
    quote_fully: bool,
) -> Result<String> {
    let full = repo.odb.hash_algo().hex_len();
    let width = abbrev.unwrap_or(full).clamp(4, full);

    let (old_oid_disp, new_oid_disp) = {
        let old_oid = if entry.old_oid == zero_oid() {
            "0".repeat(width)
        } else {
            match abbrev {
                Some(min_len) => abbreviate_object_id(repo, entry.old_oid, min_len)?,
                None => entry.old_oid.to_hex(),
            }
        };
        let new_oid = if entry.new_oid == zero_oid()
            || raw_gitlink_new_is_worktree_zero(index, entry, diff_index_uncached)
            || raw_added_new_is_worktree_zero(index, entry, diff_index_uncached)
        {
            "0".repeat(width)
        } else {
            match abbrev {
                Some(min_len) => abbreviate_object_id(repo, entry.new_oid, min_len)?,
                None => entry.new_oid.to_hex(),
            }
        };
        (old_oid, new_oid)
    };

    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{:03}", s),
        (DiffStatus::Copied, Some(s)) => format!("C{:03}", s),
        (DiffStatus::TypeChanged, Some(s)) => format!("T{:03}", s),
        _ => entry.status.letter().to_string(),
    };

    let path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            format!(
                "{}\t{}",
                quote_c_style(&old_path_lossy(entry), quote_fully),
                quote_c_style(&new_path_lossy(entry), quote_fully),
            )
        }
        _ => quote_c_style(&entry.path().to_string_lossy(), quote_fully),
    };

    Ok(format!(
        ":{} {} {} {} {}\t{}",
        entry.old_mode, entry.new_mode, old_oid_disp, new_oid_disp, status_str, path
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SubmoduleDirtyFlags {
    untracked: bool,
    modified: bool,
}

pub(crate) fn submodule_dirty_flags(
    super_wt: &Path,
    path: &str,
    index_gitlink_oid: &ObjectId,
    ignore_untracked: bool,
    ignore_dirty: bool,
) -> SubmoduleDirtyFlags {
    let sub = super_wt.join(path);
    if read_submodule_head_oid(&sub).is_none() {
        return SubmoduleDirtyFlags::default();
    }
    // Mirror Git's `is_submodule_modified`, which runs `git status --porcelain=2` inside the
    // submodule. `submodule_porcelain_flags` reproduces that v2 state machine: it counts staged
    // (index vs HEAD) *and* unstaged changes as `modified`, and — crucially — does not descend into
    // a *nested* submodule's work tree when scanning for untracked content (a nested checkout is a
    // tracked gitlink, not loose untracked files). The naive directory walk used previously treated
    // every file under a nested submodule as untracked content of the parent (t4060 #50).
    let flags = grit_lib::diff::submodule_porcelain_flags(super_wt, path, *index_gitlink_oid);
    SubmoduleDirtyFlags {
        untracked: !ignore_untracked && flags.untracked,
        modified: !ignore_dirty && flags.modified,
    }
}

fn read_commit_tree(odb: &Odb, commit_oid: &ObjectId) -> Option<ObjectId> {
    let obj = odb.read(commit_oid).ok()?;
    if obj.kind != ObjectKind::Commit {
        return None;
    }
    parse_commit(&obj.data).ok().map(|c| c.tree)
}

fn submodule_display_name(full_path: &str) -> &str {
    full_path.rsplit('/').next().unwrap_or(full_path)
}

fn write_submodule_log_commit_lines(
    out: &mut impl Write,
    sub_repo: &Repository,
    old_commit: ObjectId,
    new_commit: ObjectId,
    fast_forward: bool,
    fast_backward: bool,
) -> Result<()> {
    let z = zero_oid();
    if old_commit == z || new_commit == z {
        return Ok(());
    }
    // Mirror Git's `print_submodule_diff_summary` (submodule.c): a single first-parent,
    // left-right symmetric walk of `old...new` with the merge bases excluded, emitting
    // commits in commit-date order (newest first). Left (old) side prints `<`, right (new)
    // side prints `>`. This interleaves the two sides exactly like Git rather than grouping.
    let _ = (fast_forward, fast_backward);
    let mut opts = RevListOptions::default();
    opts.first_parent = true;
    opts.left_right = true;
    opts.symmetric_left = Some(old_commit);
    opts.symmetric_right = Some(new_commit);

    let bases = merge_bases(sub_repo, old_commit, new_commit, true).unwrap_or_default();
    let negatives: Vec<String> = bases.iter().map(ObjectId::to_hex).collect();

    let Ok(res) = rev_list(
        sub_repo,
        &[old_commit.to_hex(), new_commit.to_hex()],
        &negatives,
        &opts,
    ) else {
        return Ok(());
    };
    for oid in &res.commits {
        // Skip the merge bases / shared spine (only commits unique to one side are shown).
        let is_left = match res.left_right_map.get(oid) {
            Some(&v) => v,
            None => continue,
        };
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
        if is_left {
            writeln!(out, "  < {subject}")?;
        } else {
            writeln!(out, "  > {subject}")?;
        }
    }
    Ok(())
}

/// Locate the absorbed git directory of a submodule whose worktree is no longer present.
///
/// Git stores an absorbed submodule under `<super_git_dir>/modules/<name>`, keyed by the submodule
/// logical *name* (from `.gitmodules`), which may differ from its path. A *nested* submodule lives
/// one level deeper: under `<parent_gitdir>/modules/<nested_name>`. This walks the path hierarchy —
/// `sm2/nested` resolves the top-level `sm2` from the superproject `.gitmodules`, then descends
/// `modules/nested` inside `sm2`'s absorbed gitdir (t4060 #51). Returns the path only if it exists
/// and looks like a git directory.
fn absorbed_submodule_gitdir(super_repo: &Repository, sub_path: &str) -> Option<PathBuf> {
    let work_tree = super_repo.work_tree.as_deref()?;
    let index = super_repo.load_index().ok();
    let regs = grit_lib::submodule_config::load_submodule_registrations(
        work_tree,
        index.as_ref(),
        Some(&super_repo.odb),
    );
    // Fall back to the path itself as the name (the common case where name == path).
    let top_name =
        grit_lib::submodule_config::submodule_name_for_path(&regs, sub_path).unwrap_or(sub_path);
    let direct =
        grit_lib::submodule_gitdir::submodule_modules_git_dir(&super_repo.git_dir, top_name);
    if grit_lib::submodule_gitdir::is_git_directory(&direct) {
        return Some(direct);
    }
    // Nested submodule: descend `modules/<seg>` for each path segment, resolving the top-level
    // segment's logical name from the superproject `.gitmodules`. Inner segments use the path
    // component as the name (Git records nested submodules with name == path in the common case).
    let segments: Vec<&str> = sub_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let first = segments[0];
    let first_name =
        grit_lib::submodule_config::submodule_name_for_path(&regs, first).unwrap_or(first);
    let mut gitdir =
        grit_lib::submodule_gitdir::submodule_modules_git_dir(&super_repo.git_dir, first_name);
    for seg in &segments[1..] {
        gitdir = grit_lib::submodule_gitdir::submodule_modules_git_dir(&gitdir, seg);
    }
    if grit_lib::submodule_gitdir::is_git_directory(&gitdir) {
        Some(gitdir)
    } else {
        None
    }
}

pub(crate) fn write_submodule_diff_recursive(
    out: &mut impl Write,
    super_repo: &Repository,
    full_path_from_root: &str,
    old_commit: ObjectId,
    new_commit: ObjectId,
    dirty: SubmoduleDirtyFlags,
    ignore_dirty_for_inner: bool,
    submodule_ignore: SubmoduleIgnoreFlags,
    submodule_format: SubmodulePatchFormat,
    context_lines: usize,
    indent_heuristic: bool,
) -> Result<()> {
    let z = zero_oid();
    let super_wt = super_repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
    let sub_path = super_wt.join(full_path_from_root);
    let sub_name = submodule_display_name(full_path_from_root);

    if dirty.untracked {
        writeln!(out, "Submodule {sub_name} contains untracked content")?;
    }
    if dirty.modified {
        writeln!(out, "Submodule {sub_name} contains modified content")?;
    }

    let mut sub_repo = Repository::discover(Some(&sub_path)).ok();
    // `Repository::discover` walks up from a missing submodule path and may resolve to the
    // *superproject* itself; that repo cannot read the submodule's objects. Treat it as "no
    // submodule repo" so the absorbed-gitdir fallback below kicks in.
    if sub_repo
        .as_ref()
        .is_some_and(|r| r.git_dir == super_repo.git_dir)
    {
        sub_repo = None;
    }
    // When the submodule worktree is gone (e.g. `mv sm sm-bak` or an outright delete) but its git
    // directory was absorbed into `<super>/.git/modules/<name>`, Git still reads the submodule's
    // objects from there to render the inner diff (t4060 #45/#46). Fall back to that gitdir.
    if sub_repo.is_none() {
        if let Some(gitdir) = absorbed_submodule_gitdir(super_repo, full_path_from_root) {
            sub_repo = Repository::open(&gitdir, None).ok();
        }
    }
    let odb_for = sub_repo.as_ref().map(|r| &r.odb).unwrap_or(&super_repo.odb);

    let old_tree = if old_commit == z {
        None
    } else {
        read_commit_tree(odb_for, &old_commit)
    };
    let new_tree = if new_commit == z {
        None
    } else {
        read_commit_tree(odb_for, &new_commit)
    };

    let mut message: Option<&'static str> = None;
    if old_commit == z {
        message = Some("(new submodule)");
    } else if new_commit == z {
        message = Some("(submodule deleted)");
    } else if old_tree.is_none() || new_tree.is_none() {
        // Git's `show_submodule_summary`: `(commits not present)` only when BOTH endpoints are
        // real commits that the superproject ODB cannot resolve. A zero endpoint is always
        // reported as `(new submodule)`/`(submodule deleted)` even if the other commit is absent.
        message = Some("(commits not present)");
    }

    let mut fast_forward = false;
    let mut fast_backward = false;
    if message.is_none()
        && old_commit != z
        && new_commit != z
        && old_tree.is_some()
        && new_tree.is_some()
    {
        if let Some(ref sr) = sub_repo {
            let bases = merge_bases(sr, old_commit, new_commit, true).unwrap_or_default();
            if let Some(b) = bases.first() {
                if *b == old_commit {
                    fast_forward = true;
                } else if *b == new_commit {
                    fast_backward = true;
                }
            }
        }
    }

    if old_commit == new_commit && message.is_none() {
        if !dirty.untracked && !dirty.modified {
            return Ok(());
        }
        if !dirty.modified {
            // Untracked only: `Submodule … contains untracked content` — no commit-range header or patches.
            return Ok(());
        }
        // Only `--submodule=diff` emits the inner per-file unified diff for dirty content; for
        // `log`/`short` the `contains modified content` line (already printed) is the whole output.
        if submodule_format != SubmodulePatchFormat::Diff {
            return Ok(());
        }
        // Modified working tree vs same recorded commit: show inner diff only (no `Submodule a..b:` line).
        let use_worktree_for_right =
            !ignore_dirty_for_inner && new_commit != z && sub_repo.is_some();
        if let Some(sr) = sub_repo.as_ref().filter(|_| use_worktree_for_right) {
            let idx = sr.load_index().unwrap_or_else(|_| Index::new());
            let inner =
                grit_lib::diff::diff_tree_to_worktree(&sr.odb, old_tree.as_ref(), &sub_path, &idx)?;
            for e in &inner {
                let inner_full = format!("{full_path_from_root}/{}", e.path());
                if e.old_mode == "160000" || e.new_mode == "160000" {
                    write_patch_entry(
                        out,
                        super_repo,
                        &sr.odb,
                        e,
                        context_lines,
                        Some(&sub_path),
                        SubmodulePatchFormat::Diff,
                        submodule_ignore,
                        &inner_full,
                        indent_heuristic,
                    )?;
                } else {
                    write_patch_entry_inner(
                        out,
                        super_repo,
                        &sr.odb,
                        e,
                        context_lines,
                        Some(&sub_path),
                        full_path_from_root,
                        indent_heuristic,
                    )?;
                }
            }
        } else {
            let odb_inner = sub_repo.as_ref().map(|r| &r.odb).unwrap_or(&super_repo.odb);
            let inner = diff_trees(odb_inner, old_tree.as_ref(), new_tree.as_ref(), "")?;
            for e in &inner {
                let inner_full = format!("{full_path_from_root}/{}", e.path());
                if e.old_mode == "160000" || e.new_mode == "160000" {
                    write_patch_entry(
                        out,
                        super_repo,
                        odb_inner,
                        e,
                        context_lines,
                        None,
                        SubmodulePatchFormat::Diff,
                        submodule_ignore,
                        &inner_full,
                        indent_heuristic,
                    )?;
                } else {
                    write_patch_entry_inner(
                        out,
                        super_repo,
                        odb_inner,
                        e,
                        context_lines,
                        None,
                        full_path_from_root,
                        indent_heuristic,
                    )?;
                }
            }
        }
        return Ok(());
    }

    let old_abbr = abbreviate_object_id(super_repo, old_commit, 7)?;
    let new_abbr = abbreviate_object_id(super_repo, new_commit, 7)?;
    let sep = if fast_forward || fast_backward {
        ".."
    } else {
        "..."
    };
    write!(out, "Submodule {sub_name} {old_abbr}{sep}{new_abbr}")?;
    if let Some(m) = message {
        writeln!(out, " {m}")?;
    } else if fast_backward {
        writeln!(out, " (rewind):")?;
    } else {
        writeln!(out, ":")?;
    }

    if message == Some("(commits not present)") {
        return Ok(());
    }
    // For `(new submodule)`/`(submodule deleted)`, `--submodule=diff` still renders the
    // inner per-file diff (empty-tree → new_tree for a new submodule, old_tree → empty-tree
    // for a deleted one) whenever the submodule objects are reachable; the message is only a
    // header annotation. `log`/`short` stop at the header. When the submodule repo is gone
    // (e.g. `rm -rf sm`), both trees are None and the inner diff is empty, so only the header
    // prints — matching Git.
    let new_or_deleted =
        message == Some("(new submodule)") || message == Some("(submodule deleted)");
    if message.is_some() && !(new_or_deleted && submodule_format == SubmodulePatchFormat::Diff) {
        return Ok(());
    }
    if new_or_deleted && submodule_format == SubmodulePatchFormat::Diff {
        let odb_inner = sub_repo.as_ref().map(|r| &r.odb).unwrap_or(&super_repo.odb);
        let inner = diff_trees(odb_inner, old_tree.as_ref(), new_tree.as_ref(), "")?;
        for e in &inner {
            let inner_full = format!("{full_path_from_root}/{}", e.path());
            if e.old_mode == "160000" || e.new_mode == "160000" {
                write_patch_entry(
                    out,
                    super_repo,
                    odb_inner,
                    e,
                    context_lines,
                    None,
                    SubmodulePatchFormat::Diff,
                    submodule_ignore,
                    &inner_full,
                    indent_heuristic,
                )?;
            } else {
                write_patch_entry_inner(
                    out,
                    super_repo,
                    odb_inner,
                    e,
                    context_lines,
                    None,
                    full_path_from_root,
                    indent_heuristic,
                )?;
            }
        }
        return Ok(());
    }

    if submodule_format == SubmodulePatchFormat::Log {
        if let Some(sr) = sub_repo.as_ref() {
            write_submodule_log_commit_lines(
                out,
                sr,
                old_commit,
                new_commit,
                fast_forward,
                fast_backward,
            )?;
        }
        return Ok(());
    }

    let use_worktree_for_right =
        dirty.modified && !ignore_dirty_for_inner && new_commit != z && sub_repo.is_some();

    if submodule_format != SubmodulePatchFormat::Diff {
        return Ok(());
    }

    if let Some(sr) = sub_repo.as_ref().filter(|_| use_worktree_for_right) {
        let idx = sr.load_index().unwrap_or_else(|_| Index::new());
        let inner =
            grit_lib::diff::diff_tree_to_worktree(&sr.odb, old_tree.as_ref(), &sub_path, &idx)?;
        for e in &inner {
            let inner_full = format!("{full_path_from_root}/{}", e.path());
            if e.old_mode == "160000" || e.new_mode == "160000" {
                write_patch_entry(
                    out,
                    super_repo,
                    &sr.odb,
                    e,
                    context_lines,
                    Some(&sub_path),
                    SubmodulePatchFormat::Diff,
                    submodule_ignore,
                    &inner_full,
                    indent_heuristic,
                )?;
            } else {
                write_patch_entry_inner(
                    out,
                    super_repo,
                    &sr.odb,
                    e,
                    context_lines,
                    Some(&sub_path),
                    full_path_from_root,
                    indent_heuristic,
                )?;
            }
        }
    } else {
        let odb_inner = sub_repo.as_ref().map(|r| &r.odb).unwrap_or(&super_repo.odb);
        let inner = diff_trees(odb_inner, old_tree.as_ref(), new_tree.as_ref(), "")?;
        for e in &inner {
            let inner_full = format!("{full_path_from_root}/{}", e.path());
            if e.old_mode == "160000" || e.new_mode == "160000" {
                write_patch_entry(
                    out,
                    super_repo,
                    odb_inner,
                    e,
                    context_lines,
                    None,
                    SubmodulePatchFormat::Diff,
                    submodule_ignore,
                    &inner_full,
                    indent_heuristic,
                )?;
            } else {
                write_patch_entry_inner(
                    out,
                    super_repo,
                    odb_inner,
                    e,
                    context_lines,
                    None,
                    full_path_from_root,
                    indent_heuristic,
                )?;
            }
        }
    }

    Ok(())
}

/// Write a unified-diff block for one entry (diff-index -p).
pub(crate) fn write_patch_entry(
    out: &mut impl std::io::Write,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    context_lines: usize,
    work_tree: Option<&Path>,
    submodule_format: SubmodulePatchFormat,
    submodule_ignore: SubmoduleIgnoreFlags,
    full_path_from_root: &str,
    indent_heuristic: bool,
) -> Result<()> {
    let z = zero_oid();

    if submodule_format != SubmodulePatchFormat::Short {
        // Typechange blob→gitlink: tree had a blob, index records a gitlink.
        let is_blob_to_gitlink = entry.old_mode != "000000"
            && entry.old_mode != "160000"
            && entry.new_mode == "160000"
            && entry.new_oid != z;
        // Typechange gitlink→blob: tree had gitlink, index/worktree has a regular file.
        let is_gitlink_to_blob = entry.old_mode == "160000"
            && entry.old_oid != z
            && entry.new_mode != "160000"
            && entry.new_mode != "000000";

        if is_gitlink_to_blob {
            // Submodule summary first (`(submodule deleted)`), then the blob **add** — the regular
            // file is the new side of the typechange (matches `git diff --submodule=log`).
            write_submodule_diff_recursive(
                out,
                repo,
                full_path_from_root,
                entry.old_oid,
                z,
                SubmoduleDirtyFlags::default(),
                submodule_ignore.ignore_dirty,
                submodule_ignore,
                submodule_format,
                context_lines,
                indent_heuristic,
            )?;
            let mut blob_add = entry.clone();
            blob_add.status = DiffStatus::Added;
            blob_add.old_path = None;
            blob_add.old_mode = "000000".to_owned();
            blob_add.old_oid = z;
            blob_add.new_mode = entry.new_mode.clone();
            blob_add.new_oid = entry.new_oid;
            write_patch_entry_inner(
                out,
                repo,
                odb,
                &blob_add,
                context_lines,
                work_tree,
                "",
                indent_heuristic,
            )?;
            return Ok(());
        }

        if is_blob_to_gitlink {
            // Blob delete first, then submodule summary (matches `git diff --submodule=log`).
            let mut blob_del = entry.clone();
            blob_del.status = DiffStatus::Deleted;
            blob_del.old_mode = entry.old_mode.clone();
            blob_del.old_oid = entry.old_oid;
            blob_del.new_path = None;
            blob_del.new_mode = "000000".to_owned();
            blob_del.new_oid = z;
            write_patch_entry_inner(
                out,
                repo,
                odb,
                &blob_del,
                context_lines,
                work_tree,
                "",
                indent_heuristic,
            )?;
            write_submodule_diff_recursive(
                out,
                repo,
                full_path_from_root,
                z,
                entry.new_oid,
                SubmoduleDirtyFlags::default(),
                submodule_ignore.ignore_dirty,
                submodule_ignore,
                submodule_format,
                context_lines,
                indent_heuristic,
            )?;
            return Ok(());
        }

        if entry.old_mode == "160000" || entry.new_mode == "160000" {
            let super_wt = repo.work_tree.as_deref().unwrap_or(Path::new(""));
            let index_oid = if entry.new_mode == "160000" && entry.new_oid != z {
                entry.new_oid
            } else if entry.old_mode == "160000" && entry.old_oid != z {
                entry.old_oid
            } else {
                z
            };
            let dirty = if !super_wt.as_os_str().is_empty() && index_oid != z {
                submodule_dirty_flags(
                    super_wt,
                    full_path_from_root,
                    &index_oid,
                    submodule_ignore.ignore_untracked,
                    submodule_ignore.ignore_dirty,
                )
            } else {
                SubmoduleDirtyFlags::default()
            };
            write_submodule_diff_recursive(
                out,
                repo,
                full_path_from_root,
                entry.old_oid,
                entry.new_oid,
                dirty,
                submodule_ignore.ignore_dirty,
                submodule_ignore,
                submodule_format,
                context_lines,
                indent_heuristic,
            )?;
            return Ok(());
        }
    }

    write_patch_entry_inner(
        out,
        repo,
        odb,
        entry,
        context_lines,
        work_tree,
        "",
        indent_heuristic,
    )
}

pub(crate) fn write_patch_entry_inner(
    out: &mut impl std::io::Write,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    context_lines: usize,
    work_tree: Option<&Path>,
    path_prefix: &str,
    indent_heuristic: bool,
) -> Result<()> {
    use grit_lib::diff::unified_diff_with_prefix;

    validate_patch_entry_oids(entry)?;

    let quote_fully = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_default()
        .quote_path_fully();

    let empty = RepoPath::from_str("");
    let old_path = entry
        .old_path
        .as_deref()
        .unwrap_or(entry.new_path.as_deref().unwrap_or(empty));
    let new_path = entry
        .new_path
        .as_deref()
        .unwrap_or(entry.old_path.as_deref().unwrap_or(empty));

    let (disp_old, disp_new) = if path_prefix.is_empty() {
        (old_path.to_string(), new_path.to_string())
    } else {
        (
            format!("{path_prefix}/{old_path}"),
            format!("{path_prefix}/{new_path}"),
        )
    };
    let git_old = format_diff_path_with_prefix("a/", &disp_old, quote_fully);
    let git_new = format_diff_path_with_prefix("b/", &disp_new, quote_fully);

    if entry.status == DiffStatus::Modified
        && entry.old_mode == entry.new_mode
        && entry.new_oid == zero_oid()
    {
        if let Some(wt) = work_tree {
            let new_raw = read_worktree_path_raw(&new_path.to_fs_path(wt));
            if read_blob_raw(odb, &entry.old_oid) == new_raw {
                return Ok(());
            }
        }
    }

    if entry.old_mode == "160000" || entry.new_mode == "160000" {
        writeln!(out, "diff --git {git_old} {git_new}")?;
        match entry.status {
            DiffStatus::Added => {
                writeln!(out, "new file mode {}", entry.new_mode)?;
                writeln!(
                    out,
                    "index {}..{}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7]
                )?;
                writeln!(out, "--- /dev/null")?;
                writeln!(
                    out,
                    "+++ {}",
                    format_diff_path_with_prefix("b/", &disp_new, quote_fully)
                )?;
                writeln!(out, "@@ -0,0 +1 @@")?;
                writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
            }
            DiffStatus::Deleted => {
                writeln!(out, "deleted file mode {}", entry.old_mode)?;
                writeln!(
                    out,
                    "index {}..{}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7]
                )?;
                writeln!(
                    out,
                    "--- {}",
                    format_diff_path_with_prefix("a/", &disp_old, quote_fully)
                )?;
                writeln!(out, "+++ /dev/null")?;
                writeln!(out, "@@ -1 +0,0 @@")?;
                writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
            }
            DiffStatus::Modified | DiffStatus::Renamed | DiffStatus::Copied => {
                if entry.old_mode == "160000" && entry.new_mode == "160000" {
                    writeln!(
                        out,
                        "index {}..{} {}",
                        &entry.old_oid.to_hex()[..7],
                        &entry.new_oid.to_hex()[..7],
                        entry.old_mode
                    )?;
                    writeln!(
                        out,
                        "--- {}",
                        format_diff_path_with_prefix("a/", &disp_old, quote_fully)
                    )?;
                    writeln!(
                        out,
                        "+++ {}",
                        format_diff_path_with_prefix("b/", &disp_new, quote_fully)
                    )?;
                    writeln!(out, "@@ -1 +1 @@")?;
                    writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                    writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
                } else if entry.old_mode == "160000" {
                    writeln!(
                        out,
                        "index {}..{} {}",
                        &entry.old_oid.to_hex()[..7],
                        &entry.new_oid.to_hex()[..7],
                        entry.old_mode
                    )?;
                    writeln!(
                        out,
                        "--- {}",
                        format_diff_path_with_prefix("a/", &disp_old, quote_fully)
                    )?;
                    writeln!(
                        out,
                        "+++ {}",
                        format_diff_path_with_prefix("b/", &disp_new, quote_fully)
                    )?;
                    writeln!(out, "@@ -1 +0,0 @@")?;
                    writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                } else {
                    writeln!(
                        out,
                        "index {}..{} {}",
                        &entry.old_oid.to_hex()[..7],
                        &entry.new_oid.to_hex()[..7],
                        entry.new_mode
                    )?;
                    writeln!(
                        out,
                        "--- {}",
                        format_diff_path_with_prefix("a/", &disp_old, quote_fully)
                    )?;
                    writeln!(
                        out,
                        "+++ {}",
                        format_diff_path_with_prefix("b/", &disp_new, quote_fully)
                    )?;
                    writeln!(out, "@@ -0,0 +1 @@")?;
                    writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
                }
            }
            DiffStatus::TypeChanged => {
                if entry.old_mode == "160000" {
                    writeln!(
                        out,
                        "index {}..{} {}",
                        &entry.old_oid.to_hex()[..7],
                        &entry.new_oid.to_hex()[..7],
                        entry.old_mode
                    )?;
                    writeln!(
                        out,
                        "--- {}",
                        format_diff_path_with_prefix("a/", &disp_old, quote_fully)
                    )?;
                    writeln!(
                        out,
                        "+++ {}",
                        format_diff_path_with_prefix("b/", &disp_new, quote_fully)
                    )?;
                    writeln!(out, "@@ -1 +0,0 @@")?;
                    writeln!(out, "-Subproject commit {}", entry.old_oid.to_hex())?;
                } else if entry.new_mode == "160000" {
                    writeln!(
                        out,
                        "index {}..{} {}",
                        &entry.old_oid.to_hex()[..7],
                        &entry.new_oid.to_hex()[..7],
                        entry.new_mode
                    )?;
                    writeln!(
                        out,
                        "--- {}",
                        format_diff_path_with_prefix("a/", &disp_old, quote_fully)
                    )?;
                    writeln!(
                        out,
                        "+++ {}",
                        format_diff_path_with_prefix("b/", &disp_new, quote_fully)
                    )?;
                    writeln!(out, "@@ -0,0 +1 @@")?;
                    writeln!(out, "+Subproject commit {}", entry.new_oid.to_hex())?;
                }
            }
            DiffStatus::Unmerged => {}
        }
        return Ok(());
    }

    writeln!(out, "diff --git {git_old} {git_new}")?;

    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "new file mode {}", entry.new_mode)?;
            writeln!(
                out,
                "index {}..{}",
                &entry.old_oid.to_hex()[..7],
                &entry.new_oid.to_hex()[..7]
            )?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "deleted file mode {}", entry.old_mode)?;
            writeln!(
                out,
                "index {}..{}",
                &entry.old_oid.to_hex()[..7],
                &entry.new_oid.to_hex()[..7]
            )?;
        }
        DiffStatus::Modified => {
            if entry.old_mode == entry.new_mode {
                writeln!(
                    out,
                    "index {}..{} {}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7],
                    entry.old_mode
                )?;
            } else {
                writeln!(out, "old mode {}", entry.old_mode)?;
                writeln!(out, "new mode {}", entry.new_mode)?;
                writeln!(
                    out,
                    "index {}..{}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7]
                )?;
            }
        }
        DiffStatus::Renamed => {
            if entry.old_mode != entry.new_mode {
                writeln!(out, "old mode {}", entry.old_mode)?;
                writeln!(out, "new mode {}", entry.new_mode)?;
            }
            let sim = entry.score.unwrap_or(100);
            writeln!(out, "similarity index {sim}%")?;
            writeln!(
                out,
                "rename from {}",
                quote_c_style(&old_path.to_string_lossy(), quote_fully)
            )?;
            writeln!(
                out,
                "rename to {}",
                quote_c_style(&new_path.to_string_lossy(), quote_fully)
            )?;
            if entry.old_oid != entry.new_oid {
                writeln!(
                    out,
                    "index {}..{}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7]
                )?;
            }
        }
        DiffStatus::Copied => {
            let sim = entry.score.unwrap_or(100);
            writeln!(out, "similarity index {sim}%")?;
            writeln!(
                out,
                "copy from {}",
                quote_c_style(&old_path.to_string_lossy(), quote_fully)
            )?;
            writeln!(
                out,
                "copy to {}",
                quote_c_style(&new_path.to_string_lossy(), quote_fully)
            )?;
            if entry.old_oid != entry.new_oid {
                writeln!(
                    out,
                    "index {}..{}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7]
                )?;
            }
        }
        _ => {}
    }

    // For rename/copy with 100% similarity, skip the diff content entirely
    if (entry.status == DiffStatus::Renamed || entry.status == DiffStatus::Copied)
        && entry.old_oid == entry.new_oid
    {
        return Ok(());
    }

    // Read raw bytes for binary detection
    let old_raw = read_blob_raw(odb, &entry.old_oid);
    let new_raw = if entry.new_oid == zero_oid()
        && entry.status != DiffStatus::Deleted
        && worktree_side_is_placeholder(repo, entry)
    {
        // Zero OID for non-deleted entries means worktree content
        if let Some(wt) = work_tree {
            let path = entry.new_path.as_deref().unwrap_or(new_path);
            read_worktree_path_raw(&path.to_fs_path(wt))
        } else {
            Vec::new()
        }
    } else {
        let blob = read_blob_raw(odb, &entry.new_oid);
        // A dirty submodule's modified worktree blob is hashed (non-zero new_oid) but never
        // written to the submodule ODB, so `read_blob_raw` comes up empty. Fall back to the
        // worktree file so the inner `--submodule=diff` hunk shows the new content (t4060 #23).
        if blob.is_empty()
            && entry.new_oid != empty_blob_oid()
            && entry.status != DiffStatus::Deleted
        {
            if let Some(wt) = work_tree {
                let path = entry.new_path.as_deref().unwrap_or(new_path);
                read_worktree_path_raw(&path.to_fs_path(wt))
            } else {
                blob
            }
        } else {
            blob
        }
    };

    // Check for binary content
    let treat_as_binary_by_driver = !mode_is_symlink(&entry.old_mode)
        && !mode_is_symlink(&entry.new_mode)
        && (is_binary_driver_path(repo, work_tree, &old_path.to_string_lossy())
            || is_binary_driver_path(repo, work_tree, &new_path.to_string_lossy()));
    if treat_as_binary_by_driver || is_binary(&old_raw) || is_binary(&new_raw) {
        let bo = if entry.status == DiffStatus::Added {
            "/dev/null".to_owned()
        } else {
            format_diff_path_with_prefix("a/", &old_path.to_string_lossy(), quote_fully)
        };
        let bn = if entry.status == DiffStatus::Deleted {
            "/dev/null".to_owned()
        } else {
            format_diff_path_with_prefix("b/", &new_path.to_string_lossy(), quote_fully)
        };
        writeln!(out, "Binary files {bo} and {bn} differ")?;
        return Ok(());
    }

    let old_content = String::from_utf8_lossy(&old_raw).into_owned();
    let new_content = String::from_utf8_lossy(&new_raw).into_owned();

    let display_old = if entry.status == DiffStatus::Added {
        "/dev/null".to_owned()
    } else {
        format_diff_path_with_prefix("a/", &disp_old, quote_fully)
    };
    let display_new = if entry.status == DiffStatus::Deleted {
        "/dev/null".to_owned()
    } else {
        format_diff_path_with_prefix("b/", &disp_new, quote_fully)
    };

    let patch = unified_diff_with_prefix(
        &old_content,
        &new_content,
        &display_old,
        &display_new,
        context_lines,
        0,
        "",
        "",
        indent_heuristic,
        quote_fully,
    );
    write!(out, "{patch}")?;

    Ok(())
}

/// Check whether data looks binary (contains NUL in first 8 KiB).
fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

fn is_binary_driver_path(repo: &Repository, work_tree: Option<&Path>, path: &str) -> bool {
    let Some(wt) = work_tree else {
        return false;
    };
    let rules = grit_lib::crlf::load_gitattributes(wt);
    let Ok(config) = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true) else {
        return false;
    };
    let attrs = grit_lib::crlf::get_file_attrs(&rules, path, false, &config);
    let grit_lib::crlf::DiffAttr::Driver(ref driver) = attrs.diff_attr else {
        return false;
    };
    config
        .get_bool(&format!("diff.{driver}.binary"))
        .and_then(Result::ok)
        .unwrap_or(false)
}

/// Read raw blob bytes, returning empty vec for zero OID.
fn read_blob_raw(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if *oid == zero_oid() {
        Vec::new()
    } else {
        odb.read(oid).map(|o| o.data).unwrap_or_default()
    }
}

fn read_worktree_path_raw(path: &Path) -> Vec<u8> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Vec::new();
    };
    if meta.file_type().is_symlink() {
        return fs::read_link(path)
            .map(|target| target.as_os_str().as_bytes().to_vec())
            .unwrap_or_default();
    }
    if meta.is_dir() {
        return Vec::new();
    }
    fs::read(path).unwrap_or_default()
}

fn mode_is_symlink(mode: &str) -> bool {
    u32::from_str_radix(mode, 8).ok() == Some(MODE_SYMLINK)
}

fn validate_patch_entry_oids(entry: &DiffEntry) -> Result<()> {
    let zero = zero_oid();
    let old_bogus = entry.old_oid == zero && entry.old_mode != "000000";
    let new_bogus = entry.new_oid == zero
        && entry.new_mode != "000000"
        && !matches!(entry.status, DiffStatus::Modified | DiffStatus::TypeChanged);
    if old_bogus || new_bogus {
        bail!("bogus object {}", zero.to_hex());
    }
    Ok(())
}

/// Write --stat output for diff-index.
fn write_diff_index_stat(entries: &[DiffEntry], odb: &Odb) -> Result<()> {
    let mut file_stats: Vec<(std::borrow::Cow<str>, usize, usize, bool)> = Vec::new();
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    let mut files_changed = 0usize;

    for entry in entries {
        let old_raw = read_blob_raw(odb, &entry.old_oid);
        let new_raw = read_blob_raw(odb, &entry.new_oid);
        let binary = is_binary(&old_raw) || is_binary(&new_raw);
        let (ins, del) = if binary {
            (0, 0)
        } else {
            let old_content = String::from_utf8_lossy(&old_raw).into_owned();
            let new_content = String::from_utf8_lossy(&new_raw).into_owned();
            count_line_changes(&old_content, &new_content)
        };
        file_stats.push((entry.path().to_string_lossy(), ins, del, binary));
        total_ins += ins;
        total_del += del;
        files_changed += 1;
    }

    let max_path_len = file_stats
        .iter()
        .map(|(p, _, _, _)| p.len())
        .max()
        .unwrap_or(0);
    let max_count = file_stats
        .iter()
        .map(|(_, i, d, _)| i + d)
        .max()
        .unwrap_or(0);
    let count_width = format!("{}", max_count).len();

    for (path, ins, del, binary) in &file_stats {
        if *binary {
            println!(" {:<width$} | Bin", path, width = max_path_len);
        } else {
            let total = ins + del;
            let bar_len = if max_count > 0 {
                (total * 40) / max_count.max(1)
            } else {
                0
            };
            let plus_len = if total > 0 {
                (ins * bar_len) / total.max(1)
            } else {
                0
            };
            let minus_len = bar_len.saturating_sub(plus_len);
            let bar: String = "+".repeat(plus_len) + &"-".repeat(minus_len);
            println!(
                " {:<width$} | {:>cw$} {}",
                path,
                total,
                bar,
                width = max_path_len,
                cw = count_width
            );
        }
    }

    let mut summary = format!(
        " {} file{} changed",
        files_changed,
        if files_changed == 1 { "" } else { "s" }
    );
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
    println!("{summary}");
    Ok(())
}

/// Write --numstat output for diff-index.
fn write_diff_index_numstat(entries: &[DiffEntry], odb: &Odb) -> Result<()> {
    for entry in entries {
        let old_raw = read_blob_raw(odb, &entry.old_oid);
        let new_raw = read_blob_raw(odb, &entry.new_oid);
        if is_binary(&old_raw) || is_binary(&new_raw) {
            println!("-\t-\t{}", entry.path());
        } else {
            let old_content = String::from_utf8_lossy(&old_raw).into_owned();
            let new_content = String::from_utf8_lossy(&new_raw).into_owned();
            let (ins, del) = count_line_changes(&old_content, &new_content);
            println!("{}\t{}\t{}", ins, del, entry.path());
        }
    }
    Ok(())
}

/// Count insertions and deletions between two text contents.
fn count_line_changes(old: &str, new: &str) -> (usize, usize) {
    let old_lines: Vec<&str> = if old.is_empty() {
        vec![]
    } else {
        old.lines().collect()
    };
    let new_lines: Vec<&str> = if new.is_empty() {
        vec![]
    } else {
        new.lines().collect()
    };

    // Use a simple LCS-based approach
    let mut ins = 0;
    let mut del = 0;
    let mut i = 0;
    let mut j = 0;
    while i < old_lines.len() && j < new_lines.len() {
        if old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
        } else {
            // Try to find old_lines[i] ahead in new_lines
            let mut found_in_new = false;
            for k in (j + 1)..new_lines.len().min(j + 10) {
                if old_lines[i] == new_lines[k] {
                    ins += k - j;
                    j = k;
                    found_in_new = true;
                    break;
                }
            }
            if !found_in_new {
                del += 1;
                i += 1;
            }
        }
    }
    del += old_lines.len() - i;
    ins += new_lines.len() - j;
    (ins, del)
}

/// Returns true when stage-0 index content differs from `HEAD`'s tree (`diff-index --cached HEAD`).
pub fn index_cached_differs_from_head(repo: &Repository) -> Result<bool> {
    let tree_oid = resolve_tree_ish(repo, "HEAD")?;
    let mut tree_map = BTreeMap::new();
    collect_tree_entries(repo, &tree_oid, "", &mut tree_map)?;
    let index_path = effective_index_path(repo)?;
    let index = repo.load_index_at(&index_path).context("loading index")?;
    let mut index_map = BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        if let Ok(path) = String::from_utf8(entry.path.clone()) {
            index_map.insert(path, Snapshot::from_index_entry(entry.mode, entry.oid));
        }
    }
    let changes = diff_tree_vs_index(&tree_map, &index_map);
    Ok(!changes.is_empty())
}
