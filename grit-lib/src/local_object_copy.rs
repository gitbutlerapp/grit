//! Fast object transfer between on-disk repositories (local / `file://` remotes).
//!
//! When the source stores objects in packfiles, linking or copying those packs
//! avoids rebuilding a pack in memory. A transfer may only install a pack whose
//! objects are all part of the set being sent ([`link_packs_within`]); linking
//! the whole source object store ([`link_remote_pack_files`],
//! [`link_loose_objects`]) is reserved for a clone from a plain local path
//! (Git's `clone --local`).

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::fs_publish::hard_link_or_copy;
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::odb::{BoundedRead, Odb};
use crate::pack;

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

/// Outcome of [`link_remote_pack_files`].
pub(crate) struct LinkRemotePacksOutcome {
    /// Pack sidecar files (`.pack`, `.idx`, …) installed by this call.
    pub files: usize,
    /// Git objects in newly installed pack indexes (not sidecar file count).
    pub objects: usize,
}

/// Count objects stored under `source_objects` (packed index entries plus loose files).
///
/// Used to size local whole-store link progress before copying.
pub(crate) fn count_store_objects_for_link(source_objects: &Path) -> Result<usize> {
    let mut objects = 0usize;
    let pack_dir = source_objects.join("pack");
    if pack_dir.is_dir() {
        for entry in fs::read_dir(&pack_dir).map_err(Error::Io)? {
            let entry = entry.map_err(Error::Io)?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "idx") {
                let index = pack::read_pack_index_no_verify(&path)?;
                objects += index.len();
            }
        }
    }
    for entry in fs::read_dir(source_objects).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.len() != 2 || !name_str.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(shard) = fs::read_dir(entry.path()) else {
            continue;
        };
        objects += shard.flatten().count();
    }
    Ok(objects)
}

/// Link or copy pack-related files from `remote_objects/pack/` into `local_objects/pack/`.
///
/// Skips files that already exist in the destination. Every `.idx` is linked after
/// the other files, so a reader never finds an index without its pack. Safe to run
/// from several processes into one destination at once. Returns the number of files
/// this call found missing and installed (racing writers may each count a file).
pub(crate) fn link_remote_pack_files(
    remote_objects: &Path,
    local_objects: &Path,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
    total_hint: usize,
) -> Result<LinkRemotePacksOutcome> {
    let remote_pack = remote_objects.join("pack");
    if !remote_pack.is_dir() {
        return Ok(LinkRemotePacksOutcome {
            files: 0,
            objects: 0,
        });
    }
    let local_pack = local_objects.join("pack");
    fs::create_dir_all(&local_pack).map_err(Error::Io)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(&remote_pack).map_err(Error::Io)? {
        let name = entry.map_err(Error::Io)?.file_name();
        if pack_entry_name_ok(&name.to_string_lossy()) {
            names.push(name);
        }
    }
    names.sort_by_key(|name| name.to_string_lossy().ends_with(".idx"));
    let mut linked_files = 0usize;
    let mut linked_objects = 0usize;
    for name in names {
        crate::progress::poll_cancel(progress, cancel)?;
        let dst = local_pack.join(&name);
        if dst.exists() {
            continue;
        }
        hard_link_or_copy(&remote_pack.join(&name), &dst)?;
        linked_files += 1;
        if total_hint > 0 {
            progress.inc(1);
        }
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".idx") {
            let index = pack::read_pack_index_no_verify(&dst)?;
            linked_objects += index.len();
        }
    }
    Ok(LinkRemotePacksOutcome {
        files: linked_files,
        objects: linked_objects,
    })
}

pub(crate) fn link_loose_objects(
    remote_objects: &Path,
    local_objects: &Path,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<usize> {
    let mut copied = 0usize;
    for entry in fs::read_dir(remote_objects).map_err(Error::Io)? {
        crate::progress::poll_cancel(progress, cancel)?;
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
            if copied.is_multiple_of(64) {
                crate::progress::poll_cancel(progress, cancel)?;
            }
            let dst = local_shard.join(obj.file_name());
            if dst.exists() {
                continue;
            }
            hard_link_or_copy(&obj.path(), &dst)?;
            copied += 1;
            progress.inc(1);
        }
    }
    Ok(copied)
}

