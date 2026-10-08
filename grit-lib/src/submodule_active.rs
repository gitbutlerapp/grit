//! Submodule "active" state (`submodule.c` `is_submodule_active` parity).
//!
//! Used by `test-tool submodule is-active` and `git submodule add` when deciding
//! whether to write `submodule.<name>.active`.

use std::path::Path;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::index::{Index, MODE_GITLINK};
use crate::objects::ObjectKind;
use crate::pathspec::{matches_pathspec_with_context, PathspecMatchContext};
use crate::repo::Repository;
use crate::wildmatch::wildmatch;

/// Returns `true` when `submodule.active` is configured (any `submodule.active` entry exists).
fn config_has_submodule_active_key(cfg: &ConfigSet) -> bool {
    cfg.has_key("submodule.active")
}

/// Reads `submodule.active` patterns for [`submodule_add_should_set_active`].
///
/// Returns `Ok(None)` when the key is absent. Returns `Err` when a value is missing (bare key),
/// matching Git's `repo_config_get_string_multi` error path.
fn submodule_active_pattern_values(
    cfg: &ConfigSet,
) -> std::result::Result<Option<Vec<String>>, String> {
    if !config_has_submodule_active_key(cfg) {
        return Ok(None);
    }
    let values = cfg.get_all("submodule.active");
    if values.is_empty() {
        return Ok(Some(Vec::new()));
    }
    for v in &values {
        if v.is_empty() {
            return Err("missing value for 'submodule.active'".to_string());
        }
    }
    Ok(Some(values))
}

/// Whether `git submodule add` should write `submodule.<name>.active=true` to the local config.
///
/// Mirrors `submodule--helper.c` after registering `.gitmodules`: when `submodule.active` is
/// absent, or multi-string read fails, always set active; otherwise set active only when no
/// configured pattern [`wildmatch`]s the submodule path (flags `0`, matching Git).
#[must_use]
pub fn submodule_add_should_set_active(repo: &Repository, sm_path: &str) -> bool {
    let Ok(cfg) = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    ) else {
        return true;
    };
    let path = sm_path.replace('\\', "/");
    match submodule_active_pattern_values(&cfg) {
        Ok(None) => true,
        Err(_) => true,
        Ok(Some(patterns)) => {
            let matched = patterns
                .iter()
                .any(|p| wildmatch(p.trim().as_bytes(), path.as_bytes(), 0));
            !matched
        }
    }
}

fn read_gitmodules_text(
    repo: &Repository,
    work_tree: &Path,
) -> crate::error::Result<Option<String>> {
    let gitmodules_path = work_tree.join(".gitmodules");
    if gitmodules_path.exists() {
        let content = std::fs::read_to_string(&gitmodules_path).map_err(crate::error::Error::Io)?;
        return Ok(Some(content));
    }
    let index = repo.load_index()?;
    let Some(ie) = index.get(b".gitmodules", 0) else {
        return Ok(None);
    };
    let obj = repo.odb.read(&ie.oid)?;
    if obj.kind != ObjectKind::Blob {
        return Ok(None);
    }
    String::from_utf8(obj.data)
        .map(Some)
        .map_err(|e| crate::error::Error::ConfigError(format!("invalid .gitmodules utf-8: {e}")))
}

