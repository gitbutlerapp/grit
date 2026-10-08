//! Submodule configuration cache (Git `submodule-config.c` subset for test-tool).
//!
//! Parses `.gitmodules` blobs keyed by blob OID and supports lookup by path or
//! logical submodule name, matching the behavior exercised by `t7411-submodule-config`.

use std::collections::HashMap;
use std::path::Path;

use crate::config::{canonical_key, ConfigFile, ConfigScope};
use crate::merge_diff::blob_oid_at_path;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::rev_parse::resolve_revision;

/// Resolved submodule identity for test output (`Submodule name: 'x' for path 'y'`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleInfo {
    /// Logical submodule name from `.gitmodules`.
    pub name: String,
    /// Checkout path relative to the superproject root.
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchRecurse {
    None,
    On,
    Off,
    OnDemand,
    Error,
}

#[derive(Clone, Debug)]
struct SubmoduleBuild {
    name: String,
    path: Option<String>,
    url: Option<String>,
    fetch_recurse: FetchRecurse,
}

impl SubmoduleBuild {
    fn new(name: String) -> Self {
        Self {
            name,
            path: None,
            url: None,
            fetch_recurse: FetchRecurse::None,
        }
    }
}

/// Cache of parsed `.gitmodules` blobs (by blob OID) plus path/name indexes.
#[derive(Default)]
pub struct SubmoduleConfigCache {
    by_blob: HashMap<ObjectId, Vec<SubmoduleBuild>>,
    path_index: HashMap<(ObjectId, String), String>,
    name_index: HashMap<(ObjectId, String), SubmoduleBuild>,
}

impl SubmoduleConfigCache {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks up a submodule by checkout path for the given treeish (commit or tree OID).
    ///
    /// `treeish` is `None` for the worktree / index / `HEAD` `.gitmodules` layer (Git null OID).
    pub fn submodule_from_path(
        &mut self,
        repo: &Repository,
        treeish: Option<(ObjectId, ObjectId)>,
        path: &str,
    ) -> Result<Option<SubmoduleInfo>, String> {
        let gm_oid = self.gitmodules_oid_for_treeish(repo, treeish)?;
        let gm_oid = match gm_oid {
            Some(o) => o,
            None => return Ok(None),
        };
        self.ensure_blob_parsed(repo, treeish, gm_oid)?;
        let key_path = norm_path_key(path);
        let name = self
            .path_index
            .get(&(gm_oid, key_path.clone()))
            .cloned()
            .or_else(|| self.path_index.get(&(gm_oid, path.to_string())).cloned());
        let Some(name) = name else {
            return Ok(None);
        };
        let path_out = self
            .name_index
            .get(&(gm_oid, name.clone()))
            .and_then(|b| b.path.clone())
            .unwrap_or_else(|| path.to_string());
        Ok(Some(SubmoduleInfo {
            name,
            path: path_out,
        }))
    }

    /// Looks up a submodule by its logical `.gitmodules` name.
    pub fn submodule_from_name(
        &mut self,
        repo: &Repository,
        treeish: Option<(ObjectId, ObjectId)>,
        name: &str,
    ) -> Result<Option<SubmoduleInfo>, String> {
        let gm_oid = self.gitmodules_oid_for_treeish(repo, treeish)?;
        let gm_oid = match gm_oid {
            Some(o) => o,
            None => return Ok(None),
        };
        self.ensure_blob_parsed(repo, treeish, gm_oid)?;
        let b = self.name_index.get(&(gm_oid, name.to_string())).cloned();
        let Some(b) = b else {
            return Ok(None);
        };
        let Some(path) = b.path.clone() else {
            return Ok(None);
        };
        Ok(Some(SubmoduleInfo { name: b.name, path }))
    }

    fn gitmodules_oid_for_treeish(
        &self,
        repo: &Repository,
        treeish: Option<(ObjectId, ObjectId)>,
    ) -> Result<Option<ObjectId>, String> {
        let Some((_rev, tree_oid)) = treeish else {
            return self.gitmodules_oid_worktree_index_head(repo);
        };
        Ok(blob_oid_at_path(&repo.odb, &tree_oid, ".gitmodules"))
    }

