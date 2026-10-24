//! Linked worktree move and repair (Git `worktree move` / `worktree repair`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::git_path::{absolute_path_for_gitfile_stored_path, path_for_disk_compare};
use crate::index::{Index, MODE_GITLINK};
use crate::lockfile::{LockError, LockFile, LockWait};
use crate::repo::Repository;

use super::core::{common_git_dir, list_worktrees, WorktreeEntry};

/// Options for [`move_linked_worktree`].
#[derive(Debug, Clone, Default)]
pub struct MoveWorktreeOptions {
    /// Allow move when the worktree is locked (Git requires `-f` twice; grit uses one `--force`).
    pub force: bool,
    /// Report the move without changing disk state.
    pub dry_run: bool,
}

/// What [`repair_worktrees`] fixed for one worktree path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeRepairKind {
    /// The worktree `.git` gitfile pointed at the wrong admin directory.
    GitFileAdminDir,
    /// `<admin>/gitdir` did not point at this worktree's `.git` file.
    GitdirWorktreePath,
    /// `.git` used a relative/absolute path where the other form was required.
    GitFilePathStyle,
    /// `gitdir` used a relative/absolute path where the other form was required.
    GitdirPathStyle,
}

/// One repair action recorded by [`repair_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRepair {
    /// Working tree root that was repaired.
    pub path: PathBuf,
    /// Which link was rewritten.
    pub kind: WorktreeRepairKind,
}

/// Move a linked worktree to a new path and rewrite `gitdir` / `.git` links.
///
/// # Errors
///
/// Refuses the main worktree, locked worktrees (unless [`MoveWorktreeOptions::force`]),
/// an existing destination, populated submodules, or cross-operation I/O failures.
pub fn move_linked_worktree(
    repo: &Repository,
    from: &Path,
    to: &Path,
    options: &MoveWorktreeOptions,
) -> Result<()> {
    let common = common_git_dir(repo.git_dir());
    let from_canon = path_for_disk_compare(from);
    let entry = find_worktree_entry(repo, &from_canon)?;
    if entry.admin_dir == common {
        return Err(Error::Message("cannot move the main working tree".into()));
    }
    if entry.is_locked && !options.force {
        return Err(Error::Message(
            entry
                .lock_reason
                .map(|r| format!("worktree is locked: {r}"))
                .unwrap_or_else(|| "worktree is locked".into()),
        ));
    }

    let dest = resolve_move_destination(&entry.path, to)?;
    if dest.exists() {
        return Err(Error::Message(format!(
            "'{}' already exists",
            dest.display()
        )));
    }

    if worktree_has_blocking_submodules(&entry)? {
        return Err(Error::Message(
            "working trees containing submodules cannot be moved or removed".into(),
        ));
    }

    if options.dry_run {
        return Ok(());
    }

    rename_worktree_directory(&entry.path, &dest)?;

    let dotgit = dest.join(".git");
    let gitdir_file = entry.admin_dir.join("gitdir");
    write_worktree_linking_files(&dotgit, &gitdir_file, &dest, &entry.admin_dir)?;
    Ok(())
}

/// Repair `.git` / `gitdir` links for linked worktrees.
///
/// When `paths` is empty, repairs every registered linked worktree from the open repository.
/// Otherwise each path must be a linked worktree root (not the main worktree).
///
/// # Errors
///
/// Propagates I/O failures and returns [`Error::Message`] for invalid repair targets.
pub fn repair_worktrees(
    repo: &Repository,
    paths: &[&Path],
    dry_run: bool,
) -> Result<Vec<WorktreeRepair>> {
    let common = common_git_dir(repo.git_dir());
    let mut repairs = Vec::new();
    if paths.is_empty() {
        for entry in list_worktrees(repo)? {
            if entry.admin_dir == common {
                continue;
            }
            if !entry.path.exists() {
                continue;
            }
            repair_gitfile(&entry, dry_run, &mut repairs)?;
        }
        return Ok(repairs);
    }
    for path in paths {
        let wt_path = path_for_disk_compare(path);
        if !wt_path.exists() {
            return Err(Error::Message(format!(
                "'{}' is not a valid path",
                wt_path.display()
            )));
        }
        if !wt_path.is_dir() {
            return Err(Error::Message(format!(
                "'{}' is not a directory",
                wt_path.display()
            )));
        }
        repair_worktree_at_path(repo, &wt_path, dry_run, &mut repairs)?;
    }
    Ok(repairs)
}

