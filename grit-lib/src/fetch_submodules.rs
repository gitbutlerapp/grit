//! Logic for `git fetch --recurse-submodules` (changed-submodule detection and config).
//!
//! Mirrors the subset of Git's `submodule.c` / `submodule-config.c` needed for recursive fetch:
//! revision walking with merge-aware gitlink diffs, per-submodule recurse mode, and checking
//! whether recorded gitlink commits are already present in a submodule repository.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::combined_tree_diff::{combined_diff_paths_filtered, CombinedTreeDiffOptions};
use crate::config::{parse_bool as config_parse_bool, ConfigFile, ConfigScope, ConfigSet};
use crate::diff::{diff_trees, DiffStatus};
use crate::error::Result;
use crate::index::MODE_GITLINK;
use crate::merge_diff::blob_oid_at_path;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::refs;
use crate::repo::Repository;
use crate::rev_list::{rev_list, RevListOptions};
use crate::submodule_gitdir::submodule_modules_git_dir;

/// `fetch.recurseSubmodules` / `--recurse-submodules` modes for fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchRecurseSubmodules {
    /// Use submodule / `.gitmodules` defaults (Git `RECURSE_SUBMODULES_DEFAULT`).
    Default,
    /// Never recurse.
    Off,
    /// Always recurse into configured submodules (`yes` / `true`).
    On,
    /// Only recurse when the superproject fetch brought in new submodule commits.
    OnDemand,
}

/// Parse `fetch.recurseSubmodules` or `--recurse-submodules=<value>` (Git `parse_fetch_recurse_submodules_arg`).
/// Build the positive OID list for `rev-list` when diffing fetched history (Git `ref_tips_after` ∪ submodule tips).
pub fn merge_tips_for_changed_walk(
    submodule_commits: &[ObjectId],
    tips_after: &[ObjectId],
) -> Vec<String> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for o in submodule_commits {
        if seen.insert(*o) {
            out.push(o.to_hex());
        }
    }
    for o in tips_after {
        if seen.insert(*o) {
            out.push(o.to_hex());
        }
    }
    out
}

pub fn parse_fetch_recurse_submodules_arg(
    opt: &str,
    arg: &str,
) -> std::result::Result<FetchRecurseSubmodules, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(format!("option `{opt}` requires a value"));
    }
    match config_parse_bool(arg) {
        Ok(true) => Ok(FetchRecurseSubmodules::On),
        Ok(false) => Ok(FetchRecurseSubmodules::Off),
        Err(_) => {
            if arg.eq_ignore_ascii_case("on-demand") {
                Ok(FetchRecurseSubmodules::OnDemand)
            } else if arg.eq_ignore_ascii_case("no") || arg.eq_ignore_ascii_case("false") {
                Ok(FetchRecurseSubmodules::Off)
            } else {
                Err(format!("bad {opt} argument: {arg}"))
            }
        }
    }
}

/// One submodule that gained new gitlink targets in `rev-list <tips> --not <neg>`.
#[derive(Debug, Clone)]
pub struct ChangedSubmoduleFetch {
    /// Submodule name (`.gitmodules` key or worktree path for unconfigured gitlinks).
    pub name: String,
    /// Path in the superproject tree.
    pub path: String,
    /// A superproject commit OID whose tree supplies `.gitmodules` / config context.
    pub super_oid: ObjectId,
    /// New gitlink commit OIDs observed along the walk (unique, sorted).
    pub new_commits: Vec<ObjectId>,
}

fn mode_from_octal(mode_str: &str) -> Option<u32> {
    u32::from_str_radix(mode_str, 8).ok()
}

fn is_gitlink_mode(mode_str: &str) -> bool {
    mode_from_octal(mode_str) == Some(MODE_GITLINK)
}

/// Map `submodule.<name>.path` -> name from a `.gitmodules` file body.
fn path_to_submodule_name(gitmodules_text: &str) -> HashMap<String, String> {
    let Ok(cfg) = ConfigFile::parse(
        Path::new(".gitmodules"),
        gitmodules_text,
        ConfigScope::Local,
    ) else {
        return HashMap::new();
    };
    let mut name_to_path: HashMap<String, String> = HashMap::new();
    for e in &cfg.entries {
        let key = &e.key;
        if !key.starts_with("submodule.") {
            continue;
        }
        let rest = &key["submodule.".len()..];
        let Some(last_dot) = rest.rfind('.') else {
            continue;
        };
        let name = rest[..last_dot].to_string();
        let var = &rest[last_dot + 1..];
        if var == "path" {
            if let Some(p) = e.value.as_deref() {
                name_to_path.insert(name, p.to_string());
            }
        }
    }
    name_to_path
        .into_iter()
        .map(|(name, path)| (path, name))
        .collect()
}

