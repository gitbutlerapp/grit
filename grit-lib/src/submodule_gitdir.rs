//! Submodule gitdir paths when `extensions.submodulePathConfig` is enabled.
//!
//! Mirrors Git's `create_default_gitdir_config` / `validate_submodule_git_dir` logic
//! enough for upstream tests (encoded paths, nesting checks, conflict resolution).

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::{ConfigFile, ConfigScope};
use crate::error::{Error, Result};
use crate::hash;
use crate::index::Index;
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;

/// Filesystem path to the separate git directory for a submodule under `super_git_dir`.
///
/// Matches Git `submodule_name_to_gitdir` with `extensions.submodulePathConfig` off: append
/// `modules/<name>` by splitting only on `/` and `\\` (components like `..` are kept; no
/// normalization).
#[must_use]
pub fn submodule_modules_git_dir(super_git_dir: &Path, submodule_relpath: &str) -> PathBuf {
    let mut out = super_git_dir.to_path_buf();
    out.push("modules");
    for seg in submodule_relpath.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        out.push(seg);
    }
    out
}

/// Returns whether `extensions.submodulePathConfig` is enabled in `git_dir/config`.
///
/// `git_dir` should be the repository directory that holds `config` (the common git directory when
/// using linked worktrees).
pub fn submodule_path_config_enabled(git_dir: &Path) -> bool {
    let config_path = git_dir.join("config");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return false;
    };
    let mut in_extensions = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_extensions = trimmed.eq_ignore_ascii_case("[extensions]");
            continue;
        }
        if in_extensions {
            if let Some((k, v)) = trimmed.split_once('=') {
                if k.trim().eq_ignore_ascii_case("submodulepathconfig") {
                    return parse_bool(v.trim());
                }
            }
        }
    }
    false
}

fn parse_bool(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1")
}

fn is_rfc3986_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

fn is_casefolding_rfc3986_unreserved(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~')
}

fn percent_encode(name: &str, pred: fn(u8) -> bool) -> String {
    let mut out = String::new();
    for &b in name.as_bytes() {
        if pred(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02x}", b));
        }
    }
    out
}

/// Returns true if `path` looks like a git directory (`HEAD` and `objects/` exist).
pub fn is_git_directory(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir()
}

/// Rejects submodule paths that traverse symlinks (Git `validate_submodule_path`).
///
/// Path components follow Git `is_dir_sep`: on Unix only `/` separates; on Windows both `/`
/// and `\` do. A carriage return in a single segment (e.g. `sub\r`) must not be split.
pub fn validate_submodule_path(work_tree: &Path, rel: &str) -> Result<()> {
    if rel.is_empty() {
        return Err(Error::ConfigError("empty submodule path".into()));
    }
    let mut cur = work_tree.to_path_buf();
    #[cfg(windows)]
    let parts = rel.split(|c| c == '/' || c == '\\');
    #[cfg(not(windows))]
    let parts = rel.split('/');
    for comp in parts.filter(|s| !s.is_empty()) {
        cur.push(comp);
        let meta = match fs::symlink_metadata(&cur) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            return Err(Error::ConfigError(format!(
                "expected '{comp}' in submodule path '{rel}' not to be a symbolic link"
            )));
        }
    }
    Ok(())
}

fn last_modules_segment(git_dir_abs: &Path) -> Option<String> {
    let s = git_dir_abs.to_string_lossy();
    let marker = "/modules/";
    let mut p = 0usize;
    let mut last_start = None;
    while let Some(idx) = s[p..].find(marker) {
        let start = p + idx + marker.len();
        last_start = Some(start);
        p = start + 1;
    }
    last_start.map(|start| s[start..].to_string())
}

fn path_inside_other_gitdir(git_dir: &Path, submodule_name: &str) -> bool {
    submodule_gitdir_outer_conflict(git_dir, submodule_name).is_some()
}