fn find_worktree_entry(repo: &Repository, path: &Path) -> Result<WorktreeEntry> {
    list_worktrees(repo)?
        .into_iter()
        .find(|e| e.path == path)
        .ok_or_else(|| Error::Message(format!("'{}' is not a registered worktree", path.display())))
}

/// Resolve the destination directory for [`move_linked_worktree`].
///
/// When `to` is an existing directory, the worktree basename is appended (Git behavior).
///
/// # Errors
///
/// Returns [`Error::Message`] when `from` has no final path component or a parent is not a directory.
pub fn resolve_move_destination(from: &Path, to: &Path) -> Result<PathBuf> {
    let dest = if to.is_dir() {
        let base = from
            .file_name()
            .ok_or_else(|| Error::Message("invalid worktree path".into()))?;
        path_for_disk_compare(&to.join(base))
    } else {
        path_for_disk_compare(to)
    };
    if let Some(parent) = dest.parent() {
        if parent.exists() && !parent.is_dir() {
            return Err(Error::Message(format!(
                "'{}' is not a directory",
                parent.display()
            )));
        }
    }
    Ok(dest)
}

fn rename_worktree_directory(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            match copy_dir_all(from, to) {
                Ok(()) => {
                    fs::remove_dir_all(from).map_err(Error::Io)?;
                    Ok(())
                }
                Err(copy_err) => {
                    let _ = fs::remove_dir_all(to);
                    Err(copy_err)
                }
            }
        }
        Err(err) => Err(Error::Io(err)),
    }
}

fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(Error::Io)?;
    for entry in fs::read_dir(from).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let src = entry.path();
        let dest = to.join(entry.file_name());
        copy_tree_entry(&src, &dest)?;
    }
    Ok(())
}

fn copy_tree_entry(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).map_err(Error::Io)?;
    let file_type = meta.file_type();
    if file_type.is_dir() {
        copy_dir_all(from, to)?;
    } else if file_type.is_symlink() {
        let target = fs::read_link(from).map_err(Error::Io)?;
        create_symlink(&meta, &target, to)?;
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        fs::copy(from, to).map_err(Error::Io)?;
    }
    Ok(())
}

fn create_symlink(source_meta: &fs::Metadata, target: &Path, link_path: &Path) -> Result<()> {
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    #[cfg(unix)]
    {
        let _ = source_meta;
        use std::os::unix::fs::symlink;
        symlink(target, link_path).map_err(Error::Io)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        // Preserve the source reparse-point kind (directory vs file). `target.is_dir()` follows
        // relative targets from the process cwd and is false for dangling directory links.
        if source_meta.is_dir() {
            symlink_dir(target, link_path).map_err(Error::Io)?;
        } else {
            symlink_file(target, link_path).map_err(Error::Io)?;
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_symlink_copy_tests {
    use super::*;
    use std::os::windows::fs::symlink_dir;

    #[test]
    fn copy_tree_entry_preserves_dangling_directory_symlink_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = tmp.path().join("src");
        fs::create_dir_all(&src_root).expect("srcdir");
        let src_link = src_root.join("dirlink");
        symlink_dir("missing-directory", &src_link).expect("symlink_dir");

        let dest_link = tmp.path().join("dst").join("dirlink");
        copy_tree_entry(&src_link, &dest_link).expect("copy");

        let meta = fs::symlink_metadata(&dest_link).expect("dest meta");
        assert!(meta.file_type().is_symlink());
        assert!(
            meta.is_dir(),
            "directory reparse point must stay a directory link"
        );
        assert_eq!(
            fs::read_link(&dest_link).expect("readlink"),
            PathBuf::from("missing-directory")
        );
    }

    #[test]
    fn copy_tree_entry_preserves_relative_directory_symlink_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = tmp.path().join("src");
        fs::create_dir_all(&src_root).expect("srcdir");
        let src_link = src_root.join("dirlink");
        symlink_dir("relative\\child", &src_link).expect("symlink_dir");

        let dest_link = tmp.path().join("dst").join("dirlink");
        copy_tree_entry(&src_link, &dest_link).expect("copy");

        let meta = fs::symlink_metadata(&dest_link).expect("dest meta");
        assert!(meta.file_type().is_symlink());
        assert!(
            meta.is_dir(),
            "relative directory link must not become a file link"
        );
        assert_eq!(
            fs::read_link(&dest_link).expect("readlink"),
            PathBuf::from("relative\\child")
        );
    }
}

fn worktree_has_blocking_submodules(entry: &WorktreeEntry) -> Result<bool> {
    let modules = entry.admin_dir.join("modules");
    if modules.is_dir() && fs::read_dir(&modules).map_err(Error::Io)?.next().is_some() {
        return Ok(true);
    }
    let index_path = entry.admin_dir.join("index");
    if !index_path.is_file() {
        return Ok(false);
    }
    let index = Index::load(&index_path)?;
    for idx_entry in index.entries() {
        if idx_entry.mode != MODE_GITLINK {
            continue;
        }
        let name = std::str::from_utf8(&idx_entry.path)
            .map_err(|_| Error::Message("invalid submodule path in index".into()))?;
        let sub_path = entry.path.join(name);
        if is_submodule_populated(&sub_path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_submodule_populated(sub_path: &Path) -> bool {
    let dot_git = sub_path.join(".git");
    if dot_git.is_dir() {
        return true;
    }
    if dot_git.is_file() {
        if let Ok(content) = fs::read_to_string(&dot_git) {
            if let Some(target) = parse_gitfile_target(&content, sub_path) {
                return target.exists();
            }
        }
    }
    false
}

fn parse_gitfile_target(content: &str, base: &Path) -> Option<PathBuf> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let rel = rest.trim();
            let path = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                base.join(rel)
            };
            return Some(path_for_disk_compare(&path));
        }
    }
    None
}

