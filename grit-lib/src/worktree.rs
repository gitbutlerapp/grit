//! Linked worktree registry: discovery, listing, and admin-dir layout.
//!
//! Git stores linked worktrees under `<common-git-dir>/worktrees/<id>/` with
//! `gitdir`, `commondir`, `HEAD`, and optional `locked` / `prunable` files.
//! This module reads that layout; lifecycle mutations (`add` / `remove`) remain
//! in the CLI for now and will move here in Phase 1.2.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::repo::{common_git_dir_for_config, Repository};
use crate::state::{resolve_head, HeadState};

/// One row returned by [`list_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Absolute path to the working tree (for bare main worktree, the common git dir).
    pub path: PathBuf,
    /// Resolved HEAD for this worktree.
    pub head: HeadState,
    /// Whether this worktree is bare (only the main entry can be bare).
    pub is_bare: bool,
    /// True when `<admin>/locked` exists.
    pub is_locked: bool,
    /// Contents of the `locked` file when non-empty.
    pub lock_reason: Option<String>,
    /// Administrative directory: common git dir for main, else `worktrees/<id>/`.
    pub admin_dir: PathBuf,
}

/// Shared git directory for `git_dir` (follows `commondir` for linked worktrees).
#[must_use]
pub fn common_git_dir(git_dir: &Path) -> PathBuf {
    common_git_dir_for_config(git_dir)
}

/// Resolve HEAD for a linked worktree admin dir (`HEAD` local, branch refs in `common`).
#[must_use]
pub fn resolve_linked_head(admin: &Path, _common: &Path) -> HeadState {
    resolve_head(admin).unwrap_or(HeadState::Invalid)
}

/// Number of registered worktrees (main + linked entries under `worktrees/`).
#[must_use]
pub fn registered_worktree_count(common: &Path) -> usize {
    let worktrees_dir = common.join("worktrees");
    if !worktrees_dir.is_dir() {
        return 1;
    }
    let linked = fs::read_dir(&worktrees_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .count();
    1 + linked
}

/// Whether `common` is configured as a bare repository (`core.bare=true`).
#[must_use]
pub fn is_bare_repository(common: &Path) -> bool {
    ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(common),
        true,
    )
    .ok()
    .and_then(|cfg| cfg.get_bool("core.bare"))
    .and_then(|r| r.ok())
    .unwrap_or_else(|| {
        // Heuristic when config is missing: bare repos usually are not named `.git`.
        !common.ends_with(".git") && common.join("config").is_file()
    })
}

/// Enumerate the main and linked worktrees for `repo`.
///
/// Order matches Git: main worktree first, then linked worktrees sorted by admin id.
pub fn list_worktrees(repo: &Repository) -> Result<Vec<WorktreeEntry>> {
    let common = common_git_dir(&repo.git_dir);
    let mut entries = Vec::new();

    let bare = is_bare_repository(&common);
    let main_path = if bare {
        common.clone()
    } else if let Some(wt) = repo.work_tree.as_ref() {
        // When opened from the main worktree, use the discovered work tree path.
        if repo.git_dir == common || !repo.git_dir.starts_with(common.join("worktrees")) {
            wt.clone()
        } else {
            common.parent().unwrap_or(&common).to_path_buf()
        }
    } else {
        common.parent().unwrap_or(&common).to_path_buf()
    };

    let main_head = resolve_head(&common).unwrap_or(HeadState::Invalid);
    entries.push(WorktreeEntry {
        path: main_path,
        head: main_head,
        is_bare: bare,
        is_locked: false,
        lock_reason: None,
        admin_dir: common.clone(),
    });

    let worktrees_dir = common.join("worktrees");
    if !worktrees_dir.is_dir() {
        return Ok(entries);
    }

    let mut names: Vec<String> = fs::read_dir(&worktrees_dir)
        .map_err(Error::Io)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    for name in names {
        let admin = worktrees_dir.join(&name);
        let wt_head = resolve_linked_head(&admin, &common);
        let wt_path = read_worktree_path(&admin)?;
        let (is_locked, lock_reason) = read_lock_state(&admin)?;
        entries.push(WorktreeEntry {
            path: wt_path,
            head: wt_head,
            is_bare: false,
            is_locked,
            lock_reason,
            admin_dir: admin,
        });
    }

    Ok(entries)
}

/// Last path component of `path`, without trailing directory separators (Git `worktree_basename`).
#[must_use]
pub fn worktree_path_basename(path: &Path) -> String {
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_owned()
}

/// Sanitize a path basename for use as `worktrees/<id>/` (Git `sanitize_refname_component`).
#[must_use]
pub fn sanitize_worktree_id_component(name: &str) -> String {
    if name == "@" {
        return "-".to_owned();
    }

    let mut out = String::new();
    let mut last = '\0';
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_ascii_control()
            || matches!(ch, ':' | '?' | '[' | '\\' | '^' | '~' | ' ' | '\t' | '*')
        {
            if (out.is_empty() && last != '-') || !out.is_empty() {
                out.push('-');
            }
            last = '-';
            i += 1;
            continue;
        }
        if ch == '.' && i + 1 < chars.len() && chars[i + 1] == '.' {
            if last == '.' {
                out.pop();
            } else {
                out.push('.');
                last = '.';
            }
            i += 2;
            continue;
        }
        if ch == '@' && i + 1 < chars.len() && chars[i + 1] == '{' {
            if let Some(last_ch) = out.pop() {
                if last_ch != '-' {
                    out.push('-');
                }
            }
            last = '-';
            i += 2;
            continue;
        }
        if ch == '.' && out.is_empty() {
            out.push('-');
            last = '-';
            i += 1;
            continue;
        }
        out.push(ch);
        last = ch;
        i += 1;
    }

    const LOCK_SUFFIX: &str = ".lock";
    while out.ends_with(LOCK_SUFFIX) {
        out.truncate(out.len() - LOCK_SUFFIX.len());
    }
    while out.ends_with('.') {
        out.pop();
    }
    out
}

