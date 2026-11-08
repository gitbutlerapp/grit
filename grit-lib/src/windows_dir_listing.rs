//! Windows directory enumeration via `FindFirstFileExW` with `FIND_FIRST_EX_LARGE_FETCH`.
//!
//! Non-Windows builds expose a stub [`WorktreeListingCache`] so status can thread `Option`
//! through shared call sites.

use std::sync::Arc;

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub(crate) struct WorktreeListingCache;

#[cfg(not(windows))]
#[must_use]
pub(crate) fn spawn_listing_cache() -> Arc<WorktreeListingCache> {
    Arc::new(WorktreeListingCache)
}

#[cfg(not(windows))]
pub(crate) fn listing_cache_ref(
    _cache: &Option<Arc<WorktreeListingCache>>,
) -> Option<&WorktreeListingCache> {
    None
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use hashbrown::HashMap;

    use crate::error::{Error, Result};
    use crate::index::{IndexStatFields, MODE_REGULAR, MODE_SYMLINK, MODE_TREE};
    use crate::worktree_scan::BlobDiskStat;

    fn path_to_wide(path: &Path) -> Vec<u16> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.last() != Some(&0) {
            wide.push(0);
        }
        wide
    }

    fn filetime_pair(low: u32, high: u32) -> (u32, u32) {
        let ticks = (u64::from(high) << 32) | u64::from(low);
        let interval = 10_000_000u64;
        let secs = ticks / interval;
        let nsec = ((ticks % interval) * 100) as u32;
        let unix = secs.saturating_sub(11_644_473_600);
        (unix as u32, nsec)
    }

    /// One directory entry from a single `FindFirstFileEx` walk.
    #[derive(Debug, Clone)]
    pub(crate) struct WindowsListedEntry {
        pub(crate) stat: BlobDiskStat,
    }

    impl WindowsListedEntry {
        fn from_find_data(
            data: &windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW,
        ) -> (Vec<u8>, Self) {
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            };
            let end = data
                .cFileName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(data.cFileName.len());
            let name_wide = &data.cFileName[..end];
            let name = String::from_utf16_lossy(name_wide).into_bytes();
            let attrs = data.dwFileAttributes;
            let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            let size = u64::from(data.nFileSizeHigh) << 32 | u64::from(data.nFileSizeLow);
            let (ctime_sec, ctime_nsec) = filetime_pair(
                data.ftCreationTime.dwLowDateTime,
                data.ftCreationTime.dwHighDateTime,
            );
            let (mtime_sec, mtime_nsec) = filetime_pair(
                data.ftLastWriteTime.dwLowDateTime,
                data.ftLastWriteTime.dwHighDateTime,
            );
            let fields = IndexStatFields::from_windows_directory_stat(
                size, ctime_sec, ctime_nsec, mtime_sec, mtime_nsec,
            );
            let git_mode = if is_dir && !is_reparse {
                MODE_TREE
            } else if is_reparse {
                MODE_SYMLINK
            } else {
                MODE_REGULAR
            };
            let stat = BlobDiskStat {
                fields,
                git_mode,
                is_dir: is_dir && !is_reparse,
                raw_size: size,
                reparse_point: is_reparse,
            };
            (name, Self { stat })
        }
    }

    /// Contents of one directory listing, keyed by file name bytes.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct WindowsDirListing {
        pub(crate) by_name: HashMap<Vec<u8>, WindowsListedEntry>,
    }

    pub(crate) fn read_windows_directory(
        abs_dir: &Path,
        expected: usize,
    ) -> io::Result<WindowsDirListing> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
            FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
        };

        crate::test_probe_hooks::hooks::worktree_read_dir(abs_dir);
        let mut pattern = path_to_wide(abs_dir);
        if pattern.last() == Some(&0) {
            pattern.pop();
        }
        if pattern.last().copied() != Some(b'\\' as u16)
            && pattern.last().copied() != Some(b'/' as u16)
        {
            pattern.push(b'\\' as u16);
        }
        pattern.extend_from_slice(&[b'*' as u16, 0]);

        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: `pattern` is NUL-terminated and outlives the call.
        let handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                &mut data as *mut _ as *mut _,
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut listing = WindowsDirListing {
            by_name: HashMap::with_capacity(expected),
        };
        loop {
            if data.cFileName[0] == b'.' as u16
                && (data.cFileName[1] == 0
                    || (data.cFileName[1] == b'.' as u16 && data.cFileName[2] == 0))
            {
                // skip . and ..
            } else {
                let (name, entry) = WindowsListedEntry::from_find_data(&data);
                if name.as_slice() != b".git" {
                    listing.by_name.insert(name, entry);
                }
            }
            // SAFETY: valid find handle from `FindFirstFileExW`.
            let ok = unsafe { FindNextFileW(handle, &mut data) };
            if ok == 0 {
                break;
            }
        }
        // SAFETY: handle from successful FindFirstFileExW.
        unsafe {
            FindClose(handle);
        }
        Ok(listing)
    }

    /// Shared find-first listings for one status operation (tracked scan + untracked walk).
    #[derive(Debug, Default)]
    pub(crate) struct WorktreeListingCache {
        inner: Mutex<HashMap<PathBuf, Arc<WindowsDirListing>>>,
    }

    impl WorktreeListingCache {
        #[must_use]
        pub(crate) fn new() -> Self {
            Self {
                inner: Mutex::new(HashMap::new()),
            }
        }

        pub(crate) fn listing(
            &self,
            abs_dir: &Path,
            expected: usize,
        ) -> io::Result<Arc<WindowsDirListing>> {
            if let Ok(guard) = self.inner.lock() {
                if let Some(hit) = guard.get(abs_dir) {
                    return Ok(Arc::clone(hit));
                }
            }
            let fresh = Arc::new(read_windows_directory(abs_dir, expected)?);
            if let Ok(mut guard) = self.inner.lock() {
                if let Some(hit) = guard.get(abs_dir) {
                    return Ok(Arc::clone(hit));
                }
                guard.insert(abs_dir.to_path_buf(), Arc::clone(&fresh));
            }
            Ok(fresh)
        }
    }

    #[must_use]
    pub(crate) fn spawn_listing_cache() -> Arc<WorktreeListingCache> {
        Arc::new(WorktreeListingCache::new())
    }

    pub(crate) fn listing_cache_ref(
        cache: &Option<Arc<WorktreeListingCache>>,
    ) -> Option<&WorktreeListingCache> {
        cache.as_deref()
    }

    pub(crate) fn read_listing_cached(
        cache: Option<&WorktreeListingCache>,
        abs_dir: &Path,
        expected: usize,
    ) -> Result<Arc<WindowsDirListing>> {
        let listing = match cache {
            Some(c) => c.listing(abs_dir, expected).map_err(Error::Io)?,
            None => Arc::new(read_windows_directory(abs_dir, expected).map_err(Error::Io)?),
        };
        Ok(listing)
    }
}

#[cfg(windows)]
pub(crate) use imp::{
    listing_cache_ref, read_listing_cached, spawn_listing_cache, WindowsDirListing,
    WindowsListedEntry, WorktreeListingCache,
};