fn write_worktree_linking_files(
    dotgit: &Path,
    gitdir_file: &Path,
    wt_path: &Path,
    admin_dir: &Path,
) -> Result<()> {
    let admin_abs = absolute_path_for_gitfile_stored_path(admin_dir);
    let dotgit_abs = absolute_path_for_gitfile_stored_path(dotgit);
    atomic_write_text(gitdir_file, &format!("{dotgit_abs}\n"))?;
    atomic_write_text(dotgit, &format!("gitdir: {admin_abs}\n"))?;
    let _ = wt_path;
    Ok(())
}

fn atomic_write_text(path: &Path, content: &str) -> Result<()> {
    let lock_path = lock_path_for(path);
    let mut lock = LockFile::acquire(&lock_path, LockWait::Immediately).map_err(map_lock_err)?;
    lock.write_all(content.as_bytes()).map_err(Error::Io)?;
    lock.commit_to(path).map_err(map_lock_err)?;
    Ok(())
}

fn lock_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| format!("{}.lock", n.to_string_lossy()))
        .unwrap_or_else(|| "file.lock".to_owned());
    path.parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn map_lock_err(err: LockError) -> Error {
    match err {
        LockError::Io { source, .. } => Error::Io(source),
        LockError::Held { path } => {
            Error::Message(format!("could not acquire lock for '{}'", path.display()))
        }
    }
}

fn repair_gitfile(
    entry: &WorktreeEntry,
    dry_run: bool,
    repairs: &mut Vec<WorktreeRepair>,
) -> Result<()> {
    let dotgit = entry.path.join(".git");
    if !dotgit.is_file() {
        return Ok(());
    }
    let expected_admin = absolute_path_for_gitfile_stored_path(&entry.admin_dir);
    let content = fs::read_to_string(&dotgit).unwrap_or_default();
    let kind = gitfile_repair_kind(&content, &entry.path, &expected_admin);
    if let Some(kind) = kind {
        repairs.push(WorktreeRepair {
            path: entry.path.clone(),
            kind,
        });
        if !dry_run {
            let gitdir_file = entry.admin_dir.join("gitdir");
            write_worktree_linking_files(&dotgit, &gitdir_file, &entry.path, &entry.admin_dir)?;
        }
    }
    Ok(())
}

