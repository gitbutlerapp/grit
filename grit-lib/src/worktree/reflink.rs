//! Reflink checkout: fill a working tree by cloning files out of another
//! checkout instead of writing blob bytes.
//!
//! Ported from the `git worktree add --reflink` patch kept at
//! `~/computing/terminal/git-custom`, branch `reflink-checkout` (commit
//! `8642d58f2b`, "worktree: add --reflink"). What that patch does for git, this
//! does for grit: given a reference working tree in
//! `GRIT_CHECKOUT_REFERENCE_TREE` (or the git-compatible
//! `GIT_CHECKOUT_REFERENCE_TREE`), a file is created by `FICLONE`-ing the file of
//! the same path out of that tree, and the result is **hashed and compared with
//! the blob it is supposed to hold** before it is kept. A dirty or stale
//! reference tree therefore costs a syscall and never a wrong file: any
//! mismatch, any error, any file below 4 KiB and every platform that is not
//! Linux falls through to the ordinary checkout, which is unchanged. The tree is
//! read once per repository, not per file: `RepositoryOptions::from_process`
//! fills it, and the checkout reads `Repository::checkout_reference_tree`.
//!
//! Same two traps the git patch paid for, both avoided here:
//!
//! - a gate on "did the conversion code return a filter" is dead code, because
//!   the normal write path always has one; the useful test is the attributes;
//! - `/tmp` is tmpfs here, where `FICLONE` answers `EOPNOTSUPP`, so a test tree
//!   under `/tmp` proves nothing about reflinking.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};
use crate::hash::ObjectHasher;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};

/// Files below this are written out normally: a clone costs more than the copy.
const REFLINK_MIN_SIZE: u64 = 4096;

/// `FICLONE` from `<linux/fs.h>`: `_IOW(0x94, 9, int)`.
#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

/// Create `dest` by cloning `ref_root`/`rel`, keeping it only when it hashes to
/// `oid`.
///
/// `create_mode` is the `open(2)` mode the ordinary checkout would use, so a
/// cloned file is created with the same permissions as a written one.
///
/// Returns `true` when the file is in place and verified; `false` when the
/// caller must write the blob out as usual. Nothing is left behind on `false`.
#[cfg(target_os = "linux")]
pub(crate) fn try_reflink(
    dest: &Path,
    rel: &str,
    oid: &ObjectId,
    ref_root: &Path,
    algo: HashAlgo,
    create_mode: u32,
) -> Result<bool> {
    use std::os::unix::fs::OpenOptionsExt;

    let source = ref_root.join(rel);
    let Ok(src) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&source)
    else {
        return Ok(false);
    };
    let Ok(stat) = src.metadata() else {
        return Ok(false);
    };
    if !stat.file_type().is_file() || stat.len() < REFLINK_MIN_SIZE {
        return Ok(false);
    }
    let len = stat.len();

    let out = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(create_mode)
        .open(dest)
    {
        Ok(file) => file,
        // The caller's own write would fail the same way; let it report.
        Err(_) => return Ok(false),
    };

    // SAFETY: both descriptors are owned by `out` and `src` for the call, and
    // FICLONE only reads the source inode.
    let cloned = unsafe {
        libc::ioctl(
            std::os::unix::io::AsRawFd::as_raw_fd(&out),
            FICLONE,
            std::os::unix::io::AsRawFd::as_raw_fd(&src),
        )
    };
    drop(out);
    if cloned < 0 {
        let _ = fs::remove_file(dest);
        return Ok(false);
    }

    match verify_clone(dest, oid, algo, len) {
        Ok(true) => Ok(true),
        Ok(false) | Err(_) => {
            let _ = fs::remove_file(dest);
            Ok(false)
        }
    }
}

/// Hash the file that was just cloned and compare it with the blob id.
#[cfg(target_os = "linux")]
fn verify_clone(dest: &Path, oid: &ObjectId, algo: HashAlgo, len: u64) -> Result<bool> {
    let mut file = File::open(dest).map_err(Error::Io)?;
    let mut hasher = ObjectHasher::for_object(algo, ObjectKind::Blob, len);
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).map_err(Error::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let actual = hasher.finalize()?;
    Ok(&actual == oid)
}

