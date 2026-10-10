use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use grit_lib::index::{Index, IndexEntry, MODE_EXECUTABLE, MODE_REGULAR};
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::repo::Repository;

pub mod counting_ref_store;
pub mod packfile_kv;
pub mod remote;
pub mod sqlite_odb;

/// Resolve a revision-like string as a ref first, then as a raw object id.
pub fn resolve_name(repo: &Repository, name: &str) -> Result<ObjectId> {
    grit_lib::refs::resolve_ref(&repo.git_dir, name)
        .or_else(|_| name.parse::<ObjectId>().map_err(anyhow::Error::from))
        .with_context(|| format!("could not resolve {name}"))
}

/// Return the current HEAD object id.
pub fn head_oid(repo: &Repository) -> Result<ObjectId> {
    grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD").context("could not resolve HEAD")
}

/// Return the object id of a commit's tree.
pub fn commit_tree(repo: &Repository, commit_oid: ObjectId) -> Result<ObjectId> {
    let object = repo.odb.read(&commit_oid)?;
    if object.kind != ObjectKind::Commit {
        bail!("{commit_oid} is not a commit");
    }
    Ok(parse_commit(&object.data)?.tree)
}

/// Flatten a tree into index entries, recursively descending subtrees.
pub fn entries_from_tree(repo: &Repository, tree_oid: ObjectId) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    append_tree_entries(repo, tree_oid, Path::new(""), &mut entries)?;
    Ok(entries)
}

fn append_tree_entries(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &Path,
    entries: &mut Vec<IndexEntry>,
) -> Result<()> {
    let object = repo.odb.read(&tree_oid)?;
    if object.kind != ObjectKind::Tree {
        bail!("{tree_oid} is not a tree");
    }

    for entry in parse_tree(&object.data)? {
        let path = prefix.join(String::from_utf8_lossy(&entry.name).as_ref());
        if entry.mode == 0o040000 {
            append_tree_entries(repo, entry.oid, &path, entries)?;
        } else {
            entries.push(index_entry(
                path.as_os_str().as_encoded_bytes().to_vec(),
                entry.mode,
                entry.oid,
                0,
                0,
            ));
        }
    }
    Ok(())
}

/// Write tree contents to the work tree for blob and subtree entries.
pub fn checkout_tree(repo: &Repository, tree_oid: ObjectId) -> Result<()> {
    let work_tree = repo
        .work_tree
        .as_ref()
        .context("bare repository has no work tree")?;
    checkout_tree_at(repo, tree_oid, work_tree)
}

fn checkout_tree_at(repo: &Repository, tree_oid: ObjectId, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let object = repo.odb.read(&tree_oid)?;
    if object.kind != ObjectKind::Tree {
        bail!("{tree_oid} is not a tree");
    }

    for entry in parse_tree(&object.data)? {
        let path = dir.join(String::from_utf8_lossy(&entry.name).as_ref());
        if entry.mode == 0o040000 {
            checkout_tree_at(repo, entry.oid, &path)?;
            continue;
        }
        let blob = repo.odb.read(&entry.oid)?;
        if blob.kind != ObjectKind::Blob {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, blob.data)?;
        if entry.mode == MODE_EXECUTABLE {
            let mut perms = fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)?;
        }
    }
    Ok(())
}

/// Build an index entry for a path and blob oid.
pub fn index_entry(
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
    size: u32,
    mtime_sec: u32,
) -> IndexEntry {
    IndexEntry {
        ctime_sec: mtime_sec,
        ctime_nsec: 0,
        mtime_sec,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size,
        oid,
        flags: (path.len().min(0xfff)) as u16,
        flags_extended: None,
        path,
        base_index_pos: 0,
    }
}

/// Stage one work-tree file in the index.
pub fn add_path(repo: &Repository, index: &mut Index, path: &Path) -> Result<()> {
    let work_tree = repo
        .work_tree
        .as_ref()
        .context("bare repository has no work tree")?;
    let full_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        work_tree.join(path)
    };
    let data =
        fs::read(&full_path).with_context(|| format!("could not read {}", path.display()))?;
    let metadata = fs::metadata(&full_path)?;
    let mode = if metadata.permissions().mode() & 0o111 != 0 {
        MODE_EXECUTABLE
    } else {
        MODE_REGULAR
    };
    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
    let rel = path_relative_to_worktree(work_tree, &full_path)?;
    let rel_bytes = rel.as_os_str().as_encoded_bytes().to_vec();
    index.add_or_replace(index_entry(
        rel_bytes,
        mode,
        oid,
        metadata.len().min(u64::from(u32::MAX)) as u32,
        metadata.mtime().max(0) as u32,
    ));
    Ok(())
}

fn path_relative_to_worktree(work_tree: &Path, full_path: &Path) -> Result<PathBuf> {
    let canonical_work_tree = work_tree.canonicalize()?;
    let canonical_path = full_path.canonicalize()?;
    Ok(canonical_path
        .strip_prefix(canonical_work_tree)?
        .to_path_buf())
}