    /// Where to read `.gitmodules` for null treeish: disk, else index blob, else `HEAD` tree (Git `config_from_gitmodules`).
    fn gitmodules_oid_worktree_index_head(
        &self,
        repo: &Repository,
    ) -> Result<Option<ObjectId>, String> {
        let Some(wt) = repo.work_tree.as_ref() else {
            return Ok(None);
        };
        if wt.join(".gitmodules").exists() {
            return Ok(Some(ObjectId::zero()));
        }
        let index = repo.load_index().map_err(|e| e.to_string())?;
        if let Some(ie) = index.get(b".gitmodules", 0) {
            return Ok(Some(ie.oid));
        }
        let head_oid = crate::state::resolve_head(&repo.git_dir)
            .map_err(|e| e.to_string())?
            .oid()
            .copied();
        let Some(commit_oid) = head_oid else {
            return Ok(None);
        };
        let obj = repo.odb.read(&commit_oid).map_err(|e| e.to_string())?;
        if obj.kind != ObjectKind::Commit {
            return Ok(None);
        }
        let commit = parse_commit(&obj.data).map_err(|e| e.to_string())?;
        Ok(blob_oid_at_path(&repo.odb, &commit.tree, ".gitmodules"))
    }

    fn ensure_blob_parsed(
        &mut self,
        repo: &Repository,
        treeish: Option<(ObjectId, ObjectId)>,
        gitmodules_blob: ObjectId,
    ) -> Result<(), String> {
        if self.by_blob.contains_key(&gitmodules_blob) {
            return Ok(());
        }
        if gitmodules_blob.is_zero() {
            let Some(wt) = repo.work_tree.as_ref() else {
                self.by_blob.insert(gitmodules_blob, Vec::new());
                return Ok(());
            };
            let path = wt.join(".gitmodules");
            let text = if path.exists() {
                std::fs::read_to_string(&path).map_err(|e| e.to_string())?
            } else {
                let index = repo.load_index().map_err(|e| e.to_string())?;
                if let Some(ie) = index.get(b".gitmodules", 0) {
                    let obj = repo.odb.read(&ie.oid).map_err(|e| e.to_string())?;
                    if obj.kind != ObjectKind::Blob {
                        self.by_blob.insert(gitmodules_blob, Vec::new());
                        return Ok(());
                    }
                    String::from_utf8(obj.data).map_err(|e| e.to_string())?
                } else {
                    let head_oid = crate::state::resolve_head(&repo.git_dir)
                        .ok()
                        .and_then(|h| h.oid().copied());
                    let Some(commit_oid) = head_oid else {
                        self.by_blob.insert(gitmodules_blob, Vec::new());
                        return Ok(());
                    };
                    let obj = repo.odb.read(&commit_oid).map_err(|e| e.to_string())?;
                    if obj.kind != ObjectKind::Commit {
                        self.by_blob.insert(gitmodules_blob, Vec::new());
                        return Ok(());
                    }
                    let commit = parse_commit(&obj.data).map_err(|e| e.to_string())?;
                    let Some(blob_oid) = blob_oid_at_path(&repo.odb, &commit.tree, ".gitmodules")
                    else {
                        self.by_blob.insert(gitmodules_blob, Vec::new());
                        return Ok(());
                    };
                    let blob = repo.odb.read(&blob_oid).map_err(|e| e.to_string())?;
                    if blob.kind != ObjectKind::Blob {
                        self.by_blob.insert(gitmodules_blob, Vec::new());
                        return Ok(());
                    }
                    String::from_utf8(blob.data).map_err(|e| e.to_string())?
                }
            };
            self.ingest_gitmodules_blob(repo, None, None, ObjectId::zero(), &text, true)?;
            return Ok(());
        }
        let obj = repo
            .odb
            .read(&gitmodules_blob)
            .map_err(|e| format!("failed to read .gitmodules blob: {e}"))?;
        if obj.kind != ObjectKind::Blob {
            self.by_blob.insert(gitmodules_blob, Vec::new());
            return Ok(());
        }
        let text = String::from_utf8(obj.data).map_err(|e| e.to_string())?;
        let commit_for_warn = treeish.map(|(rev, _)| rev).filter(|o| !o.is_zero());
        self.ingest_gitmodules_blob(
            repo,
            commit_for_warn,
            treeish.map(|(rev, _)| rev),
            gitmodules_blob,
            &text,
            false,
        )?;
        Ok(())
    }

