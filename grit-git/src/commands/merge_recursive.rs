//! `grit merge-recursive` — backend-style recursive three-way merge.
//!
//! This command mirrors the historical `git merge-recursive` plumbing entrypoint:
//! `git merge-recursive [options] <base> -- <ours> <theirs>`.
//! It updates index + working tree and exits non-zero on conflicts.

#![allow(clippy::manual_contains)]
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::index::IndexEntry;
use grit_lib::merge_file::MergeFavor;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use super::merge::{merge_trees_for_replay, MergeDirectoryRenamesMode, MergeRenameOptions};

/// Arguments for `grit merge-recursive`.
#[derive(Debug, ClapArgs)]
#[command(about = "Run recursive merge backend on explicit commits")]
pub struct Args {
    /// Raw merge-recursive arguments.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

/// Run `grit merge-recursive`.
pub fn run(args: Args) -> Result<()> {
    let repo = discover_repo_for_merge_recursive().context("not a git repository")?;
    let MergeRecursiveParsed {
        ws,
        rename_options,
        positional,
    } = parse_args(&repo, &args.args)?;
    if positional.len() != 3 {
        bail!("usage: git merge-recursive [<options>] <base> -- <head> <remote> [<base>...]");
    }

    let base_oid = resolve_commit_oid(&repo, &positional[0])?;
    let ours_oid = resolve_commit_oid(&repo, &positional[1])?;
    let theirs_oid = resolve_commit_oid(&repo, &positional[2])?;

    let base_tree = commit_tree(&repo, base_oid)?;
    let ours_tree = commit_tree(&repo, ours_oid)?;
    let theirs_tree = commit_tree(&repo, theirs_oid)?;

    let base_entries = tree_to_map(tree_to_index_entries(&repo, &base_tree, "")?);
    let ours_entries = tree_to_map(tree_to_index_entries(&repo, &ours_tree, "")?);
    let theirs_entries = tree_to_map(tree_to_index_entries(&repo, &theirs_tree, "")?);

    let their_name = positional[2].clone();
    let base_label = positional[0].clone();
    let mut merge_result = merge_trees_for_replay(
        &repo,
        &base_entries,
        &ours_entries,
        &theirs_entries,
        &their_name,
        &base_label,
        &ours_oid.to_hex(),
        &theirs_oid.to_hex(),
        MergeFavor::None,
        None,
        false,
        ws.ignore_all_space,
        ws.ignore_space_change,
        ws.ignore_space_at_eol,
        ws.ignore_cr_at_eol,
        MergeDirectoryRenamesMode::FromConfig,
        rename_options,
        Some((ours_oid.to_hex(), theirs_oid.to_hex())),
        None,
    )?;

    let auto_resolved_directory_file_paths: Vec<Vec<u8>> = merge_result
        .conflict_descriptions
        .iter()
        .filter(|desc| desc.kind == "file/directory")
        .map(|desc| desc.subject_path.as_bytes().to_vec())
        .collect();
    let only_auto_resolved_directory_file_conflicts = !auto_resolved_directory_file_paths
        .is_empty()
        && auto_resolved_directory_file_paths
            .iter()
            .all(|path| relocated_unmerged_entries_match(&merge_result.index, path))
        && merge_result.conflict_descriptions.iter().all(|desc| {
            auto_resolved_directory_file_paths
                .iter()
                .any(|path| desc.subject_path.as_bytes() == path.as_slice())
        });
    if merge_result.has_conflicts && only_auto_resolved_directory_file_conflicts {
        merge_result.index.entries.retain(|entry| {
            entry.stage() == 0
                || !auto_resolved_directory_file_paths
                    .iter()
                    .any(|path| entry.path == *path)
        });
        merge_result.index.sort();
        merge_result.conflict_descriptions.clear();
        merge_result.conflict_files.clear();
        merge_result.has_conflicts = false;
    }

    let index_path = repo.index_path_for_env()?;
    repo.write_index_at(&index_path, &mut merge_result.index)?;
    if let Some(ref wt) = repo.work_tree {
        remove_deleted_files(wt, &ours_entries, &merge_result.index)?;
        checkout_entries(&repo, wt, &merge_result.index)?;
        let attr_rules = grit_lib::crlf::load_gitattributes(wt);
        let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
        for (path, content) in &merge_result.conflict_files {
            let abs = wt.join(path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let output = if let Some(ref cfg) = config {
                let file_attrs = grit_lib::crlf::get_file_attrs(&attr_rules, path, false, cfg);
                let conv = grit_lib::crlf::ConversionConfig::from_config(cfg);
                grit_lib::crlf::convert_to_worktree_eager(
                    content,
                    path,
                    &conv,
                    &file_attrs,
                    None,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                content.clone()
            };
            std::fs::write(&abs, &output)?;
        }
    }

    if merge_result.has_conflicts {
        // The `git merge-recursive` plumbing command never invokes rerere; only the porcelain
        // `git merge` (and friends) do. Running rerere here would auto-resolve the working tree
        // and break callers that capture the raw conflict (t4200 'set up an unresolved merge').
        for desc in &merge_result.conflict_descriptions {
            if desc.kind == "binary" {
                println!("warning: Cannot merge binary files: {}", desc.subject_path);
                println!("Cannot merge binary files: {}", desc.subject_path);
            } else {
                println!("CONFLICT ({}): {}", desc.kind, desc.body);
            }
        }
        std::process::exit(1);
    }

    Ok(())
}

fn relocated_unmerged_entries_match(index: &grit_lib::index::Index, path: &[u8]) -> bool {
    let entries = index
        .entries
        .iter()
        .filter(|entry| entry.stage() != 0 && entry.path == path)
        .collect::<Vec<_>>();
    let Some(first) = entries.first() else {
        return false;
    };
    entries.len() > 1
        && entries
            .iter()
            .all(|entry| entry.oid == first.oid && entry.mode == first.mode)
}

/// Discover the repo for `merge-recursive` when using an alternate `GIT_WORK_TREE` (and
/// optional `GIT_INDEX_FILE`) that is a sibling of the main repo — walk up from the work
/// tree, not from cwd (t6430 empty work tree tests).
fn discover_repo_for_merge_recursive() -> Result<Repository> {
    if env::var("GIT_DIR").is_ok() {
        return Repository::discover(None).map_err(|e| e.into());
    }
    if let Ok(wt_raw) = env::var("GIT_WORK_TREE") {
        let cwd = env::current_dir()?;
        let wt = PathBuf::from(wt_raw);
        let wt_abs = if wt.is_absolute() { wt } else { cwd.join(wt) };
        return Repository::discover(Some(wt_abs.as_path())).map_err(|e| e.into());
    }
    Repository::discover(None).map_err(|e| e.into())
}

#[derive(Clone, Copy, Debug, Default)]
struct WhitespaceOptions {
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
}

struct MergeRecursiveParsed {
    ws: WhitespaceOptions,
    rename_options: MergeRenameOptions,
    positional: Vec<String>,
}

/// Parse `--find-renames` / `--rename-threshold` percentage like Git (optional `%`, capped at 100).
fn parse_merge_rename_percent(raw: &str) -> Result<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("invalid similarity index: {raw}");
    }
    let num = trimmed.strip_suffix('%').unwrap_or(trimmed);
    let value: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid similarity index: {raw}"))?;
    if value < 0 {
        bail!("invalid similarity index: {raw}");
    }
    Ok((value as u32).min(100))
}

fn parse_args(repo: &Repository, args: &[String]) -> Result<MergeRecursiveParsed> {
    let mut ws = WhitespaceOptions::default();
    let mut rename_options = MergeRenameOptions::from_config(repo);
    let mut positional = Vec::new();
    let mut end_of_options = false;

    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            i += 1;
            continue;
        }

        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "--ignore-space-change" | "-b" => ws.ignore_space_change = true,
                "--ignore-all-space" | "-w" => ws.ignore_all_space = true,
                "--ignore-space-at-eol" => ws.ignore_space_at_eol = true,
                "--ignore-cr-at-eol" => ws.ignore_cr_at_eol = true,
                "--patience" | "--histogram" | "--minimal" => { /* accepted, ignored */ }
                "--renormalize" | "--no-renormalize" => { /* accepted, ignored for now */ }
                "--no-renames" => {
                    rename_options.detect = false;
                }
                "--find-renames" => {
                    rename_options.detect = true;
                    rename_options.threshold = 50;
                }
                _ if arg.starts_with("--find-renames=") => {
                    let val = &arg["--find-renames=".len()..];
                    rename_options.detect = true;
                    rename_options.threshold = parse_merge_rename_percent(val)
                        .with_context(|| format!("bad --find-renames argument: {val}"))?;
                }
                _ if arg.starts_with("--rename-threshold=") => {
                    let val = &arg["--rename-threshold=".len()..];
                    rename_options.detect = true;
                    rename_options.threshold = parse_merge_rename_percent(val)
                        .with_context(|| format!("bad --rename-threshold argument: {val}"))?;
                }
                _ => bail!("unknown option: {arg}"),
            }
            i += 1;
            continue;
        }

        positional.push(arg.clone());
        i += 1;
    }

    Ok(MergeRecursiveParsed {
        ws,
        rename_options,
        positional,
    })
}

