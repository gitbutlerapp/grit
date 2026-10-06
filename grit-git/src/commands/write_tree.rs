//! `grit write-tree` — create a tree object from the current index.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use grit_lib::index::{
    Index, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK, MODE_TREE,
};
use grit_lib::objects::{serialize_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use grit_lib::write_tree as lib_write_tree;

fn resolved_env_index_path(repo: &Repository) -> PathBuf {
    if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(p)
        } else {
            p
        }
    } else {
        repo.index_path()
    }
}

/// Arguments for `grit write-tree`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Allow writing a tree with missing objects.
    #[arg(long = "missing-ok")]
    pub missing_ok: bool,

    /// Write the tree of the named directory (prefix must end with '/').
    #[arg(long)]
    pub prefix: Option<String>,
}

/// Run `grit write-tree`.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let index_path = resolved_env_index_path(&repo);
    let mut index = repo
        .load_index_at(&index_path)
        .with_context(|| format!("loading index {}", index_path.display()))?;

    let prefix = args.prefix.as_deref().unwrap_or("");
    let oid = match write_tree_from_index(&repo.odb, &index, prefix, args.missing_ok)
        .context("building tree from index")
    {
        Ok(oid) => oid,
        Err(err) if is_permission_denied_error(&err) => {
            eprintln!("error: insufficient permission for adding an object to repository database .git/objects");
            eprintln!("fatal: git-write-tree: error building trees");
            std::process::exit(1);
        }
        Err(err) => return Err(err),
    };

    if prefix.is_empty() {
        let cache_tree = lib_write_tree::build_cache_tree_from_index(&repo.odb, &index)
            .context("building cache-tree from index")?;
        index.set_cache_tree(cache_tree);
        repo.write_index_at(&index_path, &mut index)
            .with_context(|| format!("writing index {}", index_path.display()))?;
    }

    println!("{oid}");
    Ok(())
}

fn is_permission_denied_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(ioe) = cause.downcast_ref::<std::io::Error>() {
            if ioe.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
        }
        if let Some(grit_lib::error::Error::Io(ioe)) =
            cause.downcast_ref::<grit_lib::error::Error>()
        {
            if ioe.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
        }
    }
    false
}

/// Build and write tree objects from the index, return the root tree OID.
///
/// Supports a `prefix` to restrict to a subtree.
pub fn write_tree_from_index(
    odb: &Odb,
    index: &Index,
    prefix: &str,
    missing_ok: bool,
) -> Result<ObjectId> {
    if index.entries.iter().any(|e| e.stage() != 0) {
        anyhow::bail!("unmerged entries in index");
    }

    let prefix_bytes = prefix.as_bytes();

    // Collect stage-0 entries matching the prefix.
    // The index is already sorted by path — we exploit this for a single-pass
    // tree build (similar to git's cache_tree_update).
    let entries: Vec<_> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.path.starts_with(prefix_bytes) && !e.intent_to_add())
        .collect();

    // Check for null SHA1 entries
    for entry in &entries {
        if entry.oid.is_zero() {
            let path = String::from_utf8_lossy(&entry.path);
            anyhow::bail!("entry '{}' has a null sha1", path);
        }
    }

    // Verify all referenced objects exist (unless --missing-ok).
    // Skip gitlink entries (mode 160000) — their OIDs reference commits
    // in submodule repositories, not the parent ODB.
    if !missing_ok {
        for entry in &entries {
            if entry.mode == 0o160000 {
                continue; // gitlink: submodule commit, not in our ODB
            }
            if odb.read(&entry.oid).is_err() {
                let path = String::from_utf8_lossy(&entry.path);
                anyhow::bail!("invalid object {} '{}'", entry.oid.to_hex(), path);
            }
        }
    }

    let dir_prefix = if prefix_bytes.ends_with(b"/") {
        &prefix_bytes[..prefix_bytes.len() - 1]
    } else {
        prefix_bytes
    };

    let (oid, _) = build_tree_flat(odb, &entries, 0, dir_prefix)?;
    Ok(oid)
}

/// Single-pass tree builder.
///
/// Processes `entries[start..]` that belong under `dir_prefix`.
/// Returns `(tree_oid, next_index)` where `next_index` is the first entry
/// index NOT consumed by this directory level.
fn build_tree_flat(
    odb: &Odb,
    entries: &[&grit_lib::index::IndexEntry],
    start: usize,
    dir_prefix: &[u8],
) -> Result<(ObjectId, usize)> {
    let mut tree_entries: Vec<TreeEntry> = Vec::new();
    let mut i = start;

    while i < entries.len() {
        let entry = entries[i];
        let path = &entry.path;

        // Check if this entry still belongs under dir_prefix
        let rel = if dir_prefix.is_empty() {
            path.as_slice()
        } else {
            match path.strip_prefix(dir_prefix) {
                Some(rest) => match rest.strip_prefix(b"/") {
                    Some(r) => r,
                    None => break, // doesn't belong here
                },
                None => break, // doesn't belong here
            }
        };

        if let Some(slash) = rel.iter().position(|&b| b == b'/') {
            // Subdirectory — recurse. All entries sharing this directory
            // component will be consumed by the recursive call.
            let child_name = &rel[..slash];
            let sub_prefix: Vec<u8> = if dir_prefix.is_empty() {
                child_name.to_vec()
            } else {
                let mut p = Vec::with_capacity(dir_prefix.len() + 1 + child_name.len());
                p.extend_from_slice(dir_prefix);
                p.push(b'/');
                p.extend_from_slice(child_name);
                p
            };

            let (sub_oid, next) = build_tree_flat(odb, entries, i, &sub_prefix)?;
            tree_entries.push(TreeEntry {
                mode: MODE_TREE,
                name: child_name.to_vec(),
                oid: sub_oid,
            });
            i = next;
        } else {
            // Leaf blob/symlink/gitlink entry
            tree_entries.push(TreeEntry {
                mode: canonicalize_blob_mode(entry.mode),
                name: rel.to_vec(),
                oid: entry.oid,
            });
            i += 1;
        }
    }

    tree_entries.sort_by(|a, b| {
        let a_tree = a.mode == MODE_TREE;
        let b_tree = b.mode == MODE_TREE;
        tree_entry_cmp(&a.name, a_tree, &b.name, b_tree)
    });

    let data = serialize_tree(&tree_entries);
    freshen_tree_entries(odb, &tree_entries);
    let oid = odb.write(ObjectKind::Tree, &data).context("writing tree")?;
    Ok((oid, i))
}

fn freshen_tree_entries(odb: &Odb, tree_entries: &[TreeEntry]) {
    for entry in tree_entries {
        let _ = odb.freshen_object(&entry.oid);
    }
}

fn canonicalize_blob_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o120000 => MODE_SYMLINK,
        0o160000 => MODE_GITLINK,
        0o100000 => {
            if mode & 0o111 != 0 {
                MODE_EXECUTABLE
            } else {
                MODE_REGULAR
            }
        }
        _ => MODE_REGULAR,
    }
}
