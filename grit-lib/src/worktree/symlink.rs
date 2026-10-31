//! Symbolic link creation in the working tree when `core.symlinks` is enabled.

use std::path::{Path, PathBuf};

/// Resolve `target` against `link_parent` for directory-vs-file classification.
///
/// Git resolves relative symlink targets against the link's parent directory, not the process
/// current working directory.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))]
pub fn resolve_symlink_lookup_path(link_parent: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        link_parent.join(target)
    }
}

/// Whether `target` should be materialised with a directory symlink API (Windows `symlink_dir`).
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))]
pub fn symlink_target_is_directory(link_parent: &Path, target: &Path) -> bool {
    resolve_symlink_lookup_path(link_parent, target).is_dir()
}

/// Create a symbolic link at `link` pointing at `target`.
///
/// On Windows, chooses `symlink_file` vs `symlink_dir` from the resolved target path. Dangling
/// links use the file API when the lookup path is not an existing directory.
pub fn create_worktree_symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let parent = link.parent().unwrap_or_else(|| Path::new("."));
        if symlink_target_is_directory(parent, target) {
            symlink_dir(target, link)
        } else {
            symlink_file(target, link)
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (link, target);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }
}

/// Decode symlink blob bytes to a path (UTF-8 on Windows, any bytes on Unix).
pub fn symlink_target_path_from_bytes(bytes: &[u8]) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(windows)]
    {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "symlink target must be UTF-8 on Windows",
            )
        })?;
        Ok(PathBuf::from(text))
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = bytes;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }
}

/// Write a mode-120000 blob at `link`, creating parent directories when needed.
pub fn create_worktree_symlink_from_blob(link: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = symlink_target_path_from_bytes(bytes)?;
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    create_worktree_symlink(link, &target)
}

/// Materialise an index symlink entry in the worktree.
///
/// When `has_symlinks` is false (Git for Windows default), writes a plain file holding the target
/// text while the index keeps mode `120000`.
pub fn write_index_symlink_to_worktree(
    link: &Path,
    target: &str,
    has_symlinks: bool,
) -> std::io::Result<()> {
    if has_symlinks {
        create_worktree_symlink(link, Path::new(target))
    } else {
        std::fs::write(link, target.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn relative_directory_target_uses_parent_not_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("parent");
        let real_dir = tmp.path().join("real");
        fs::create_dir_all(&real_dir).expect("mkdir real");
        fs::create_dir_all(&parent).expect("mkdir parent");
        let rel = Path::new("../real");
        assert!(symlink_target_is_directory(&parent, rel));
        let file = tmp.path().join("plain.txt");
        fs::write(&file, b"x").expect("write");
        assert!(!symlink_target_is_directory(
            &parent,
            Path::new("../plain.txt")
        ));
    }

    #[test]
    fn dangling_relative_target_is_not_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("dir");
        fs::create_dir_all(&parent).expect("mkdir");
        assert!(!symlink_target_is_directory(&parent, Path::new("missing")));
    }

    #[test]
    fn disabled_symlinks_write_plain_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let link = tmp.path().join("link.txt");
        write_index_symlink_to_worktree(&link, "target.txt", false).expect("write");
        let meta = fs::symlink_metadata(&link).expect("meta");
        assert!(meta.is_file());
        #[cfg(unix)]
        assert!(!meta.file_type().is_symlink());
        assert_eq!(fs::read_to_string(&link).expect("read"), "target.txt");
    }
}