fn resolve_commit_oid(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = grit_lib::rev_parse::resolve_revision(repo, spec)
        .with_context(|| format!("unknown revision: {spec}"))?;
    let obj = repo.odb.read(&oid)?;
    if obj.kind != grit_lib::objects::ObjectKind::Commit {
        bail!("object {spec} is not a commit");
    }
    Ok(oid)
}

fn commit_tree(repo: &Repository, commit_oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(&commit_oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<IndexEntry>> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != grit_lib::objects::ObjectKind::Tree {
        bail!("expected tree object");
    }
    let entries = grit_lib::objects::parse_tree(&obj.data)?;
    let mut out = Vec::new();
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name).to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            out.extend(tree_to_index_entries(repo, &entry.oid, &path)?);
            continue;
        }
        let path_bytes = path.into_bytes();
        out.push(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: entry.mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid: entry.oid,
            flags: path_bytes.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path_bytes,
            base_index_pos: 0,
        });
    }
    Ok(out)
}

fn tree_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = HashMap::new();
    for entry in entries {
        out.insert(entry.path.clone(), entry);
    }
    out
}

fn checkout_entries(
    repo: &Repository,
    work_tree: &Path,
    index: &grit_lib::index::Index,
) -> Result<()> {
    let attr_rules = grit_lib::crlf::load_gitattributes(work_tree);
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
    let conv = config
        .as_ref()
        .map(grit_lib::crlf::ConversionConfig::from_config);

    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }

        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs = work_tree.join(&path_str);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let obj = repo.odb.read(&entry.oid)?;
        if obj.kind != grit_lib::objects::ObjectKind::Blob {
            continue;
        }

        if abs.is_dir() {
            std::fs::remove_dir_all(&abs)?;
        }

        if entry.mode == grit_lib::index::MODE_SYMLINK {
            let target = String::from_utf8(obj.data)
                .map_err(|_| anyhow::anyhow!("symlink target is not UTF-8"))?;
            if abs.exists() || abs.is_symlink() {
                let _ = std::fs::remove_file(&abs);
            }
            std::os::unix::fs::symlink(target, &abs)?;
        } else {
            let data = if let (Some(config), Some(conv)) = (&config, &conv) {
                let file_attrs =
                    grit_lib::crlf::get_file_attrs(&attr_rules, &path_str, false, config);
                let oid_hex = entry.oid.to_string();
                let smudge_meta =
                    grit_lib::filter_process::smudge_meta_for_checkout(repo, &oid_hex);
                grit_lib::crlf::convert_to_worktree_eager(
                    &obj.data,
                    &path_str,
                    conv,
                    &file_attrs,
                    None,
                    Some(&smudge_meta),
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                obj.data.clone()
            };
            std::fs::write(&abs, &data)?;
            if entry.mode == grit_lib::index::MODE_EXECUTABLE {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&abs)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&abs, perms)?;
            }
        }
    }

    Ok(())
}

fn remove_deleted_files(
    work_tree: &Path,
    old_entries: &HashMap<Vec<u8>, IndexEntry>,
    new_index: &grit_lib::index::Index,
) -> Result<()> {
    let new_paths: std::collections::HashSet<&[u8]> = new_index
        .entries
        .iter()
        .map(|entry| entry.path.as_slice())
        .collect();
    for path in old_entries.keys() {
        if new_paths.contains(path.as_slice()) {
            continue;
        }
        let rel = String::from_utf8_lossy(path);
        let abs = work_tree.join(rel.as_ref());
        if abs.exists() || abs.is_symlink() {
            let _ = std::fs::remove_file(&abs);
        }
    }
    Ok(())
}
