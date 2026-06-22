//! `git checkout` worktree-apply primitives.
//!
//! `checkout` is a large worktree mutator. Its CLI shell
//! (`grit/src/commands/checkout.rs`) still owns argv/clap parsing,
//! branch-switch messaging, hook dispatch, progress-to-stderr, and the
//! detached-HEAD advice text. This module holds the **pure worktree-apply
//! primitives** that shell calls into: writing a blob's bytes to a working-tree
//! path (handling symlinks, executable bits, and parent-directory
//! preparation), removing now-empty parent directories, and the simple glob
//! matcher used to resolve interactive-patch path filters.
//!
//! These functions compute and apply worktree changes from index/object data
//! and make no presentation decisions — no colour, pager, tty, or stdout.

use std::path::Path;

use crate::diff::{diff_trees, DiffStatus};
use crate::error::{Error, Result};
use crate::index::{entry_from_stat, MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
use crate::objects::ObjectId;
use crate::repo::Repository;

/// Apply the change from tree `from` to tree `to` onto the working tree and
/// index, then write the index.
///
/// This is the reusable core of a clean branch switch / fast-forward: it writes,
/// updates, and deletes only the paths that differ between the two trees, and
/// rebuilds the matching index entries (with fresh stat data so a subsequent
/// status reports a clean tree). Unchanged paths are left untouched.
///
/// The caller is responsible for policy: this function assumes the tracked
/// working tree is **clean** relative to `from` (no staged or unstaged changes
/// that would be silently overwritten) and that no untracked file sits where an
/// added path needs to land. It performs the mechanical apply only.
pub fn checkout_between_trees(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: &ObjectId,
) -> Result<()> {
    let work_tree = repo
        .work_tree
        .clone()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    let changes = diff_trees(&repo.odb, from, Some(to), "")?;
    let mut index = repo.load_index()?;

    for change in &changes {
        if change.status == DiffStatus::Deleted {
            if let Some(path) = &change.old_path {
                // TODO(byte-paths): worktree write/stat paths route through the &str
                // worktree API and stay lossy until Phase 2 migrates working-tree I/O.
                let path = path.to_string_lossy();
                let abs = work_tree.join(&*path);
                let _ = std::fs::remove_file(&abs);
                remove_empty_parent_dirs(&work_tree, &abs);
                index.remove(path.as_bytes());
            }
            continue;
        }

        let Some(path) = &change.new_path else {
            continue;
        };
        let path = path.to_string_lossy();
        let mode = u32::from_str_radix(&change.new_mode, 8).unwrap_or(MODE_REGULAR);
        let object = repo.odb.read(&change.new_oid)?;
        write_to_worktree(&work_tree, &path, &object.data, mode)?;
        let entry = entry_from_stat(&work_tree.join(&*path), path.as_bytes(), change.new_oid, mode)?;
        index.add_or_replace(entry);
    }

    index.sort();
    repo.write_index(&mut index)?;
    Ok(())
}

/// Set `abs_path` permissions to match Git index `mode` (regular vs executable blob).
pub fn apply_index_file_mode(abs_path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(abs_path)?.permissions();
        let new_mode = if mode == MODE_EXECUTABLE {
            0o755
        } else {
            0o644
        };
        perms.set_mode(new_mode);
        std::fs::set_permissions(abs_path, perms)?;
    }
    // Windows has no POSIX mode bits; the executable bit is not represented in
    // the filesystem, so this is a no-op there.
    #[cfg(not(unix))]
    let _ = (abs_path, mode);
    Ok(())
}

/// Ensure each component of `rel_path`'s parent exists as a real directory.
///
/// Replaces a parent path that is a symlink or regular file (e.g. `D` → `untracked` or `D` as a
/// file) so `mkdir -p` can create `D/A` during checkout (`t2080` force checkout cases).
pub fn prepare_parent_dirs_for_checkout(work_tree: &Path, rel_path: &str) -> Result<()> {
    use std::path::Component;
    let path = Path::new(rel_path);
    let Some(parent_rel) = path.parent() else {
        return Ok(());
    };
    if parent_rel.as_os_str().is_empty() {
        return Ok(());
    }
    let mut cur = work_tree.to_path_buf();
    for comp in parent_rel.components() {
        if let Component::Normal(name) = comp {
            cur.push(name);
            if let Ok(meta) = std::fs::symlink_metadata(&cur) {
                if meta.file_type().is_symlink() {
                    std::fs::remove_file(&cur)?;
                } else if !meta.is_dir() {
                    std::fs::remove_file(&cur)?;
                }
            }
        }
    }
    Ok(())
}

