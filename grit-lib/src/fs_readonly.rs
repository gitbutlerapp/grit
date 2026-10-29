//! Read-only file attributes (Windows and Unix) for atomic replace and object freshen.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};

fn system_time_from_file_time(ft: filetime::FileTime) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::new(ft.unix_seconds() as u64, ft.nanoseconds())
}

/// Clear the read-only attribute when present so replace, remove, or utime can succeed.
///
/// Git marks commit-graph and pack files read-only on Windows; same-directory
/// rename over an existing read-only file fails with access denied until cleared.
pub(crate) fn clear_readonly_attribute(path: &Path, required: bool) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut perms = meta.permissions();
    if !perms.readonly() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        perms.set_readonly(false);
        fs::set_permissions(path, perms).map_err(Error::Io)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = perms.mode();
        mode |= 0o200;
        perms.set_mode(mode);
        if let Err(e) = fs::set_permissions(path, perms) {
            if required {
                return Err(Error::Io(e));
            }
        }
    }
    #[cfg(windows)]
    let _ = required;
    Ok(())
}

/// Restore `permissions` saved before [`clear_readonly_attribute`].
pub(crate) fn restore_file_permissions(path: &Path, permissions: &fs::Permissions) -> Result<()> {
    fs::set_permissions(path, permissions.clone()).map_err(Error::Io)
}

/// Set atime and mtime to now, clearing a read-only attribute temporarily when needed.
///
/// Matches Git's `mingw_utime` / `write_object_file` freshen path: a failed `utime` on a
/// read-only pack or loose object is retried after clearing the attribute, then restored.
pub(crate) fn set_file_times_now(path: &Path) -> Option<SystemTime> {
    let now = filetime::FileTime::now();
    let touched = system_time_from_file_time(now);
    if filetime::set_file_times(path, now, now).is_ok() {
        return Some(touched);
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return None;
    };
    let saved = meta.permissions();
    if !saved.readonly() {
        return None;
    }
    if clear_readonly_attribute(path, false).is_err() {
        return None;
    }
    if filetime::set_file_times(path, now, now).is_err() {
        let _ = restore_file_permissions(path, &saved);
        return None;
    }
    if restore_file_permissions(path, &saved).is_err() {
        return None;
    }
    Some(touched)
}

/// Walk a tree and clear read-only attributes so replace/remove can succeed.
pub(crate) fn clear_readonly_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(Error::Io)?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }
    if file_type.is_dir() {
        for entry in fs::read_dir(path).map_err(Error::Io)? {
            let entry = entry.map_err(Error::Io)?;
            let child_type = entry.file_type().map_err(Error::Io)?;
            if child_type.is_symlink() {
                continue;
            }
            clear_readonly_tree(&entry.path())?;
        }
    }
    clear_readonly_attribute(path, cfg!(windows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn set_file_times_now_updates_readonly_file_and_restores_attribute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pack-a.pack");
        fs::write(&path, b"fake pack bytes").expect("write");
        let mut perms = fs::metadata(&path).expect("meta").permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).expect("readonly");

        let before = fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");
        thread::sleep(Duration::from_millis(25));
        assert!(
            set_file_times_now(&path).is_some(),
            "freshen must succeed on readonly pack"
        );
        let after = fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert!(after > before, "mtime must advance");
        assert!(
            fs::metadata(&path).expect("meta").permissions().readonly(),
            "read-only attribute must be restored"
        );
    }
}