    fn ingest_gitmodules_blob(
        &mut self,
        repo: &Repository,
        treeish_for_warning: Option<ObjectId>,
        treeish_for_blob_spec: Option<ObjectId>,
        gitmodules_blob: ObjectId,
        content: &str,
        die_on_bad_fetch_recurse: bool,
    ) -> Result<(), String> {
        if self.by_blob.contains_key(&gitmodules_blob) {
            return Ok(());
        }

        let (git_entries, bad_line) = ConfigFile::parse_gitmodules_best_effort(
            Path::new(".gitmodules"),
            content,
            ConfigScope::Local,
        );
        if let Some(line) = bad_line {
            eprintln!(
                "{}",
                gitmodules_config_error(
                    repo,
                    treeish_for_blob_spec,
                    gitmodules_blob,
                    line,
                    "bad config",
                )
            );
        }

        let mut by_name: HashMap<String, SubmoduleBuild> = HashMap::new();

        for ent in &git_entries {
            let Some((name, var)) = submodule_name_and_var(&ent.key) else {
                continue;
            };
            if !check_submodule_name_ok(&name) {
                eprintln!(
                    "{}",
                    crate::diagnostics::warning_line("ignoring suspicious submodule name: {name}")
                );
                continue;
            }
            let entry = by_name
                .entry(name.clone())
                .or_insert_with(|| SubmoduleBuild::new(name.clone()));

            match var.as_str() {
                "path" => {
                    let Some(value) = ent.value.as_deref() else {
                        return Err(gitmodules_config_error(
                            repo,
                            treeish_for_blob_spec,
                            gitmodules_blob,
                            ent.line,
                            "bad config",
                        ));
                    };
                    if crate::gitmodules::looks_like_command_line_option(value) {
                        eprintln!(
                            "{}",
                            crate::diagnostics::warning_line(&format!(
                                "ignoring '{}' which may be interpreted as a command-line option: {value}",
                                ent.key
                            ))
                        );
                        continue;
                    }
                    let overwrite = gitmodules_blob.is_zero();
                    if entry.path.is_some() && !overwrite {
                        warn_multiple_config(treeish_for_warning, &entry.name, "path");
                    } else {
                        if let Some(old) = &entry.path {
                            self.path_index_remove(gitmodules_blob, old);
                        }
                        entry.path = Some(value.to_string());
                        self.path_index_insert(gitmodules_blob, value, entry.name.clone());
                    }
                }
                "url" => {
                    let Some(value) = ent.value.as_deref() else {
                        return Err(gitmodules_config_error(
                            repo,
                            treeish_for_blob_spec,
                            gitmodules_blob,
                            ent.line,
                            "bad config",
                        ));
                    };
                    if crate::gitmodules::looks_like_command_line_option(value) {
                        eprintln!(
                            "{}",
                            crate::diagnostics::warning_line(&format!(
                                "ignoring '{}' which may be interpreted as a command-line option: {value}",
                                ent.key
                            ))
                        );
                        continue;
                    }
                    let overwrite = gitmodules_blob.is_zero();
                    if entry.url.is_some() && !overwrite {
                        warn_multiple_config(treeish_for_warning, &entry.name, "url");
                    } else {
                        entry.url = Some(value.to_string());
                    }
                }
                "fetchrecursesubmodules" => {
                    let value = ent.value.as_deref().unwrap_or("");
                    let parsed = parse_fetch_recurse(value, die_on_bad_fetch_recurse);
                    let parsed = parsed?;
                    let overwrite = gitmodules_blob.is_zero();
                    if entry.fetch_recurse != FetchRecurse::None && !overwrite {
                        warn_multiple_config(
                            treeish_for_warning,
                            &entry.name,
                            "fetchrecursesubmodules",
                        );
                    } else {
                        entry.fetch_recurse = parsed;
                    }
                }
                "ignore" => {
                    let Some(value) = ent.value.as_deref() else {
                        return Err(gitmodules_config_error(
                            repo,
                            treeish_for_blob_spec,
                            gitmodules_blob,
                            ent.line,
                            "bad config",
                        ));
                    };
                    let _ = value;
                }
                "branch" => {
                    let Some(_value) = ent.value.as_deref() else {
                        return Err(gitmodules_config_error(
                            repo,
                            treeish_for_blob_spec,
                            gitmodules_blob,
                            ent.line,
                            "bad config",
                        ));
                    };
                }
                "update" | "shallow" => {}
                _ => {}
            }
        }

        let list: Vec<SubmoduleBuild> = by_name.into_values().collect();
        for b in &list {
            self.name_index
                .insert((gitmodules_blob, b.name.clone()), b.clone());
        }
        self.by_blob.insert(gitmodules_blob, list);
        Ok(())
    }

