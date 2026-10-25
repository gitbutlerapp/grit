//! Publishing content-addressed files (packs, pack indexes, loose objects) safely when
//! several processes write the same object store at once.
//!
//! The file name of every such file is derived from its content, so a name that already
//! exists holds the bytes the caller is about to write. Publishing therefore never
//! replaces or truncates an existing file: losing the race to another writer is success.

use std::fs;
use std::io;
use std::path::Path;

use tempfile::TempPath;

use crate::error::{Error, Result};

/// Move `staged` to `dst` unless `dst` already exists.
///
/// `staged` must be on the same filesystem as `dst`. It is gone when this returns,
/// whatever the outcome.
///
/// # Returns
///
/// `true` when this call created `dst`, `false` when another writer already had.
///
/// # Errors
///
/// Returns the I/O error when `dst` can neither be created nor found.
pub(crate) fn publish_no_clobber(staged: TempPath, dst: &Path) -> io::Result<bool> {
    match staged.persist_noclobber(dst) {
        Ok(()) => Ok(true),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error),
    }
}

/// Refuse symlinks and non-regular files (matches `git clone --local` safety).
fn ensure_linkable_regular_file(src: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(src).map_err(Error::Io)?;
    if meta.file_type().is_symlink() {
        return Err(Error::PathError(format!(
            "refusing to link symlink object store entry {}",
            src.display()
        )));
    }
    if !meta.is_file() {
        return Err(Error::PathError(format!(
            "refusing to link non-regular object store entry {}",
            src.display()
        )));
    }
    Ok(())
}

/// Hard-link the content-addressed file `src` to `dst`, or copy it when linking is
/// unavailable (another filesystem, no link support).
///
/// An existing `dst` is left untouched and counts as success. A copy is written to a
/// fresh temp file next to `dst` and published with [`publish_no_clobber`], so a write
/// never lands on an inode shared with `src`.
///
/// # Errors
///
/// Returns [`Error::PathError`] when `src` is a symlink or not a regular file, and
/// [`Error::Io`] when neither linking nor copying succeeds.
pub(crate) fn hard_link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    ensure_linkable_regular_file(src)?;
    let parent = dst
        .parent()
        .ok_or_else(|| Error::PathError(format!("no parent directory for {}", dst.display())))?;
    fs::create_dir_all(parent).map_err(Error::Io)?;
    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => copy_then_publish(src, dst, parent),
    }
}

fn copy_then_publish(src: &Path, dst: &Path, parent: &Path) -> Result<()> {
    let tmp = tempfile::Builder::new()
        .prefix("tmp_copy_")
        .tempfile_in(parent)
        .map_err(Error::Io)?
        .into_temp_path();
    fs::copy(src, &tmp).map_err(Error::Io)?;
    publish_no_clobber(tmp, dst).map_err(Error::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn read_only(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms).unwrap();
    }

    fn staged_file(dir: &Path, bytes: &[u8]) -> TempPath {
        let tmp = tempfile::Builder::new()
            .prefix("tmp_test_")
            .tempfile_in(dir)
            .unwrap()
            .into_temp_path();
        fs::write(&tmp, bytes).unwrap();
        tmp
    }

    #[test]
    fn publish_no_clobber_creates_then_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("pack-a.pack");

        let first = staged_file(dir.path(), b"same bytes");
        let first_path = first.to_path_buf();
        assert!(publish_no_clobber(first, &dst).unwrap());
        assert!(!first_path.exists(), "staged file is consumed");

        let second = staged_file(dir.path(), b"different");
        let second_path = second.to_path_buf();
        assert!(!publish_no_clobber(second, &dst).unwrap());
        assert!(!second_path.exists(), "losing staged file is removed");
        assert_eq!(fs::read(&dst).unwrap(), b"same bytes");
    }

    #[test]
    fn publish_no_clobber_reports_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let staged = staged_file(dir.path(), b"x");
        let err = publish_no_clobber(staged, &dir.path().join("missing/dst")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn hard_link_or_copy_existing_link_leaves_read_only_source_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.pack");
        fs::write(&src, b"pack bytes").unwrap();
        read_only(&src);
        let dst = dir.path().join("dst/src.pack");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::hard_link(&src, &dst).unwrap();

        hard_link_or_copy(&src, &dst).expect("already linked is success");
        assert_eq!(fs::read(&src).unwrap(), b"pack bytes");
    }

    #[test]
    fn copy_path_never_writes_through_existing_link_to_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.pack");
        fs::write(&src, b"writable source").unwrap();
        let dst_dir = dir.path().join("dst");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("src.pack");
        fs::hard_link(&src, &dst).unwrap();

        copy_then_publish(&src, &dst, &dst_dir).expect("existing dst is success");
        assert_eq!(fs::read(&src).unwrap(), b"writable source");
        let leftovers: Vec<_> = fs::read_dir(&dst_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n != "src.pack")
            .collect();
        assert!(leftovers.is_empty(), "temp copies left: {leftovers:?}");
    }

    #[test]
    fn copy_path_publishes_when_destination_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.idx");
        fs::write(&src, b"idx bytes").unwrap();
        read_only(&src);
        let dst_dir = dir.path().join("dst");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("src.idx");

        copy_then_publish(&src, &dst, &dst_dir).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"idx bytes");
        assert_eq!(fs::read_dir(&dst_dir).unwrap().count(), 1);
    }

    #[test]
    fn racing_link_and_copy_into_one_destination_all_succeed() {
        for read_only_source in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let src = Arc::new(dir.path().join("src.pack"));
            let payload: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
            fs::write(&*src, &payload).unwrap();
            if read_only_source {
                read_only(&src);
            }
            let dst_dir = Arc::new(dir.path().join("dst"));
            let dst = Arc::new(dst_dir.join("src.pack"));
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let (src, dst, dst_dir) = (src.clone(), dst.clone(), dst_dir.clone());
                    std::thread::spawn(move || {
                        if i % 2 == 0 {
                            hard_link_or_copy(&src, &dst)
                        } else {
                            fs::create_dir_all(&*dst_dir).map_err(Error::Io)?;
                            copy_then_publish(&src, &dst, &dst_dir)
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().expect("every racer succeeds");
            }
            assert_eq!(fs::read(&*src).unwrap(), payload, "source unchanged");
            assert_eq!(fs::read(&*dst).unwrap(), payload);
            assert_eq!(fs::read_dir(&*dst_dir).unwrap().count(), 1);
        }
    }
}