/// When a proposed `.git/modules/<name>` path would sit inside another submodule's git dir
/// (e.g. names `hippo` and `hippo/hooks`), returns the outer git directory path (Git
/// `validate_submodule_legacy_git_dir`).
#[must_use]
pub fn submodule_gitdir_outer_conflict(git_dir: &Path, submodule_name: &str) -> Option<PathBuf> {
    let suffix = submodule_name.as_bytes();
    let gd = git_dir.to_string_lossy();
    let gd_bytes = gd.as_bytes();
    if gd_bytes.len() <= suffix.len() {
        return None;
    }
    let cut = gd_bytes.len() - suffix.len();
    if gd_bytes[cut - 1] != b'/' {
        return None;
    }
    if &gd_bytes[cut..] != suffix {
        return None;
    }
    for i in cut..gd_bytes.len() {
        if gd_bytes[i] == b'/' {
            let prefix = Path::new(std::str::from_utf8(&gd_bytes[..i]).unwrap_or(""));
            if is_git_directory(prefix) {
                return Some(prefix.to_path_buf());
            }
        }
    }
    None
}

fn resolve_gitdir_value(work_tree: &Path, gitdir_cfg: &str) -> PathBuf {
    let p = Path::new(gitdir_cfg.trim());
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_tree.join(p)
    }
}

fn canonical_abs(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn existing_gitdir_abs_paths(
    work_tree: &Path,
    cfg: &ConfigFile,
    except_name: &str,
) -> Result<HashSet<PathBuf>> {
    let mut set = HashSet::new();
    let suffix = ".gitdir";
    for e in &cfg.entries {
        if !e.key.starts_with("submodule.") || !e.key.ends_with(suffix) {
            continue;
        }
        let inner = &e.key["submodule.".len()..e.key.len() - suffix.len()];
        if inner == except_name {
            continue;
        }
        if let Some(v) = e.value.as_deref() {
            let abs = canonical_abs(&resolve_gitdir_value(work_tree, v));
            set.insert(abs);
        }
    }
    Ok(set)
}

fn gitdir_conflicts_with_existing(
    work_tree: &Path,
    cfg: &ConfigFile,
    abs_gitdir: &Path,
    submodule_name: &str,
) -> Result<bool> {
    let canon = canonical_abs(abs_gitdir);
    let existing = existing_gitdir_abs_paths(work_tree, cfg, submodule_name)?;
    Ok(existing.contains(&canon))
}

fn ignore_case_from_config(git_dir: &Path) -> bool {
    let config_path = git_dir.join("config");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return false;
    };
    let mut in_core = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_core = trimmed.eq_ignore_ascii_case("[core]");
            continue;
        }
        if in_core {
            if let Some((k, v)) = trimmed.split_once('=') {
                if k.trim().eq_ignore_ascii_case("ignorecase") {
                    return parse_bool(v.trim());
                }
            }
        }
    }
    false
}