    fn path_index_insert(&mut self, blob: ObjectId, path: &str, name: String) {
        let key = norm_path_key(path);
        self.path_index.insert((blob, key), name);
    }

    fn path_index_remove(&mut self, blob: ObjectId, path: &str) {
        let key = norm_path_key(path);
        self.path_index.remove(&(blob, key));
    }

    /// Prints all values for `key` (canonical submodule config key) from the nested
    /// submodule repository at `super_path` / `submodule_path`.
    pub fn print_config_from_nested_gitmodules(
        _super_repo: &Repository,
        super_work_tree: &Path,
        submodule_path: &str,
        key: &str,
    ) -> Result<(), String> {
        let wanted = canonical_key(key).map_err(|e| e.to_string())?;
        let sub_work = super_work_tree.join(submodule_path);
        let sub_git = if sub_work.join(".git").is_file() {
            let gf = std::fs::read_to_string(sub_work.join(".git"))
                .map_err(|e| format!("read gitfile: {e}"))?;
            let line = gf.lines().next().unwrap_or("").trim();
            let Some(rest) = line.strip_prefix("gitdir:") else {
                return Err("invalid gitfile".into());
            };
            let rest = rest.trim();
            let p = Path::new(rest);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                sub_work.join(rest)
            }
        } else {
            sub_work.join(".git")
        };
        let sub_repo = Repository::open(&sub_git, Some(&sub_work))
            .map_err(|e| format!("open submodule repo: {e}"))?;

        let gm_path = sub_work.join(".gitmodules");
        let (content, _) = if gm_path.exists() {
            let c = std::fs::read_to_string(&gm_path).map_err(|e| e.to_string())?;
            (c, gm_path)
        } else {
            let index = sub_repo.load_index().map_err(|e| e.to_string())?;
            if let Some(ie) = index.get(b".gitmodules", 0) {
                let obj = sub_repo.odb.read(&ie.oid).map_err(|e| e.to_string())?;
                if obj.kind != ObjectKind::Blob {
                    return Ok(());
                }
                let c = String::from_utf8(obj.data).map_err(|e| e.to_string())?;
                (c, gm_path)
            } else {
                let head_oid = crate::state::resolve_head(&sub_repo.git_dir)
                    .ok()
                    .and_then(|h| h.oid().copied());
                let Some(commit_oid) = head_oid else {
                    return Ok(());
                };
                let obj = sub_repo.odb.read(&commit_oid).map_err(|e| e.to_string())?;
                if obj.kind != ObjectKind::Commit {
                    return Ok(());
                }
                let commit = parse_commit(&obj.data).map_err(|e| e.to_string())?;
                let Some(blob_oid) = blob_oid_at_path(&sub_repo.odb, &commit.tree, ".gitmodules")
                else {
                    return Ok(());
                };
                let blob = sub_repo.odb.read(&blob_oid).map_err(|e| e.to_string())?;
                if blob.kind != ObjectKind::Blob {
                    return Ok(());
                }
                let c = String::from_utf8(blob.data).map_err(|e| e.to_string())?;
                (c, gm_path)
            }
        };

        let cfg = ConfigFile::parse(Path::new(".gitmodules"), &content, ConfigScope::Local)
            .map_err(|e| e.to_string())?;
        for e in &cfg.entries {
            if e.key == wanted {
                if let Some(v) = &e.value {
                    println!("{v}");
                }
            }
        }
        Ok(())
    }
}

fn norm_path_key(path: &str) -> String {
    path.replace('\\', "/")
}

fn warn_multiple_config(treeish: Option<ObjectId>, name: &str, option: &str) {
    let commit_string = treeish
        .map(|o| o.to_hex())
        .unwrap_or_else(|| "WORKTREE".to_string());
    eprintln!(
        "{}",
        crate::diagnostics::warning_line(&format!(
            "{commit_string}:.gitmodules, multiple configurations found for \
'submodule.{name}.{option}'. Skipping second one!"
        ))
    );
}