/// Pick a unique `<common>/worktrees/<id>/` directory for a new linked worktree at `wt_path`.
///
/// Git uses the sanitized basename and appends `1`, `2`, … when the admin dir already exists.
#[must_use]
pub fn allocate_worktree_admin_dir(common: &Path, wt_path: &Path) -> PathBuf {
    let worktrees_dir = common.join("worktrees");
    let base = sanitize_worktree_id_component(&worktree_path_basename(wt_path));
    let base = if base.is_empty() {
        "worktree".to_owned()
    } else {
        base
    };

    let mut counter = 0u32;
    loop {
        let id = if counter == 0 {
            base.clone()
        } else {
            format!("{base}{counter}")
        };
        let admin = worktrees_dir.join(&id);
        if !admin.exists() {
            return admin;
        }
        counter = counter.saturating_add(1);
        if counter == 0 {
            break;
        }
    }
    worktrees_dir.join(format!("{base}{}", std::process::id()))
}

/// Copy `config.worktree` into a linked worktree admin dir, stripping keys Git omits
/// when `extensions.worktreeConfig` is enabled (`core.bare`, `core.worktree`).
pub fn copy_filtered_worktree_config(source_git_dir: &Path, admin_dir: &Path) -> Result<()> {
    let src = source_git_dir.join("config.worktree");
    if !src.is_file() {
        return Ok(());
    }
    let dst = admin_dir.join("config.worktree");
    fs::copy(&src, &dst).map_err(Error::Io)?;
    strip_worktree_config_keys(&dst, &["core.bare", "core.worktree"])?;
    Ok(())
}

fn strip_worktree_config_keys(path: &Path, keys: &[&str]) -> Result<()> {
    let content = fs::read_to_string(path).map_err(Error::Io)?;
    let mut kept = Vec::new();
    let mut section: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            kept.push(line);
            continue;
        }
        if trimmed.starts_with('[') {
            let end = trimmed.find(']').unwrap_or(trimmed.len());
            let name = trimmed[1..end].trim().to_ascii_lowercase();
            section = Some(name);
            kept.push(line);
            continue;
        }
        if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let full = match section.as_deref() {
                Some(sec) => format!("{sec}.{key}"),
                None => key.clone(),
            };
            if keys.iter().any(|k| full.eq_ignore_ascii_case(k)) {
                continue;
            }
        } else if keys.iter().any(|k| trimmed.eq_ignore_ascii_case(k)) {
            continue;
        }
        kept.push(line);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(path, out).map_err(Error::Io)
}

/// Read the working tree path from `<admin>/gitdir` (parent of the worktree `.git` file).
pub fn read_worktree_path(admin: &Path) -> Result<PathBuf> {
    let gitdir_path = admin.join("gitdir");
    if !gitdir_path.is_file() {
        return Ok(admin.to_path_buf());
    }
    let raw = fs::read_to_string(&gitdir_path).map_err(Error::Io)?;
    let mut p = PathBuf::from(raw.trim());
    if p.is_relative() {
        p = admin.join(p);
    }
    let parent = p.parent().unwrap_or(&p).to_path_buf();
    Ok(parent.canonicalize().unwrap_or(parent))
}

fn read_lock_state(admin: &Path) -> Result<(bool, Option<String>)> {
    let locked_file = admin.join("locked");
    if !locked_file.is_file() {
        return Ok((false, None));
    }
    let content = fs::read_to_string(&locked_file).map_err(Error::Io)?;
    let reason = content.trim();
    if reason.is_empty() {
        Ok((true, None))
    } else {
        Ok((true, Some(reason.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repository;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn list_main_worktree_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();

        let repo = Repository::open(&root.join(".git"), Some(&root)).unwrap();
        let list = list_worktrees(&repo).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, root.canonicalize().unwrap());
        assert!(!list[0].is_bare);
    }

    #[test]
    fn allocate_unique_worktree_id() {
        let tmp = TempDir::new().unwrap();
        let common = tmp.path().join("git");
        fs::create_dir_all(common.join("worktrees/here")).unwrap();
        let admin = allocate_worktree_admin_dir(&common, Path::new("/tmp/sub/here"));
        assert_eq!(admin, common.join("worktrees/here1"));
    }

    #[test]
    fn strip_worktree_config_removes_core_bare_and_worktree() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.worktree");
        fs::write(
            &path,
            "[core]\n\tbare = true\n\tworktree = /wt\n[bogus]\n\tkey = value\n",
        )
        .unwrap();
        strip_worktree_config_keys(&path, &["core.bare", "core.worktree"]).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("bogus"));
        assert!(!out.contains("bare"));
        assert!(!out.contains("worktree"));
    }

    #[test]
    fn sanitize_funny_worktree_name() {
        assert_eq!(
            sanitize_worktree_id_component(".  weird*..?.lock.lock"),
            "---weird-.-"
        );
    }

    #[test]
    fn read_worktree_path_from_gitdir_file() {
        let tmp = TempDir::new().unwrap();
        let admin = tmp.path().join("wt-admin");
        fs::create_dir_all(&admin).unwrap();
        let wt = tmp.path().join("linked");
        fs::create_dir_all(wt.join(".git")).unwrap();
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        let path = read_worktree_path(&admin).unwrap();
        assert_eq!(path, wt.canonicalize().unwrap());
    }
}
