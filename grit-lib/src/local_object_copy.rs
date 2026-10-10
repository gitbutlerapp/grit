//! Fast object transfer between on-disk repositories (local / `file://` remotes).
//!
//! When the source stores objects in packfiles, linking or copying those packs
//! avoids rebuilding a pack in memory (Git's `--local` fetch/clone path).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::pack;

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

fn hard_link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(());
    }
    ensure_linkable_regular_file(src)?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let _ = fs::remove_file(dst);
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).map_err(Error::Io)?;
    Ok(())
}

fn pack_entry_name_ok(name: &str) -> bool {
    if name.starts_with("tmp_") || name.starts_with(".") {
        return false;
    }
    name.ends_with(".pack")
        || name.ends_with(".idx")
        || name.ends_with(".rev")
        || name.ends_with(".bitmap")
        || name.ends_with(".promisor")
        || name.ends_with(".keep")
        || name.ends_with(".mtimes")
}

/// Link or copy pack-related files from `remote_objects/pack/` into `local_objects/pack/`.
///
/// Skips files that already exist in the destination. Returns the number of
/// files newly installed.
pub(crate) fn link_remote_pack_files(remote_objects: &Path, local_objects: &Path) -> Result<usize> {
    let remote_pack = remote_objects.join("pack");
    if !remote_pack.is_dir() {
        return Ok(0);
    }
    let local_pack = local_objects.join("pack");
    fs::create_dir_all(&local_pack).map_err(Error::Io)?;
    let mut linked = 0usize;
    for entry in fs::read_dir(&remote_pack).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !pack_entry_name_ok(&name_str) {
            continue;
        }
        let dst = local_pack.join(&name);
        if dst.exists() {
            continue;
        }
        hard_link_or_copy(&entry.path(), &dst)?;
        linked += 1;
    }
    Ok(linked)
}

fn link_loose_objects(remote_objects: &Path, local_objects: &Path) -> Result<usize> {
    let mut copied = 0usize;
    for entry in fs::read_dir(remote_objects).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.len() != 2 || !name_str.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let local_shard = local_objects.join(&name);
        fs::create_dir_all(&local_shard).map_err(Error::Io)?;
        let Ok(shard_entries) = fs::read_dir(entry.path()) else {
            continue;
        };
        for obj in shard_entries.flatten() {
            let dst = local_shard.join(obj.file_name());
            if dst.exists() {
                continue;
            }
            hard_link_or_copy(&obj.path(), &dst)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// After pack/loose linking, refresh pack caches so [`Odb::exists`] sees new objects.
pub(crate) fn refresh_local_odb_after_link(local_odb: &Odb) {
    local_odb.invalidate_packs();
    pack::clear_pack_cache();
}

/// Try to satisfy `needed` by linking the remote's on-disk objects into `local`.
///
/// Returns `true` when every oid in `needed` is present in `local_odb` afterward.
pub(crate) fn try_satisfy_via_object_link(
    remote_objects: &Path,
    local_objects: &Path,
    local_odb: &Odb,
    needed: &[ObjectId],
    link_loose: bool,
) -> Result<bool> {
    if needed.is_empty() {
        return Ok(true);
    }
    let linked_packs = link_remote_pack_files(remote_objects, local_objects)?;
    let linked_loose = if link_loose {
        link_loose_objects(remote_objects, local_objects)?
    } else {
        0
    };
    if linked_packs == 0 && linked_loose == 0 {
        return Ok(false);
    }
    refresh_local_odb_after_link(local_odb);
    Ok(needed.iter().all(|oid| local_odb.exists(oid)))
}

/// Hard-link or copy loose object files for `oids` from `source_objects` into `dest_objects`.
///
/// Skips oids that are not stored loose at the source. Returns how many files were linked.
pub(crate) fn link_loose_objects_for_oids(
    source_objects: &Path,
    dest_objects: &Path,
    oids: &[ObjectId],
) -> Result<usize> {
    let mut linked = 0usize;
    for oid in oids {
        let hex = oid.to_hex();
        if hex.len() < 2 {
            continue;
        }
        let (shard, rest) = hex.split_at(2);
        let src = source_objects.join(shard).join(rest);
        if !src.is_file() {
            continue;
        }
        let dst_shard = dest_objects.join(shard);
        fs::create_dir_all(&dst_shard).map_err(Error::Io)?;
        let dst = dst_shard.join(rest);
        if dst.exists() {
            continue;
        }
        hard_link_or_copy(&src, &dst)?;
        linked += 1;
    }
    Ok(linked)
}

/// Oids newly visible in `local_odb` after linking (subset of `needed`).
pub(crate) fn linked_object_set(local_odb: &Odb, needed: &[ObjectId]) -> HashSet<ObjectId> {
    needed
        .iter()
        .copied()
        .filter(|oid| local_odb.exists(oid))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn link_remote_pack_files_hardlinks_repacked_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        fs::write(src.join("f"), b"payload\n").unwrap();
        git(&src, &["add", "f"]);
        git(&src, &["commit", "-qm", "c"]);
        git(&src, &["repack", "-a", "-d", "-f"]);

        let dst_objects = tmp.path().join("dst/objects");
        fs::create_dir_all(&dst_objects).unwrap();
        let linked = link_remote_pack_files(&src.join(".git/objects"), &dst_objects).expect("link");
        assert!(
            linked >= 2,
            "expected at least .pack and .idx, got {linked}"
        );

        let local_odb = Odb::new(&dst_objects);
        refresh_local_odb_after_link(&local_odb);
        let rev = Command::new("git")
            .current_dir(&src)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let tip = ObjectId::from_hex(String::from_utf8_lossy(&rev.stdout).trim()).unwrap();
        assert!(local_odb.exists(&tip));
    }

    #[test]
    fn hard_link_rejects_symlink_in_pack_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_pack = tmp.path().join("src/pack");
        fs::create_dir_all(&src_pack).unwrap();
        let evil = src_pack.join("evil.keep");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("elsewhere", &evil).unwrap();
        }
        #[cfg(not(unix))]
        {
            return;
        }
        let dst_pack = tmp.path().join("dst/pack");
        fs::create_dir_all(&dst_pack).unwrap();
        let dst = dst_pack.join("evil.keep");
        let err = hard_link_or_copy(&evil, &dst).unwrap_err();
        assert!(
            matches!(err, Error::PathError(_)),
            "expected PathError, got {err:?}"
        );
    }
}