fn gitmodules_config_error(
    repo: &Repository,
    treeish_for_blob: Option<ObjectId>,
    gitmodules_blob: ObjectId,
    line: usize,
    msg: &str,
) -> String {
    if gitmodules_blob.is_zero() {
        format!("{msg} line {line} in file .gitmodules")
    } else {
        let spec = submodule_blob_spec(repo, treeish_for_blob, gitmodules_blob);
        format!("{msg} line {line} in submodule-blob {spec}")
    }
}

/// Git names submodule-blob config sources as `<commit>:.gitmodules` (see `gitmodule_oid_from_commit`).
fn submodule_blob_spec(
    repo: &Repository,
    treeish_for_blob: Option<ObjectId>,
    blob: ObjectId,
) -> String {
    let fallback = format!("{}:.gitmodules", blob.to_hex());
    let Some(treeish) = treeish_for_blob else {
        return fallback;
    };
    let Ok(obj) = repo.odb.read(&treeish) else {
        return fallback;
    };
    let commit_oid = match obj.kind {
        ObjectKind::Commit => treeish,
        ObjectKind::Tree => {
            let Ok(c) = find_commit_containing_tree(repo, treeish) else {
                return fallback;
            };
            c
        }
        _ => return fallback,
    };
    format!("{}:.gitmodules", commit_oid.to_hex())
}

fn find_commit_containing_tree(repo: &Repository, tree_oid: ObjectId) -> Result<ObjectId, ()> {
    let mut stack = vec!["HEAD^{commit}".to_owned()];
    for name in ["HEAD", "refs/heads/master", "refs/heads/main"] {
        stack.push(name.into());
    }
    for spec in stack {
        let Ok(oid) = resolve_revision(repo, spec.as_str()) else {
            continue;
        };
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let Ok(c) = parse_commit(&obj.data) else {
            continue;
        };
        if tree_contains_oid(&repo.odb, c.tree, tree_oid)? {
            return Ok(oid);
        }
    }
    Err(())
}

fn tree_contains_oid(odb: &Odb, tree: ObjectId, target: ObjectId) -> Result<bool, ()> {
    let obj = odb.read(&tree).map_err(|_| ())?;
    if obj.kind != ObjectKind::Tree {
        return Ok(false);
    }
    let entries = crate::objects::parse_tree(&obj.data).map_err(|_| ())?;
    for e in entries {
        if e.oid == target {
            return Ok(true);
        }
        if e.mode == 0o040000 && tree_contains_oid(odb, e.oid, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn submodule_name_and_var(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("submodule.")?;
    let dot = rest.rfind('.')?;
    let name = rest[..dot].to_string();
    let var = rest[dot + 1..].to_string();
    if name.is_empty() {
        return None;
    }
    Some((name, var))
}

fn check_submodule_name_ok(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let b = name.as_bytes();
    if b.len() >= 2
        && b[0] == b'.'
        && b[1] == b'.'
        && (b.len() == 2 || b[2] == b'/' || b[2] == b'\\')
    {
        return false;
    }
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if c == b'/' || c == b'\\' {
            let j = i;
            if b.len() >= j + 2
                && b[j] == b'.'
                && b[j + 1] == b'.'
                && (j + 2 >= b.len() || b[j + 2] == b'/' || b[j + 2] == b'\\')
            {
                return false;
            }
        }
    }
    true
}

fn parse_fetch_recurse(value: &str, die_on_error: bool) -> Result<FetchRecurse, String> {
    let v = value.trim();
    match crate::config::parse_bool(v) {
        Ok(true) => return Ok(FetchRecurse::On),
        Ok(false) => return Ok(FetchRecurse::Off),
        Err(_) => {}
    }
    if v.eq_ignore_ascii_case("on-demand") {
        return Ok(FetchRecurse::OnDemand);
    }
    if die_on_error {
        Err(crate::diagnostics::fatal_line(&format!(
            "bad submodule.fetchRecurseSubmodules argument: '{v}'"
        )))
    } else {
        Ok(FetchRecurse::Error)
    }
}