fn fold_case_git_path(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn check_casefolding_conflict(
    proposed_abs: &Path,
    submodule_name: &str,
    suffixes_match: bool,
    taken_folded: &HashSet<String>,
) -> bool {
    let last = last_modules_segment(proposed_abs).unwrap_or_default();
    let folded_last = fold_case_git_path(&last);
    let folded_name = fold_case_git_path(submodule_name);
    if suffixes_match {
        taken_folded.contains(&folded_last)
    } else {
        taken_folded.contains(&folded_name) || taken_folded.contains(&folded_last)
    }
}

/// Validates a legacy submodule gitdir path (extension disabled): name suffix and no nesting clash.
pub fn validate_legacy_submodule_git_dir(git_dir: &Path, submodule_name: &str) -> Result<()> {
    let gd = git_dir.to_string_lossy();
    let suffix = submodule_name;
    if gd.len() <= suffix.len() {
        return Err(Error::ConfigError(
            "submodule name not a suffix of git dir".into(),
        ));
    }
    let cut = gd.len() - suffix.len();
    if gd
        .as_bytes()
        .get(cut.wrapping_sub(1))
        .is_none_or(|&b| b != b'/')
    {
        return Err(Error::ConfigError(
            "submodule name not a suffix of git dir".into(),
        ));
    }
    if &gd[cut..] != suffix {
        return Err(Error::ConfigError(
            "submodule name not a suffix of git dir".into(),
        ));
    }
    if path_inside_other_gitdir(git_dir, submodule_name) {
        return Err(Error::ConfigError(
            "submodule git dir inside another submodule git dir".into(),
        ));
    }
    Ok(())
}

/// Validates an encoded submodule gitdir path when `submodulePathConfig` is enabled.
pub fn validate_encoded_submodule_git_dir(
    work_tree: &Path,
    cfg: &ConfigFile,
    git_dir: &Path,
    submodule_name: &str,
    super_git_dir: &Path,
) -> Result<()> {
    let last = last_modules_segment(git_dir)
        .ok_or_else(|| Error::ConfigError("submodule gitdir missing /modules/ segment".into()))?;
    if last.contains('/') {
        return Err(Error::ConfigError(
            "encoded submodule gitdir must not contain '/' in module segment".into(),
        ));
    }
    if is_git_directory(git_dir)
        && gitdir_conflicts_with_existing(work_tree, cfg, git_dir, submodule_name)?
    {
        return Err(Error::ConfigError(
            "submodule gitdir conflicts with existing".into(),
        ));
    }
    if cfg!(unix) && ignore_case_from_config(super_git_dir) {
        let mut taken: HashSet<String> = HashSet::new();
        let suffix = ".gitdir";
        for e in &cfg.entries {
            if !e.key.starts_with("submodule.") || !e.key.ends_with(suffix) {
                continue;
            }
            let inner = &e.key["submodule.".len()..e.key.len() - suffix.len()];
            if inner == submodule_name {
                continue;
            }
            if let Some(v) = e.value.as_deref() {
                let abs = canonical_abs(&resolve_gitdir_value(work_tree, v));
                if let Some(seg) = last_modules_segment(&abs) {
                    taken.insert(fold_case_git_path(&seg));
                }
            }
        }
        let suffixes_match = last == submodule_name;
        if check_casefolding_conflict(git_dir, submodule_name, suffixes_match, &taken) {
            return Err(Error::ConfigError(
                "case-folding conflict for submodule gitdir".into(),
            ));
        }
    }
    Ok(())
}

fn repo_git_path_append(git_dir: &Path, tail: &str) -> PathBuf {
    let mut buf = git_dir.to_path_buf();
    if !tail.is_empty() {
        buf.push(tail);
    }
    buf
}

/// Returns the 40-character hex SHA-1 of a blob object for `data` (same as `git hash-object`).
pub fn hash_blob_sha1_hex(data: &[u8]) -> String {
    hash::hash_object(crate::objects::HashAlgo::Sha1, ObjectKind::Blob, data).to_hex()
}

/// Computes `submodule.<name>.gitdir` as a path relative to the work tree when not already set.
pub fn compute_default_submodule_gitdir(
    work_tree: &Path,
    git_dir: &Path,
    cfg: &ConfigFile,
    submodule_name: &str,
) -> Result<String> {
    let key = format!("submodule.{submodule_name}.gitdir");
    for e in &cfg.entries {
        if e.key == key {
            if let Some(v) = e.value.as_deref() {
                return Ok(v.to_string());
            }
        }
    }

    let try_set = |rel_under_git: &str| -> Option<String> {
        let abs = repo_git_path_append(git_dir, rel_under_git);
        if validate_encoded_submodule_git_dir(work_tree, cfg, &abs, submodule_name, git_dir)
            .is_err()
        {
            return None;
        }
        Some(format!(".git/{}", rel_under_git.replace('\\', "/")))
    };

    // Plain `modules/<name>` only when `name` has no directory separators: `PathBuf::push`
    // splits on `/`, so e.g. `nested/sub` would become `modules/nested/sub` and the encoded
    // validation rejects a multi-level tail under `modules/`.
    let rel_plain = format!("modules/{}", submodule_name.replace('\\', "/"));
    if !submodule_name.contains('/') && !submodule_name.contains('\\') {
        if let Some(v) = try_set(&rel_plain) {
            return Ok(v);
        }
    }

    let enc = percent_encode(submodule_name, is_rfc3986_unreserved);
    let rel_enc = format!("modules/{enc}");
    if let Some(v) = try_set(&rel_enc) {
        return Ok(v);
    }

    let enc_cf = percent_encode(submodule_name, is_casefolding_rfc3986_unreserved);
    let rel_cf = format!("modules/{enc_cf}");
    if let Some(v) = try_set(&rel_cf) {
        return Ok(v);
    }

    for c in b'0'..=b'9' {
        let rel = format!("modules/{}{}", enc, c as char);
        if let Some(v) = try_set(&rel) {
            return Ok(v);
        }
        let rel2 = format!("modules/{}{}", enc_cf, c as char);
        if let Some(v) = try_set(&rel2) {
            return Ok(v);
        }
    }

    let hex = hash_blob_sha1_hex(submodule_name.as_bytes());
    let rel_h = format!("modules/{hex}");
    if let Some(v) = try_set(&rel_h) {
        return Ok(v);
    }

    Err(Error::ConfigError(
        "failed to allocate submodule gitdir path".into(),
    ))
}

/// Ensures `submodule.<name>.gitdir` exists, writing it via [`compute_default_submodule_gitdir`] if needed.
pub fn ensure_submodule_gitdir_config(
    work_tree: &Path,
    git_dir: &Path,
    cfg: &mut ConfigFile,
    submodule_name: &str,
) -> Result<String> {
    let key = format!("submodule.{submodule_name}.gitdir");
    if let Some(existing) = cfg.entries.iter().find(|e| e.key == key) {
        if let Some(v) = existing.value.as_deref() {
            return Ok(v.to_string());
        }
    }
    let value = compute_default_submodule_gitdir(work_tree, git_dir, cfg, submodule_name)?;
    cfg.set(&key, &value)?;
    cfg.write()?;
    Ok(value)
}

/// Resolves the absolute filesystem path of a submodule's git directory.
pub fn submodule_gitdir_filesystem_path(
    work_tree: &Path,
    git_dir: &Path,
    cfg: &ConfigFile,
    submodule_name: &str,
) -> Result<PathBuf> {
    if submodule_path_config_enabled(git_dir) {
        let key = format!("submodule.{submodule_name}.gitdir");
        let value = cfg
            .entries
            .iter()
            .find(|e| e.key == key)
            .and_then(|e| e.value.clone())
            .ok_or_else(|| {
                Error::ConfigError(format!(
                    "submodule.{submodule_name}.gitdir is not set (submodulePathConfig enabled)"
                ))
            })?;
        Ok(resolve_gitdir_value(work_tree, &value))
    } else {
        Ok(git_dir.join("modules").join(submodule_name))
    }
}

/// Migrates legacy submodule dirs under `.git/modules/`: sets `submodule.*.gitdir` and enables the extension.
pub fn migrate_gitdir_configs(work_tree: &Path, git_dir: &Path) -> Result<()> {
    let modules_root = git_dir.join("modules");
    if !modules_root.is_dir() {
        return Ok(());
    }

    let config_path = git_dir.join("config");
    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;

    for entry in fs::read_dir(&modules_root).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "." || name_str == ".." {
            continue;
        }
        let gd_path = modules_root.join(&name);
        if !is_git_directory(&gd_path) {
            continue;
        }
        let key = format!("submodule.{name_str}.gitdir");
        if cfg.entries.iter().any(|e| e.key == key) {
            continue;
        }
        let _ = ensure_submodule_gitdir_config(work_tree, git_dir, &mut cfg, &name_str)?;
    }

    let mut repo_version = 0u32;
    if let Some(v) = cfg
        .entries
        .iter()
        .find(|e| e.key == "core.repositoryformatversion")
    {
        if let Some(s) = v.value.as_deref() {
            repo_version = s.parse().unwrap_or(0);
        }
    }
    if repo_version == 0 {
        cfg.set("core.repositoryformatversion", "1")?;
    }
    cfg.set("extensions.submodulePathConfig", "true")?;
    cfg.write()?;
    Ok(())
}