/// Reflinking is Linux-only; everywhere else the ordinary write runs.
#[cfg(not(target_os = "linux"))]
pub(crate) fn try_reflink(
    _dest: &Path,
    _rel: &str,
    _oid: &ObjectId,
    _ref_root: &Path,
    _algo: HashAlgo,
    _create_mode: u32,
) -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use crate::objects::{HashAlgo, ObjectKind};
    use std::path::PathBuf;

    /// `FICLONE` needs a filesystem that implements it (btrfs, XFS, …). `/tmp` is
    /// tmpfs on this machine and answers `EOPNOTSUPP`, so the scratch tree lives
    /// under the crate's `target/`, next to the build.
    fn scratch() -> tempfile::TempDir {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/reflink-tests");
        std::fs::create_dir_all(&root).expect("scratch root");
        tempfile::Builder::new()
            .prefix("reflink-")
            .tempdir_in(&root)
            .expect("scratch dir")
    }

    fn payload(seed: u8) -> Vec<u8> {
        (0..REFLINK_MIN_SIZE as usize + 1024)
            .map(|index| seed.wrapping_add((index % 251) as u8))
            .collect()
    }

    fn blob_id(data: &[u8]) -> ObjectId {
        HashAlgo::Sha1
            .hash_object(ObjectKind::Blob, data)
            .expect("hash blob")
    }

    /// Whether this filesystem clones at all; false means the tests below cannot
    /// say anything and say so instead of passing quietly.
    fn supports_reflink(dir: &Path) -> bool {
        let ref_root = dir.join("probe-ref");
        std::fs::create_dir_all(&ref_root).expect("probe ref");
        let data = payload(1);
        std::fs::write(ref_root.join("probe.bin"), &data).expect("probe write");
        try_reflink(
            &dir.join("probe-dest"),
            "probe.bin",
            &blob_id(&data),
            &ref_root,
            HashAlgo::Sha1,
            0o644,
        )
        .expect("probe reflink")
    }

    #[test]
    fn clones_a_file_that_matches_the_blob() {
        let dir = scratch();
        if !supports_reflink(dir.path()) {
            eprintln!("SKIP: filesystem does not implement FICLONE");
            return;
        }
        let ref_root = dir.path().join("ref");
        std::fs::create_dir_all(ref_root.join("sub")).expect("ref dirs");
        let data = payload(7);
        std::fs::write(ref_root.join("sub/big.bin"), &data).expect("ref file");
        let dest = dir.path().join("work/sub/big.bin");
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("work dirs");

        let cloned = try_reflink(
            &dest,
            "sub/big.bin",
            &blob_id(&data),
            &ref_root,
            HashAlgo::Sha1,
            0o644,
        )
        .expect("reflink");

        assert!(cloned, "a matching reference file is cloned");
        assert_eq!(std::fs::read(&dest).expect("read dest"), data);
    }

    #[test]
    fn rejects_a_reference_file_with_other_content() {
        let dir = scratch();
        if !supports_reflink(dir.path()) {
            eprintln!("SKIP: filesystem does not implement FICLONE");
            return;
        }
        let ref_root = dir.path().join("ref");
        std::fs::create_dir_all(&ref_root).expect("ref dirs");
        let wanted = payload(1);
        let stale = payload(2);
        assert_eq!(wanted.len(), stale.len(), "same size, different bytes");
        std::fs::write(ref_root.join("big.bin"), &stale).expect("stale ref file");
        let dest = dir.path().join("work/big.bin");
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("work dirs");

        let cloned = try_reflink(
            &dest,
            "big.bin",
            &blob_id(&wanted),
            &ref_root,
            HashAlgo::Sha1,
            0o644,
        )
        .expect("reflink");

        assert!(!cloned, "a stale reference file is not kept");
        assert!(!dest.exists(), "the rejected clone leaves nothing behind");
    }

    #[test]
    fn skips_a_reference_file_below_the_minimum() {
        let dir = scratch();
        if !supports_reflink(dir.path()) {
            eprintln!("SKIP: filesystem does not implement FICLONE");
            return;
        }
        let ref_root = dir.path().join("ref");
        std::fs::create_dir_all(&ref_root).expect("ref dirs");
        let small = b"tiny\n".to_vec();
        std::fs::write(ref_root.join("small.txt"), &small).expect("ref file");
        let dest = dir.path().join("work/small.txt");
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("work dirs");

        let cloned = try_reflink(
            &dest,
            "small.txt",
            &blob_id(&small),
            &ref_root,
            HashAlgo::Sha1,
            0o644,
        )
        .expect("reflink");

        assert!(
            !cloned,
            "a clone costs more than the copy below the minimum"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn skips_a_missing_reference_file() {
        let dir = scratch();
        let ref_root = dir.path().join("empty-ref");
        std::fs::create_dir_all(&ref_root).expect("ref dirs");
        let dest = dir.path().join("work/absent.bin");

        let cloned = try_reflink(
            &dest,
            "absent.bin",
            &blob_id(&payload(3)),
            &ref_root,
            HashAlgo::Sha1,
            0o644,
        )
        .expect("reflink");

        assert!(!cloned);
        assert!(!dest.exists());
    }

    #[test]
    fn skips_a_symlinked_reference_file() {
        let dir = scratch();
        if !supports_reflink(dir.path()) {
            eprintln!("SKIP: filesystem does not implement FICLONE");
            return;
        }
        let ref_root = dir.path().join("ref");
        std::fs::create_dir_all(&ref_root).expect("ref dirs");
        let data = payload(4);
        std::fs::write(ref_root.join("real.bin"), &data).expect("real file");
        std::os::unix::fs::symlink("real.bin", ref_root.join("link.bin")).expect("symlink");
        let dest = dir.path().join("work/link.bin");
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("work dirs");

        let cloned = try_reflink(
            &dest,
            "link.bin",
            &blob_id(&data),
            &ref_root,
            HashAlgo::Sha1,
            0o644,
        )
        .expect("reflink");

        assert!(
            !cloned,
            "the clone never follows a symlink out of the reference tree"
        );
        assert!(!dest.exists());
    }
}
