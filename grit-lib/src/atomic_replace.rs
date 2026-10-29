//! Atomic rename-with-replace for git-dir staging files (`index.lock`, `*.lock` refs).
//!
//! On Windows, `std::fs::rename` over a path another thread still has open often fails with
//! `ERROR_ACCESS_DENIED` or sharing violations. Git for Windows retries those errors and can
//! use `SetFileInformationByHandle` with POSIX rename semantics; grit mirrors that behavior here.

use std::io;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::time::Duration;

/// Backoff delays (milliseconds) when a replace hits transient Windows sharing errors.
pub(crate) const REPLACE_RETRY_MS: [u64; 5] = [0, 1, 10, 20, 40];

/// Whether a failed atomic replace should be retried (Windows rename-over-open semantics).
#[must_use]
pub(crate) fn is_replace_contention(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    if let Some(raw) = err.raw_os_error() {
        if raw == libc::EBUSY {
            return true;
        }
        #[cfg(windows)]
        if matches!(raw, 5 | 32 | 33) {
            return true;
        }
    }
    false
}

/// Whether creating an exclusive lock file failed because another holder is active, not ACL.
#[must_use]
pub(crate) fn is_lock_acquire_contention(err: &io::Error, lock_path: &Path) -> bool {
    if err.kind() == io::ErrorKind::AlreadyExists {
        return true;
    }
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    if let Some(raw) = err.raw_os_error() {
        if raw == libc::EBUSY {
            return true;
        }
        #[cfg(windows)]
        if matches!(raw, 32 | 33) {
            return true;
        }
        #[cfg(windows)]
        if raw == 5 && windows_lock_create_access_denied_is_contention(lock_path) {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = lock_path;
    false
}

/// Close an open staging handle without deleting the lock file (needed before Windows replace).
///
/// # Errors
///
/// Returns an I/O error when the null device cannot be opened.
pub(crate) fn detach_open_handle(file: &mut std::fs::File) -> io::Result<()> {
    let sink = null_device()?;
    let _previous = std::mem::replace(file, sink);
    Ok(())
}

fn null_device() -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new().write(true).open("/dev/null")
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new().write(true).open("\\\\.\\NUL")
    }
}

/// Rename `from` over `to`, replacing an existing `to` when present.
///
/// # Errors
///
/// Returns the last I/O error when all attempts fail.
pub(crate) fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    let mut last = None;
    for delay_ms in REPLACE_RETRY_MS {
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        match try_replace_once(from, to) {
            Ok(()) => return Ok(()),
            Err(err) if is_replace_contention(&err) => last = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("replace failed without recording an error")))
}

fn try_replace_once(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        return windows_replace_file(from, to).or_else(|err| {
            if is_replace_contention(&err) {
                Err(err)
            } else {
                std::fs::rename(from, to)
            }
        });
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

#[cfg(windows)]
fn windows_lock_create_access_denied_is_contention(lock_path: &Path) -> bool {
    paths_related_to_lock(lock_path)
        .into_iter()
        .any(|path| path_has_delete_pending(&path))
}

#[cfg(windows)]
fn paths_related_to_lock(lock_path: &Path) -> Vec<PathBuf> {
    let mut out = vec![lock_path.to_path_buf()];
    if let Some(name) = lock_path.file_name().and_then(|s| s.to_str()) {
        if let Some(base) = name.strip_suffix(".lock") {
            if let Some(parent) = lock_path.parent() {
                out.push(parent.join(base));
            }
        }
    }
    out
}

#[cfg(windows)]
fn path_has_delete_pending(path: &Path) -> bool {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileStandardInfo, GetFileInformationByHandleEx, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut info = FILE_STANDARD_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            ptr::addr_of_mut!(info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    unsafe {
        CloseHandle(handle);
    }
    ok != 0 && info.DeletePending
}

#[cfg(windows)]
fn windows_replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::mem::{align_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileRenameInfoEx, SetFileInformationByHandle, DELETE, FILE_READ_ATTRIBUTES,
        FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        SYNCHRONIZE,
    };

    const FILE_RENAME_REPLACE_IF_EXISTS: u32 = 1;
    const FILE_RENAME_POSIX_SEMANTICS: u32 = 2;

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain([0]).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain([0]).collect();
    let name_len = to_wide.len().saturating_sub(1);
    let name_bytes = name_len
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination path too long"))?;
    let info_size = size_of::<FILE_RENAME_INFO>()
        .saturating_add(name_bytes)
        .saturating_sub(size_of::<u16>());
    let align = align_of::<FILE_RENAME_INFO>();
    let mut buffer = vec![0u8; info_size + align];
    let offset = buffer.as_ptr().align_offset(align);
    let info = buffer[offset..].as_mut_ptr().cast::<FILE_RENAME_INFO>();

    let handle = unsafe {
        CreateFileW(
            from_wide.as_ptr(),
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `info` points into `buffer` at `FILE_RENAME_INFO` alignment with room for `FileName`.
    let result = unsafe {
        (*info).Anonymous.Flags = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
        (*info).RootDirectory = ptr::null_mut();
        (*info).FileNameLength = name_bytes as u32;
        ptr::copy_nonoverlapping(to_wide.as_ptr(), (*info).FileName.as_mut_ptr(), name_len);
        SetFileInformationByHandle(handle, FileRenameInfoEx, info.cast(), info_size as u32)
    };
    unsafe {
        CloseHandle(handle);
    }
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn replace_contention_matches_ebusy() {
        let err = io::Error::from_raw_os_error(libc::EBUSY as i32);
        assert!(is_replace_contention(&err));
    }

    #[test]
    #[cfg(windows)]
    fn replace_contention_matches_windows_sharing_errors() {
        for code in [5_i32, 32, 33] {
            let err = io::Error::from_raw_os_error(code);
            assert!(is_replace_contention(&err), "code {code}");
        }
    }

    #[test]
    fn lock_acquire_contention_treats_already_exists_as_held() {
        let err = io::Error::from(io::ErrorKind::AlreadyExists);
        let path = Path::new("/tmp/x.lock");
        assert!(is_lock_acquire_contention(&err, path));
    }

    #[test]
    fn unix_replace_swaps_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("staging.lock");
        let to = dir.path().join("index");
        std::fs::write(&to, b"old\n").expect("seed");
        std::fs::write(&from, b"new\n").expect("stage");
        replace_file(&from, &to).expect("replace");
        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(&to).expect("read"), "new\n");
    }

    #[test]
    #[cfg(windows)]
    fn access_denied_is_replace_contention_not_worktree_in_use() {
        let err = io::Error::from_raw_os_error(5);
        assert!(!crate::worktree::io_error::is_file_in_use(&err));
        assert!(is_replace_contention(&err));
    }
}