/// Loose objects reachable from `roots` without reading packed objects.
///
/// Used for local-clone transfer stats after hardlinking a full pack plus loose
/// shards: packed objects are counted from the linked `.idx`, and this walk adds
/// only the small loose tip set.
pub(crate) fn loose_reachable_from_roots(
    odb: &Odb,
    roots: &[ObjectId],
    shallow_grafts: &HashSet<ObjectId>,
    cancel: &dyn crate::progress::Cancel,
) -> Result<HashSet<ObjectId>> {
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut reachable: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for &root in roots {
        if visited.insert(root) {
            queue.push_back(root);
        }
    }
    while let Some(oid) = queue.pop_front() {
        if visited.len().is_multiple_of(64) {
            let mut sink = crate::progress::NullProgress;
            crate::progress::poll_cancel(&mut sink, cancel)?;
        }
        if odb.local_loose_file_size(&oid).is_none() {
            continue;
        }
        reachable.insert(oid);
        // Blobs have no outgoing edges. Keep large payloads out of memory while
        // still reading small commits and trees with a single object lookup. Each oid
        // is visited once, so retaining a second copy in the read cache is unnecessary.
        let obj = match odb.read_unless_larger(&oid, 1024 * 1024, false)? {
            BoundedRead::Object(obj) => obj,
            BoundedRead::Larger if odb.read_info(&oid)?.kind == ObjectKind::Blob => continue,
            BoundedRead::Larger => odb.read(&oid)?,
        };
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                if !shallow_grafts.contains(&oid) {
                    for parent in commit.parents {
                        if visited.insert(parent) {
                            queue.push_back(parent);
                        }
                    }
                }
                if visited.insert(commit.tree) {
                    queue.push_back(commit.tree);
                }
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if visited.insert(entry.oid) {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                if visited.insert(tag.object) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(reachable)
}

/// After pack/loose linking, refresh pack caches so [`Odb::exists`] sees new objects.
pub(crate) fn refresh_local_odb_after_link(local_odb: &Odb) {
    local_odb.invalidate_packs();
    pack::clear_pack_cache();
}

/// Largest share of a linked pack, in percent, that may be objects the destination already
/// has. Above it the pack would mostly duplicate what the destination stores, as on an
/// incremental fetch, and building a pack of only the missing objects is the better trade.
pub(crate) const LINK_ALREADY_PRESENT_PERCENT: usize = 1;

/// Link or copy every source pack whose objects all belong to `send` into `dest_objects/pack/`.
///
/// `send` is the exact set of objects the destination must receive, so a pack that
/// qualifies carries nothing the requested refs do not reach. It may carry a few objects
/// the destination already has (one, or up to [`LINK_ALREADY_PRESENT_PERCENT`] of the pack),
/// such as the empty tree a freshly initialized repository writes before its first fetch;
/// duplicates across packs are harmless and cheaper than rebuilding the pack. Packs
/// holding any other object (other branches, hidden refs, excluded tags, unreachable
/// leftovers) are skipped; the caller packs the remainder itself. Only the `.pack`, `.idx`
/// and the per-pack `.rev` / `.bitmap` files are installed; `.keep`, `.promisor` and
/// `.mtimes` markers describe the source's storage policy and are not carried over.
///
/// # Parameters
///
/// - `source_objects`: the sender's `objects/` directory.
/// - `dest_objects`: the receiver's `objects/` directory.
/// - `send`: objects the receiver lacks and must end up with.
/// - `dest_has`: whether the receiver already stores an object outside `send`.
/// - `progress` / `cancel`: polled between packs.
///
/// # Returns
///
/// The number of objects in the newly installed packs.
///
/// # Errors
///
/// Fails on I/O errors, unreadable source indexes, symlinked / non-regular pack
/// files, or cancellation.
pub(crate) fn link_packs_within(
    source_objects: &Path,
    dest_objects: &Path,
    send: &[ObjectId],
    dest_has: &dyn Fn(&ObjectId) -> bool,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<usize> {
    let source_pack = source_objects.join("pack");
    if send.is_empty() || !source_pack.is_dir() {
        return Ok(0);
    }
    let send: HashSet<ObjectId> = send.iter().copied().collect();
    let dest_pack = dest_objects.join("pack");
    let mut linked = 0usize;
    for entry in fs::read_dir(&source_pack).map_err(Error::Io)? {
        crate::progress::poll_cancel(progress, cancel)?;
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(stem) = name_str.strip_suffix(".idx") else {
            continue;
        };
        if !pack_entry_name_ok(&name_str) || dest_pack.join(&name).exists() {
            continue;
        }
        if !source_pack.join(format!("{stem}.pack")).is_file() {
            continue;
        }
        let index = pack::read_pack_index_no_verify(&entry.path())?;
        let mut already_present_budget = (index.len() * LINK_ALREADY_PRESENT_PERCENT / 100).max(1);
        let within = !index.is_empty()
            && (0..index.len()).all(|i| {
                ObjectId::from_bytes(index.oid_at(i)).is_ok_and(|oid| {
                    if send.contains(&oid) {
                        return true;
                    }
                    if already_present_budget == 0 || !dest_has(&oid) {
                        return false;
                    }
                    already_present_budget -= 1;
                    true
                })
            });
        if !within {
            continue;
        }
        fs::create_dir_all(&dest_pack).map_err(Error::Io)?;
        // The `.idx` goes in last so a concurrent reader never sees an index without its pack.
        for ext in ["pack", "rev", "bitmap", "idx"] {
            let src = source_pack.join(format!("{stem}.{ext}"));
            if ext == "pack" || ext == "idx" || src.is_file() {
                hard_link_or_copy(&src, &dest_pack.join(format!("{stem}.{ext}")))?;
            }
        }
        linked += index.len();
    }
    Ok(linked)
}

/// Hard-link or copy loose object files for `oids` from `source_objects` into `dest_objects`.
///
/// Skips oids that are not stored loose at the source. Returns how many files were linked.
pub(crate) fn link_loose_objects_for_oids(
    source_objects: &Path,
    dest_objects: &Path,
    oids: &[ObjectId],
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<usize> {
    let mut linked = 0usize;
    for (i, oid) in oids.iter().enumerate() {
        if i.is_multiple_of(64) {
            crate::progress::poll_cancel(progress, cancel)?;
        }
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
        progress.inc(1);
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
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let linked = link_remote_pack_files(
            &src.join(".git/objects"),
            &dst_objects,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
            0,
        )
        .expect("link");
        assert!(
            linked.files >= 2,
            "expected at least .pack and .idx, got {}",
            linked.files
        );
        assert!(
            linked.objects >= 3,
            "expected packed objects, got {}",
            linked.objects
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

    #[cfg(unix)]
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn parallel_link_remote_pack_files_into_one_repo_all_succeed() {
        use std::os::unix::fs::PermissionsExt;

        for source_mode in [0o444, 0o644] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let src = tmp.path().join("src");
            fs::create_dir_all(&src).unwrap();
            git(&src, &["init", "-q", "-b", "main"]);
            for i in 0..20 {
                fs::write(src.join(format!("f{i}")), format!("payload {i}\n")).unwrap();
            }
            git(&src, &["add", "."]);
            git(&src, &["commit", "-qm", "c"]);
            git(&src, &["repack", "-a", "-d", "-f"]);
            let src_objects = src.join(".git/objects");
            let src_pack_dir = src_objects.join("pack");
            let mut before = Vec::new();
            for entry in fs::read_dir(&src_pack_dir).unwrap().flatten() {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(source_mode)).unwrap();
                before.push((entry.path(), fs::read(entry.path()).unwrap()));
            }

            let dst = tmp.path().join("dst");
            fs::create_dir_all(&dst).unwrap();
            git(&dst, &["init", "-q", "-b", "main"]);
            let dst_objects = std::sync::Arc::new(dst.join(".git/objects"));
            let src_objects = std::sync::Arc::new(src_objects);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    let (s, d, b) = (src_objects.clone(), dst_objects.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        b.wait();
                        link_remote_pack_files(
                            &s,
                            &d,
                            &mut crate::progress::NullProgress,
                            &crate::progress::NeverCancel,
                            0,
                        )
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().expect("parallel link succeeds");
            }

            for (path, bytes) in &before {
                assert_eq!(&fs::read(path).unwrap(), bytes, "source changed: {path:?}");
            }
            git(&src, &["fsck", "--strict"]);
            let tip = Command::new("git")
                .current_dir(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            git(
                &dst,
                &[
                    "update-ref",
                    "refs/heads/main",
                    String::from_utf8_lossy(&tip.stdout).trim(),
                ],
            );
            git(&dst, &["fsck", "--strict"]);
        }
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