/// Returns true if `new_path` is strictly inside a gitlink path recorded in `index` (stage 0).
pub fn path_inside_indexed_submodule(index: &Index, new_path: &str) -> bool {
    let new_norm = new_path.replace('\\', "/");
    for e in &index.entries {
        if e.mode != 0o160000 || e.stage() != 0 {
            continue;
        }
        let ce = String::from_utf8_lossy(&e.path).replace('\\', "/");
        let ce_len = ce.len();
        if new_norm.len() <= ce_len {
            continue;
        }
        if new_norm.as_bytes().get(ce_len) != Some(&b'/') {
            continue;
        }
        if !new_norm.starts_with(&ce) {
            continue;
        }
        if new_norm.len() == ce_len + 1 {
            continue;
        }
        return true;
    }
    false
}

/// Returns true if `new_path` is under a submodule path declared in `.gitmodules`.
pub fn path_inside_registered_submodule(work_tree: &Path, new_path: &str) -> bool {
    let gitmodules = work_tree.join(".gitmodules");
    let Ok(content) = fs::read_to_string(&gitmodules) else {
        return false;
    };
    let Ok(mf) = ConfigFile::parse(&gitmodules, &content, ConfigScope::Local) else {
        return false;
    };
    let mut paths: Vec<String> = Vec::new();
    for e in &mf.entries {
        if e.key.starts_with("submodule.") && e.key.ends_with(".path") {
            if let Some(p) = e.value.as_deref() {
                paths.push(p.replace('\\', "/"));
            }
        }
    }
    let new_norm = new_path.replace('\\', "/");
    for p in paths {
        if new_norm == p || new_norm.starts_with(&format!("{p}/")) {
            return true;
        }
    }
    false
}