fn gitmodules_blob_text(odb: &Odb, commit_tree: &ObjectId) -> Option<String> {
    let oid = blob_oid_at_path(odb, commit_tree, ".gitmodules")?;
    let obj = odb.read(&oid).ok()?;
    if obj.kind != ObjectKind::Blob {
        return None;
    }
    String::from_utf8(obj.data).ok()
}

fn resolve_submodule_name_for_path(
    odb: &Odb,
    commit_tree: &ObjectId,
    path: &str,
    super_work_tree: Option<&Path>,
) -> Option<String> {
    if let Some(text) = gitmodules_blob_text(odb, commit_tree) {
        let m = path_to_submodule_name(&text);
        if let Some(n) = m.get(path) {
            return Some(n.clone());
        }
    }
    let wt_path = super_work_tree?.join(path);
    if wt_path.join(".git").exists() {
        return Some(path.to_string());
    }
    None
}

/// Walk `rev_list(repo, positive_hex, negative_hex)` and collect submodule names whose gitlink
/// targets changed, matching Git's `collect_changed_submodules` + name resolution.
pub fn collect_changed_submodules_for_fetch(
    repo: &Repository,
    positive_hex: &[String],
    negative_hex: &[String],
) -> Result<Vec<ChangedSubmoduleFetch>> {
    if positive_hex.is_empty() {
        return Ok(Vec::new());
    }
    let options = RevListOptions::default();
    let walked = rev_list(repo, positive_hex, negative_hex, &options)?;
    let odb = &repo.odb;
    let walk_opts = CombinedTreeDiffOptions {
        recursive: true,
        tree_in_recursive: false,
    };
    let super_wt = repo.work_tree.as_deref();

    let mut by_name: HashMap<String, ChangedSubmoduleFetch> = HashMap::new();

    for commit_oid in walked.commits {
        let obj = odb.read(&commit_oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        let parents = commit.parents;

        let mut record_gitlink =
            |path: String, oid: ObjectId, super_tree: &ObjectId| -> Result<()> {
                let Some(name) = resolve_submodule_name_for_path(odb, super_tree, &path, super_wt)
                else {
                    return Ok(());
                };
                by_name
                    .entry(name.clone())
                    .and_modify(|e| {
                        if !e.new_commits.contains(&oid) {
                            e.new_commits.push(oid);
                        }
                    })
                    .or_insert_with(|| ChangedSubmoduleFetch {
                        name,
                        path: path.clone(),
                        super_oid: commit_oid,
                        new_commits: vec![oid],
                    });
                Ok(())
            };

        if parents.is_empty() {
            let entries = diff_trees(odb, None, Some(&commit.tree), "")?;
            for e in entries {
                if !is_gitlink_mode(&e.new_mode) {
                    continue;
                }
                record_gitlink(e.path().to_string_lossy().into_owned(), e.new_oid, &commit.tree)?;
            }
        } else if parents.len() == 1 {
            let pobj = odb.read(&parents[0])?;
            if pobj.kind != ObjectKind::Commit {
                continue;
            }
            let parent = parse_commit(&pobj.data)?;
            let entries = diff_trees(odb, Some(&parent.tree), Some(&commit.tree), "")?;
            for e in entries {
                if !matches!(
                    e.status,
                    DiffStatus::Added
                        | DiffStatus::Modified
                        | DiffStatus::TypeChanged
                        | DiffStatus::Renamed
                ) {
                    continue;
                }
                let (mode, oid) = match e.status {
                    DiffStatus::Deleted => continue,
                    _ => (&e.new_mode, e.new_oid),
                };
                if !is_gitlink_mode(mode) {
                    continue;
                }
                let path = e
                    .new_path
                    .as_deref()
                    .or(e.old_path.as_deref())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if path.is_empty() {
                    continue;
                }
                record_gitlink(path, oid, &commit.tree)?;
            }
        } else {
            let paths =
                combined_diff_paths_filtered(odb, &commit.tree, &parents, &walk_opts, None)?;
            for p in paths {
                if (p.merge_mode & 0o170000) != MODE_GITLINK {
                    continue;
                }
                if p.merge_oid.is_zero() {
                    continue;
                }
                record_gitlink(p.path, p.merge_oid, &commit.tree)?;
            }
        }
    }

    let mut out: Vec<ChangedSubmoduleFetch> = by_name.into_values().collect();
    for e in &mut out {
        e.new_commits.sort();
        e.new_commits.dedup();
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// True when every OID in `commits` exists as a commit object in `sub_odb` and is reachable from
/// some ref (`git rev-list -n 1 <oids> --not --all` is empty), matching Git's `submodule_has_commits`.
pub fn submodule_has_all_commits(sub_odb: &Odb, commits: &[ObjectId]) -> Result<bool> {
    for oid in commits {
        let obj = match sub_odb.read(oid) {
            Ok(o) => o,
            Err(_) => return Ok(false),
        };
        if obj.kind != ObjectKind::Commit {
            return Ok(false);
        }
    }
    if commits.is_empty() {
        return Ok(true);
    }
    let repo_dir = sub_odb
        .objects_dir()
        .parent()
        .unwrap_or_else(|| sub_odb.objects_dir());
    let all_refs = refs::list_refs(repo_dir, "refs/")?;
    let mut reachable: HashSet<ObjectId> = HashSet::new();
    for (_, r_oid) in all_refs {
        let mut stack = vec![r_oid];
        while let Some(c) = stack.pop() {
            if !reachable.insert(c) {
                continue;
            }
            let Ok(obj) = sub_odb.read(&c) else {
                continue;
            };
            if obj.kind != ObjectKind::Commit {
                continue;
            }
            let Ok(parsed) = parse_commit(&obj.data) else {
                continue;
            };
            for p in parsed.parents {
                stack.push(p);
            }
        }
    }
    Ok(commits.iter().all(|c| reachable.contains(c)))
}

/// Whether a submodule at `path` is active for fetch at `super_oid` (Git `is_tree_submodule_active` subset).
pub fn is_submodule_active_for_fetch(
    _repo: &Repository,
    config: &ConfigSet,
    _super_tree_oid: &ObjectId,
    _path: &str,
    submodule_name: &str,
) -> bool {
    let active_key = format!("submodule.{submodule_name}.active");
    if let Some(v) = config.get(&active_key) {
        if let Ok(b) = config_parse_bool(v.trim()) {
            return b;
        }
    }
    let url_key = format!("submodule.{submodule_name}.url");
    config.get(&url_key).is_some()
}

/// Superproject has at least one submodule under `.git/modules/` (Git `repo_has_absorbed_submodules`).
pub fn repo_has_absorbed_submodules(super_git_dir: &Path) -> bool {
    let p = super_git_dir.join("modules");
    p.is_dir()
        && fs::read_dir(&p)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
}

/// `.gitmodules` says at least one submodule exists (path+url), or absorbed modules exist.
pub fn might_have_submodules_to_fetch(work_tree: &Path, super_git_dir: &Path) -> bool {
    if work_tree.join(".gitmodules").exists() {
        return true;
    }
    repo_has_absorbed_submodules(super_git_dir)
}

/// Open the git directory for a submodule at `rel_path` (work tree or `.git/modules/` fallback).
pub fn submodule_git_dir_for_fetch(super_repo: &Repository, rel_path: &str) -> Option<PathBuf> {
    let wt = super_repo.work_tree.as_ref()?;
    let abs = wt.join(rel_path);
    if abs.join(".git").exists() {
        if abs.join(".git").is_file() {
            let Ok(line) = fs::read_to_string(abs.join(".git")) else {
                return None;
            };
            let line = line.trim();
            let rest = line.strip_prefix("gitdir:")?.trim();
            let gd = if Path::new(rest).is_absolute() {
                PathBuf::from(rest)
            } else {
                abs.join(rest)
            };
            return fs::canonicalize(&gd).ok().or(Some(gd));
        }
        return Some(abs.join(".git"));
    }
    let modules = submodule_modules_git_dir(&super_repo.git_dir, rel_path);
    if modules.join("HEAD").exists() {
        return Some(modules);
    }
    None
}