/// Resolve the submodule **logical name** for an index path (`.gitmodules` `submodule.<name>.path`).
///
/// Returns `None` when the path is not listed as a submodule in `.gitmodules` (work tree file or
/// index blob), matching Git's `submodule_from_path` failure.
pub fn submodule_name_for_path(
    repo: &Repository,
    path: &str,
) -> crate::error::Result<Option<String>> {
    let Some(wt) = repo.work_tree.as_ref() else {
        return Ok(None);
    };
    let Some(content) = read_gitmodules_text(repo, wt)? else {
        return Ok(None);
    };
    let config = ConfigFile::parse(&wt.join(".gitmodules"), &content, ConfigScope::Local)?;
    let path_norm = path.replace('\\', "/");

    #[derive(Default)]
    struct ModuleFields {
        path: Option<String>,
    }
    let mut modules: std::collections::BTreeMap<String, ModuleFields> =
        std::collections::BTreeMap::new();

    for entry in &config.entries {
        let key = &entry.key;
        if !key.starts_with("submodule.") {
            continue;
        }
        let rest = &key["submodule.".len()..];
        let Some(last_dot) = rest.rfind('.') else {
            continue;
        };
        let name = &rest[..last_dot];
        let var = &rest[last_dot + 1..];
        if var == "path" {
            modules.entry(name.to_string()).or_default().path = entry.value.clone();
        }
    }

    for (name, f) in modules {
        if let Some(p) = f.path {
            let p_norm = p.replace('\\', "/");
            if p_norm == path_norm {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

fn parse_pathspec_exclude(spec: &str) -> (bool, &str) {
    let s = spec.trim();
    if let Some(rest) = s.strip_prefix(":!") {
        return (true, rest);
    }
    if let Some(rest) = s.strip_prefix(":^") {
        return (true, rest);
    }
    if let Some(inner) = s.strip_prefix(":(exclude)") {
        return (true, inner);
    }
    if let Some(inner) = s.strip_prefix(":(exclude,") {
        if let Some(close) = inner.find(')') {
            return (true, &inner[close + 1..]);
        }
    }
    (false, s)
}

fn index_gitlink_match_for_path(index: &Index, path: &str) -> Option<PathspecMatchContext> {
    for e in &index.entries {
        if e.stage() != 0 {
            continue;
        }
        if e.mode != MODE_GITLINK {
            continue;
        }
        let name = String::from_utf8_lossy(&e.path);
        let name = name.replace('\\', "/");
        if name == path {
            return Some(PathspecMatchContext {
                is_directory: false,
                is_git_submodule: true,
            });
        }
    }
    None
}

fn spec_matches_submodule_path(
    _index: &Index,
    spec: &str,
    path: &str,
    ctx: PathspecMatchContext,
) -> bool {
    let (is_exclude, pattern_src) = parse_pathspec_exclude(spec);
    let pattern = pattern_src.trim();
    if pattern.is_empty() && !is_exclude {
        return false;
    }
    matches_pathspec_with_context(pattern, path, ctx)
}

/// Whether `path` matches `submodule.active` pathspecs using the given index (Git `match_pathspec`).
///
/// On missing string values (bare `submodule.active`), returns an error message suitable for stderr.
pub fn submodule_active_pathspec_match(
    index: &Index,
    specs: &[String],
    path: &str,
) -> std::result::Result<bool, String> {
    for v in specs {
        if v.is_empty() {
            return Err("error: missing value for 'submodule.active'".to_string());
        }
    }
    let ctx = index_gitlink_match_for_path(index, path).unwrap_or(PathspecMatchContext {
        is_directory: false,
        is_git_submodule: true,
    });

    let positive = specs
        .iter()
        .any(|s| !parse_pathspec_exclude(s).0 && spec_matches_submodule_path(index, s, path, ctx));
    if !positive {
        return Ok(false);
    }
    let excluded = specs
        .iter()
        .any(|s| parse_pathspec_exclude(s).0 && spec_matches_submodule_path(index, s, path, ctx));
    Ok(!excluded)
}

/// Git `is_submodule_active` for the current repository and index (`HEAD` + `.gitmodules` mapping).
///
/// Returns `Ok(true)` / `Ok(false)` for normal checks, or `Err` when `submodule.active` has a bare
/// entry (Git `config_error_nonbool`).
pub fn is_submodule_active(repo: &Repository, path: &str) -> std::result::Result<bool, String> {
    let path_norm = path.replace('\\', "/");
    let Some(name) = submodule_name_for_path(repo, &path_norm).map_err(|e| e.to_string())? else {
        return Ok(false);
    };

    let cfg = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )
    .map_err(|e| e.to_string())?;
    let per_key = format!("submodule.{name}.active");
    if let Some(res) = cfg.get_bool(&per_key) {
        let b = res.map_err(|_| format!("invalid boolean for '{per_key}'"))?;
        return Ok(b);
    }

    if config_has_submodule_active_key(&cfg) {
        let values = cfg.get_all("submodule.active");
        let index = repo.load_index().map_err(|e| e.to_string())?;
        return submodule_active_pathspec_match(&index, &values, &path_norm);
    }

    let url_key = format!("submodule.{name}.url");
    Ok(cfg.get(&url_key).is_some())
}