/// True when `new_path` is the same as or nested under a `.gitmodules` submodule **name** (the
/// `submodule.<name>.*` section name), which may contain `/`.
///
/// Git rejects such paths when `submodulePathConfig` is off (`die_path_inside_submodule` on
/// logical names). When the extension is enabled, encoded gitdirs lift this restriction.
pub fn path_inside_registered_submodule_name(work_tree: &Path, new_path: &str) -> bool {
    let gitmodules = work_tree.join(".gitmodules");
    let Ok(content) = fs::read_to_string(&gitmodules) else {
        return false;
    };
    let Ok(mf) = ConfigFile::parse(&gitmodules, &content, ConfigScope::Local) else {
        return false;
    };
    let mut names: Vec<String> = Vec::new();
    for e in &mf.entries {
        if !e.key.starts_with("submodule.") {
            continue;
        }
        let rest = &e.key["submodule.".len()..];
        if let Some(last_dot) = rest.rfind('.') {
            let name = rest[..last_dot].replace('\\', "/");
            if !name.is_empty() {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    let new_norm = new_path.replace('\\', "/");
    for n in names {
        if new_norm == n || new_norm.starts_with(&format!("{n}/")) {
            return true;
        }
    }
    false
}

/// Fails when `submodulePathConfig` is off and `new_path` would nest inside an existing submodule.
///
/// `index` is optional; when set, checked gitlinks match Git's `die_path_inside_submodule`.
pub fn die_path_inside_submodule_when_disabled(
    git_dir: &Path,
    work_tree: &Path,
    new_path: &str,
    index: Option<&Index>,
) -> Result<()> {
    if submodule_path_config_enabled(git_dir) {
        return Ok(());
    }
    if path_inside_registered_submodule(work_tree, new_path) {
        return Err(Error::ConfigError(
            "cannot add submodule: path inside existing submodule".into(),
        ));
    }
    if let Some(ix) = index {
        if path_inside_indexed_submodule(ix, new_path) {
            return Err(Error::ConfigError(
                "cannot add submodule: path inside existing submodule".into(),
            ));
        }
    }
    Ok(())
}

/// Sets `core.worktree` in the submodule repo at `modules_dir` via `grit --git-dir`.
///
/// Stores a path relative to `modules_dir` (e.g. `../../../sub1`), matching C Git and
/// `test_git_directory_exists` in the ported submodule tests.
pub fn set_submodule_repo_worktree(grit_bin: &Path, modules_dir: &Path, sub_worktree: &Path) {
    let wt_rel = pathdiff_relative(modules_dir, sub_worktree);
    let _ = std::process::Command::new(grit_bin)
        .arg("--git-dir")
        .arg(modules_dir)
        .arg("config")
        .arg("core.worktree")
        .arg(&wt_rel)
        .status();
}

/// Writes `sub_worktree/.git` as a gitfile pointing at `modules_dir` (relative when possible).
pub fn write_submodule_gitfile(sub_worktree: &Path, modules_dir: &Path) -> Result<()> {
    let rel = pathdiff_relative(sub_worktree, modules_dir);
    let line = format!("gitdir: {rel}\n");
    fs::write(sub_worktree.join(".git"), line).map_err(Error::Io)?;
    Ok(())
}

fn pathdiff_relative(from: &Path, to: &Path) -> String {
    let from_c = fs::canonicalize(from).unwrap_or_else(|_| from.to_path_buf());
    let to_c = fs::canonicalize(to).unwrap_or_else(|_| to.to_path_buf());
    let from_comp: Vec<Component<'_>> = from_c.components().collect();
    let to_comp: Vec<Component<'_>> = to_c.components().collect();
    let mut i = 0usize;
    while i < from_comp.len() && i < to_comp.len() && from_comp[i] == to_comp[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..from_comp.len() {
        out.push("..");
    }
    for c in &to_comp[i..] {
        out.push(c.as_os_str());
    }
    out.to_string_lossy().replace('\\', "/")
}

/// Writes the gitfile and `core.worktree` for a submodule using configured `submodule.<name>.gitdir`.
pub fn connect_submodule_work_tree_and_git_dir(
    grit_bin: &Path,
    work_tree: &Path,
    super_git_dir: &Path,
    cfg: &ConfigFile,
    submodule_name: &str,
    sub_worktree: &Path,
) -> Result<()> {
    let modules_dir =
        submodule_gitdir_filesystem_path(work_tree, super_git_dir, cfg, submodule_name)?;
    write_submodule_gitfile(sub_worktree, &modules_dir)?;
    set_submodule_repo_worktree(grit_bin, &modules_dir, sub_worktree);
    Ok(())
}

/// If `modules_dir/HEAD` is missing, sets it to `oid_hex` when that commit exists in `objects/`.
pub fn init_submodule_head_from_gitlink(modules_dir: &Path, oid_hex: &str) -> Result<()> {
    let head = modules_dir.join("HEAD");
    if head.exists() {
        return Ok(());
    }
    let obj_dir = modules_dir.join("objects");
    if !obj_dir.is_dir() {
        return Ok(());
    }
    let odb = Odb::new(&obj_dir);
    let oid = ObjectId::from_hex(oid_hex)?;
    let obj = odb.read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Ok(());
    }
    fs::write(&head, format!("{oid_hex}\n")).map_err(Error::Io)?;
    Ok(())
}

#[cfg(test)]
mod submodule_modules_git_dir_tests {
    use super::submodule_modules_git_dir;
    use std::path::Path;

    #[test]
    fn nested_path_under_single_modules_prefix() {
        let super_git = Path::new("/repo/.git");
        assert_eq!(
            submodule_modules_git_dir(super_git, "sub1/sub2"),
            Path::new("/repo/.git/modules/sub1/sub2")
        );
        assert_eq!(
            submodule_modules_git_dir(super_git, r"..\foo"),
            Path::new("/repo/.git/modules/../foo")
        );
    }

    #[test]
    fn single_segment_one_modules_join() {
        let super_git = Path::new("/repo/.git");
        assert_eq!(
            submodule_modules_git_dir(super_git, "sub1"),
            Path::new("/repo/.git/modules/sub1")
        );
    }
}