fn gitfile_repair_kind(
    content: &str,
    wt_path: &Path,
    expected_admin: &str,
) -> Option<WorktreeRepairKind> {
    if content_contains_relative_gitdir(content) {
        return Some(WorktreeRepairKind::GitFilePathStyle);
    }
    match parse_gitfile_target(content, wt_path) {
        Some(current) => {
            let current_admin = absolute_path_for_gitfile_stored_path(&current);
            if current_admin != expected_admin {
                Some(WorktreeRepairKind::GitFileAdminDir)
            } else {
                None
            }
        }
        None => Some(WorktreeRepairKind::GitFileAdminDir),
    }
}

fn content_contains_relative_gitdir(content: &str) -> bool {
    content.lines().any(|line| {
        line.strip_prefix("gitdir:")
            .map(|rest| !Path::new(rest.trim()).is_absolute())
            .unwrap_or(false)
    })
}

fn repair_worktree_at_path(
    repo: &Repository,
    wt_path: &Path,
    dry_run: bool,
    repairs: &mut Vec<WorktreeRepair>,
) -> Result<()> {
    let dotgit = wt_path.join(".git");
    if dotgit.is_dir() {
        return Err(Error::Message(
            "unable to locate repository; .git is not a file".into(),
        ));
    }
    let dotgit_content = fs::read_to_string(&dotgit).map_err(Error::Io)?;
    let admin_dir = resolve_admin_from_gitfile(repo, &dotgit_content, wt_path)?;

    let expected_admin = absolute_path_for_gitfile_stored_path(&admin_dir);
    if let Some(kind) = gitfile_repair_kind(&dotgit_content, wt_path, &expected_admin) {
        repairs.push(WorktreeRepair {
            path: wt_path.to_path_buf(),
            kind,
        });
        if !dry_run {
            let gitdir_file = admin_dir.join("gitdir");
            write_worktree_linking_files(&dotgit, &gitdir_file, wt_path, &admin_dir)?;
        }
    }

    let gitdir_file = admin_dir.join("gitdir");
    let expected_dotgit = absolute_path_for_gitfile_stored_path(&dotgit);
    let gitdir_raw = fs::read_to_string(&gitdir_file).unwrap_or_default();
    if let Some(kind) = gitdir_repair_kind(&gitdir_raw, &admin_dir, &expected_dotgit) {
        repairs.push(WorktreeRepair {
            path: wt_path.to_path_buf(),
            kind,
        });
        if !dry_run {
            write_worktree_linking_files(&dotgit, &gitdir_file, wt_path, &admin_dir)?;
        }
    }
    Ok(())
}

fn gitdir_repair_kind(
    raw: &str,
    admin: &Path,
    expected_dotgit: &str,
) -> Option<WorktreeRepairKind> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(WorktreeRepairKind::GitdirWorktreePath);
    }
    if Path::new(trimmed).is_relative() {
        return Some(WorktreeRepairKind::GitdirPathStyle);
    }
    let resolved = resolve_gitdir_target(trimmed, admin)?;
    if resolved != expected_dotgit {
        Some(WorktreeRepairKind::GitdirWorktreePath)
    } else {
        None
    }
}

fn resolve_admin_from_gitfile(repo: &Repository, content: &str, wt_path: &Path) -> Result<PathBuf> {
    if let Some(admin) = parse_gitfile_target(content, wt_path) {
        if admin.join("commondir").is_file() || admin.join("HEAD").is_file() {
            return Ok(admin);
        }
    }
    if let Some(inferred) = infer_admin_from_gitfile_content(repo, content) {
        return Ok(inferred);
    }
    Err(Error::Message(
        "unable to locate repository; .git file broken".into(),
    ))
}

fn infer_admin_from_gitfile_content(repo: &Repository, content: &str) -> Option<PathBuf> {
    let line = content.lines().find_map(|l| l.strip_prefix("gitdir:"))?;
    let trimmed = line.trim();
    let id = Path::new(trimmed).file_name()?.to_str()?;
    let common = common_git_dir(repo.git_dir());
    let candidate = common.join("worktrees").join(id);
    if candidate.is_dir() {
        Some(path_for_disk_compare(&candidate))
    } else {
        None
    }
}

fn resolve_gitdir_target(trimmed: &str, admin: &Path) -> Option<String> {
    if trimmed.is_empty() {
        return None;
    }
    let path = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        admin.join(trimmed)
    };
    Some(absolute_path_for_gitfile_stored_path(&path))
}