/// Write data to a working tree file, handling symlinks and executable bits.
pub fn write_to_worktree(work_tree: &Path, rel_path: &str, data: &[u8], mode: u32) -> Result<()> {
    let abs_path = work_tree.join(rel_path);

    prepare_parent_dirs_for_checkout(work_tree, rel_path)?;
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::PathError(format!("creating parent directories for '{rel_path}': {e}"))
        })?;
    }

    // Remove existing file/dir/symlink at target path. Use symlink_metadata + is_symlink so we
    // replace symlinked paths (e.g. `D` → `untracked`) before creating a real directory tree.
    if let Ok(meta) = std::fs::symlink_metadata(&abs_path) {
        if meta.file_type().is_symlink() {
            std::fs::remove_file(&abs_path)?;
        } else if meta.is_dir() {
            std::fs::remove_dir_all(&abs_path)?;
        } else {
            std::fs::remove_file(&abs_path)?;
        }
    }

    if mode == MODE_SYMLINK {
        let target = std::str::from_utf8(data).map_err(|_| {
            Error::PathError(format!("symlink target for '{rel_path}' is not UTF-8"))
        })?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &abs_path)
            .map_err(|e| Error::PathError(format!("creating symlink '{rel_path}': {e}")))?;
        // Windows lacks unprivileged symlink creation; materialise the link as a
        // regular file containing the target path so the worktree stays populated.
        #[cfg(not(unix))]
        std::fs::write(&abs_path, target.as_bytes())
            .map_err(|e| Error::PathError(format!("writing symlink '{rel_path}': {e}")))?;
    } else {
        std::fs::write(&abs_path, data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}': {e}")))?;

        if mode == MODE_EXECUTABLE {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&abs_path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&abs_path, perms)?;
            }
        }
    }

    Ok(())
}

/// Remove empty parent directories up to (but not including) `work_tree`.
pub fn remove_empty_parent_dirs(work_tree: &Path, path: &Path) {
    let cwd = std::env::current_dir().ok();
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == work_tree {
            break;
        }
        if cwd
            .as_ref()
            .is_some_and(|cwd| cwd == dir || cwd.starts_with(dir))
        {
            break;
        }
        match std::fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(_) => break,
        }
    }
}

/// Check if a pathspec contains glob characters.
pub fn is_glob_pattern(spec: &str) -> bool {
    spec.contains('*') || spec.contains('?') || spec.contains('[')
}

/// Match a path against a simple glob pattern.
/// Supports `*` (any chars except `/`), `?` (any single char except `/`),
/// and character classes `[abc]`.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    glob_matches_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_matches_inner(pattern: &[u8], path: &[u8]) -> bool {
    let mut pi = 0; // pattern index
    let mut si = 0; // string index
    let mut star_pi = usize::MAX;
    let mut star_si = 0;

    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            si += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // "**" matches everything including '/'
                // For simplicity, try matching rest of pattern at every position
                let rest = &pattern[pi + 2..];
                // Skip optional '/' after **
                let rest = if !rest.is_empty() && rest[0] == b'/' {
                    &rest[1..]
                } else {
                    rest
                };
                for i in si..=path.len() {
                    if glob_matches_inner(rest, &path[i..]) {
                        return true;
                    }
                }
                return false;
            }
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'[' {
            // Character class
            pi += 1;
            let negate = pi < pattern.len() && (pattern[pi] == b'!' || pattern[pi] == b'^');
            if negate {
                pi += 1;
            }
            let mut found = false;
            let ch = path[si];
            while pi < pattern.len() && pattern[pi] != b']' {
                if pi + 2 < pattern.len() && pattern[pi + 1] == b'-' {
                    if ch >= pattern[pi] && ch <= pattern[pi + 2] {
                        found = true;
                    }
                    pi += 3;
                } else {
                    if ch == pattern[pi] {
                        found = true;
                    }
                    pi += 1;
                }
            }
            if pi < pattern.len() {
                pi += 1;
            } // skip ']'
            if found == negate {
                // Mismatch in character class
                if star_pi != usize::MAX {
                    pi = star_pi + 1;
                    star_si += 1;
                    si = star_si;
                } else {
                    return false;
                }
            } else {
                si += 1;
            }
        } else if pi < pattern.len() && pattern[pi] == path[si] {
            pi += 1;
            si += 1;
        } else if star_pi != usize::MAX {
            // Backtrack: '*' matches one more character (including '/')
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }

    // Consume trailing '*' or '**' in pattern
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}
