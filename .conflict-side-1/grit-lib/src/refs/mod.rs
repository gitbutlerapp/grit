//! Reference storage — files backend + reftable backend.
//!
//! Git stores references as text files under `<git-dir>/refs/` (and
//! `<git-dir>/packed-refs` for the packed backend).  Each loose ref file
//! contains either:
//!
//! - A 40-character hex SHA-1 followed by a newline, **or**
//! - The string `"ref: <target>\n"` for symbolic refs.
//!
//! `HEAD` is a special case: it is normally a symbolic ref but may also be
//! detached (pointing directly at a commit hash).
//!
//! When `extensions.refStorage = reftable`, the reftable backend is used
//! instead.  The public API is the same; dispatch is handled internally.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/refs)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! assert!(_repo.refs().read_raw("HEAD")?.is_some());
//!
//! # Ok(())
//! # }
//! ```

pub mod handle;
pub mod store;

mod domain_reexports;
pub use domain_reexports::*;
pub use handle::References;

/// In-process ref-transaction and loose-object prune helpers.
pub mod gc {
    pub use crate::gc::*;
}

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use store::{
    open_ref_store, Expected, RawRef as StoreRawRef, RefEntry, RefStorageFormat, RefStore,
    RefTransaction, RefUpdate, RefUpdateFlags, ReflogUpdate,
};

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::config::ConfigSet;
use crate::error::{Error, RefLockError, Result};
use crate::objects::ObjectId;
use crate::pack;

/// Maximum symbolic ref hops when resolving a ref (Git `SYMREF_MAXDEPTH`).
pub const SYMREF_MAXDEPTH: usize = 5;

fn map_public_ref_store_error(err: Error, refname: &str) -> Error {
    match err {
        Error::RefStore(store::RefStoreError::SymrefLoop) => {
            Error::InvalidRef(format!("ref symlink too deep: {refname}"))
        }
        Error::RefStore(store::RefStoreError::Corrupt(msg))
            if msg.contains("not-a-ref") || msg.contains("reference broken") =>
        {
            Error::InvalidRef(format!("cannot update ref '{refname}': reference broken"))
        }
        other => other,
    }
}

/// Loose-files checks shared by legacy [`write_ref_at_storage`] and RefStore-backed writes.
fn files_legacy_write_precheck(git_dir: &Path, refname: &str) -> Result<()> {
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return Ok(());
    }
    let storage_dir = ref_storage_dir(git_dir, refname);
    let stor = crate::ref_namespace::storage_ref_name(refname);
    let path = storage_dir.join(stor);
    remove_empty_ref_directory(&path);
    if fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
    {
        let display = ref_path_for_display(&path);
        return Err(crate::error::RefLockError::DirectoryInTheWay {
            refname: refname.to_owned(),
            path: display,
        }
        .into());
    }
    if path.is_file() && matches!(read_ref_file(&path), Err(Error::InvalidRef(_))) {
        return Err(Error::InvalidRef(format!(
            "cannot update ref '{refname}': reference broken"
        )));
    }
    Ok(())
}

fn legacy_ref_update_flags(refname: &str) -> RefUpdateFlags {
    let mut flags = RefUpdateFlags::default();
    // Direct updates to `HEAD` detach or reattach the worktree HEAD file; do not peel symrefs.
    if refname == "HEAD" {
        flags.no_deref = true;
    }
    flags
}

pub(crate) fn commit_store_update(git_dir: &Path, update: RefUpdate) -> Result<()> {
    let store = open_ref_store(git_dir)?;
    commit_ref_update_on_store(store.as_ref(), update)
}

/// Apply one ref update through an already-selected [`RefStore`] (for example the store on
/// [`crate::repo::Repository`] after [`crate::repo::Repository::with_ref_store`]).
///
/// # Errors
///
/// Propagates validation, compare-and-swap, and backend errors.
pub fn commit_ref_update_on_store(store: &dyn RefStore, update: RefUpdate) -> Result<()> {
    let refname = update.name.clone();
    let txn = RefTransaction::new()
        .update(update)
        .map_err(crate::error::Error::from)?;
    let prepared = store
        .prepare(txn)
        .map_err(|e| map_public_ref_store_error(Error::from(e), &refname))?;
    prepared
        .commit()
        .map_err(|e| map_public_ref_store_error(Error::from(e), &refname))
}

/// Point `refname` at `oid` using `store`, optionally running loose-files prechecks when
/// `store` uses the files backend under `git_dir`.
///
/// # Errors
///
/// Same as [`write_ref`], except updates go through `store` instead of reopening storage from
/// `git_dir`.
pub fn update_direct_ref_on_store(
    store: &dyn RefStore,
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
) -> Result<()> {
    store_write_value_on_store(
        store,
        git_dir,
        refname,
        Some(StoreRawRef::Direct(*oid)),
        Expected::Any,
        None,
    )
}

fn store_write_value_on_store(
    store: &dyn RefStore,
    git_dir: &Path,
    refname: &str,
    new_value: Option<StoreRawRef>,
    expected: Expected,
    reflog: Option<ReflogUpdate>,
) -> Result<()> {
    ensure_refname_safe_for_storage(refname)?;
    if new_value.is_some() && store.format() == RefStorageFormat::Files {
        files_legacy_write_precheck(git_dir, refname)?;
    }
    commit_ref_update_on_store(
        store,
        RefUpdate {
            name: refname.to_owned(),
            new_value,
            expected,
            reflog,
            flags: legacy_ref_update_flags(refname),
        },
    )
}

/// Apply several ref updates in one [`RefStore::prepare`] / commit (all-or-nothing).
///
/// Used by batch ref paths (`fetch`, `push`, `receive-pack`, [`crate::gc::update_refs`]).
///
/// # Errors
///
/// Returns [`crate::refs::store::RefStoreError`] for duplicate names, CAS failures,
/// lock conflicts, and namespace conflicts. Propagates I/O and corrupt-store failures.
pub fn commit_ref_store_batch(git_dir: &Path, updates: &[RefUpdate]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let label = updates.first().map(|u| u.name.as_str()).unwrap_or("refs");
    let store = open_ref_store(git_dir)?;
    let mut txn = RefTransaction::new();
    for update in updates {
        let name = update.name.clone();
        txn = txn
            .update(update.clone())
            .map_err(|e| map_public_ref_store_error(Error::from(e), &name))?;
    }
    let prepared = store
        .prepare(txn)
        .map_err(|e| map_public_ref_store_error(Error::from(e), label))?;
    prepared
        .commit()
        .map_err(|e| map_public_ref_store_error(Error::from(e), label))
}

fn store_write_value(
    git_dir: &Path,
    refname: &str,
    new_value: Option<StoreRawRef>,
    expected: Expected,
    reflog: Option<ReflogUpdate>,
) -> Result<()> {
    let store = open_ref_store(git_dir)?;
    store_write_value_on_store(
        store.as_ref(),
        git_dir,
        refname,
        new_value,
        expected,
        reflog,
    )
}

fn collect_resolved_refs(store: &dyn RefStore, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    let mut results = Vec::new();
    store.for_each_ref(prefix, &mut |entry: &RefEntry| {
        let oid = entry
            .peeled
            .or_else(|| match &entry.value {
                StoreRawRef::Direct(oid) => Some(*oid),
                StoreRawRef::Symbolic(target) => store.resolve(target).ok(),
            })
            .or_else(|| store.resolve(&entry.name).ok());
        if let Some(oid) = oid {
            results.push((entry.name.clone(), oid));
        }
        ControlFlow::Continue(())
    })?;
    Ok(results)
}

/// A symbolic or direct reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    /// Direct reference: stores an [`ObjectId`].
    Direct(ObjectId),
    /// Symbolic reference: stores the name of the target ref.
    Symbolic(String),
}

/// Read a single reference file from `path`.
///
/// # Errors
///
/// - [`Error::InvalidRef`] if the file content is not a valid ref.
/// - [`Error::Io`] on filesystem errors.
pub fn read_ref_file(path: &Path) -> Result<Ref> {
    if let Ok(target) = fs::read_link(path) {
        return Ok(Ref::Symbolic(target.to_string_lossy().into_owned()));
    }

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        // `refs/heads/master` can be a directory when a branch named `master/...` exists, and a
        // probe like `refs/remotes/v1` can hit ENOTDIR when `refs/remotes` is a file. Treat both
        // like a missing loose ref so optional DWIM candidates fall through cleanly.
        Err(e)
            if e.kind() == io::ErrorKind::IsADirectory
                || e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::EISDIR)
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            return Err(Error::Io(io::Error::new(io::ErrorKind::NotFound, e)));
        }
        Err(e) => return Err(Error::Io(e)),
    };
    let content = content.trim_end_matches('\n');
    parse_ref_content(content)
}

/// Parse the content of a ref file (without trailing newline).
pub(crate) fn parse_ref_content(content: &str) -> Result<Ref> {
    if let Some(target) = content.strip_prefix("ref: ") {
        Ok(Ref::Symbolic(target.trim().to_owned()))
    } else if let Some(token) = content.split_whitespace().next() {
        if ObjectId::is_full_hex(token) {
            let oid: ObjectId = token.parse()?;
            Ok(Ref::Direct(oid))
        } else if content == "unknown-oid" {
            const PLACEHOLDER: &[u8; 20] = b"GritUnknownOidPlc!X!";
            let oid = ObjectId::from_bytes(PLACEHOLDER)?;
            Ok(Ref::Direct(oid))
        } else {
            Err(Error::InvalidRef(content.to_owned()))
        }
    } else {
        Err(Error::InvalidRef(content.to_owned()))
    }
}

/// Resolve a reference to its target [`ObjectId`], following symbolic refs.
///
/// Dispatches to the reftable backend when `extensions.refStorage = reftable`.
///
/// # Parameters
///
/// - `git_dir` — path to the git directory.
/// - `refname` — reference name (e.g. `"HEAD"`, `"refs/heads/main"`).
///
/// # Errors
///
/// - [`Error::InvalidRef`] if the ref is malformed or forms a cycle.
/// - [`Error::ObjectNotFound`] if a symbolic target does not exist.
pub fn resolve_ref(git_dir: &Path, refname: &str) -> Result<ObjectId> {
    match open_ref_store(git_dir)?.resolve(refname) {
        Ok(oid) => Ok(oid),
        Err(Error::RefStore(store::RefStoreError::Corrupt(msg)))
            if msg.starts_with("ref not found:") =>
        {
            Err(Error::InvalidRef(msg))
        }
        Err(err) => Err(map_public_ref_store_error(err, refname)),
    }
}

/// Determine the common git directory for worktree-aware ref resolution.
///
/// If `<git_dir>/commondir` exists, its contents point to the shared
/// git directory. Returns `None` when git_dir is already the common dir.
pub fn common_dir(git_dir: &Path) -> Option<PathBuf> {
    let commondir_file = git_dir.join("commondir");
    let raw = fs::read_to_string(commondir_file).ok()?;
    let rel = raw.trim();
    // Match Git: `commondir` may be relative to this gitdir or an absolute path (see
    // `git worktree add` and `refs/files-backend.c`).
    let path = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        git_dir.join(rel)
    };
    path.canonicalize().ok()
}

/// Internal recursive resolver with cycle detection.
///
/// When operating inside a worktree, `common` points to the shared git
/// directory where most refs live.  The worktree-specific `git_dir` is
/// checked first for HEAD and per-worktree refs.
#[allow(dead_code)]
fn resolve_ref_depth(
    git_dir: &Path,
    _common: Option<&Path>,
    refname: &str,
    depth: usize,
) -> Result<ObjectId> {
    if depth >= SYMREF_MAXDEPTH {
        return Err(Error::InvalidRef(format!(
            "ref symlink too deep: {refname}"
        )));
    }

    let (store, stor_name) = crate::worktree_ref::resolve_ref_storage(git_dir, refname);
    let storage_owned = crate::ref_namespace::storage_ref_name(&stor_name);
    let try_names: Vec<&str> =
        if stor_name == "HEAD" && crate::ref_namespace::ref_storage_prefix_default().is_some() {
            vec![storage_owned.as_str()]
        } else if storage_owned != stor_name {
            vec![storage_owned.as_str(), stor_name.as_str()]
        } else {
            vec![stor_name.as_str()]
        };

    for name in try_names {
        let path = store.join(name);
        match read_ref_file(&path) {
            Ok(Ref::Direct(oid)) => return Ok(oid),
            Ok(Ref::Symbolic(target)) => {
                return resolve_ref_depth(git_dir, None, &target, depth + 1);
            }
            Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        if let Some(oid) = lookup_packed_ref(&store, name)? {
            return Ok(oid);
        }
    }

    Err(Error::InvalidRef(format!("ref not found: {refname}")))
}

/// Outcome of a single storage-level ref lookup (Git `refs_read_raw_ref` style).
///
/// This checks whether a ref **name** exists in the ref store without applying
/// DWIM rules. A symbolic ref is considered to exist if its ref file (or
/// reftable record) is present, even when the target is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawRefLookup {
    /// A loose ref file, packed ref line, or reftable record exists for this name.
    Exists,
    /// No ref is recorded under this exact name.
    NotFound,
    /// A path component exists as a directory where a ref file was expected (e.g. `refs/heads`).
    IsDirectory,
}

/// Return whether `refname` exists as a ref in the repository's ref storage.
///
/// This matches `git refs exists` / `git show-ref --exists`: no DWIM, no
/// resolution of symbolic targets. Dispatches to the reftable backend when
/// configured.
///
/// # Parameters
///
/// - `git_dir` — path to the git directory (worktree gitdir or bare `.git`).
/// - `refname` — full ref name (e.g. `HEAD`, `refs/heads/main`, `CHERRY_PICK_HEAD`).
///
/// # Errors
///
/// Propagates I/O and reftable errors other than "not found".
pub fn read_raw_ref(git_dir: &Path, refname: &str) -> Result<RawRefLookup> {
    let store = open_ref_store(git_dir)?;
    match store.read_raw(refname)? {
        Some(_) => Ok(RawRefLookup::Exists),
        None => {
            let (storage_dir, stor_name) =
                crate::worktree_ref::resolve_ref_storage(git_dir, refname);
            let path = storage_dir.join(&stor_name);
            if fs::symlink_metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                Ok(RawRefLookup::IsDirectory)
            } else {
                Ok(RawRefLookup::NotFound)
            }
        }
    }
}

/// Lock file path for a loose ref file (`<refpath>.lock`), matching Git's naming for nested refs.
#[must_use]
pub fn lock_path_for_ref(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

#[allow(dead_code)]
fn read_raw_ref_at(path: PathBuf) -> Result<Option<RawRefLookup>> {
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if meta.is_dir() {
                return Ok(Some(RawRefLookup::IsDirectory));
            }
            Ok(Some(RawRefLookup::Exists))
        }
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::Io(e)),
    }
}

fn packed_ref_with_prefix(git_dir: &Path, prefix_with_slash: &str) -> Result<Option<String>> {
    let packed = store::paths::packed_refs_path(git_dir);
    let content = match fs::read_to_string(&packed) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut best: Option<String> = None;
    for line in content.lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let _oid = parts.next();
        let Some(name) = parts.next() else {
            continue;
        };
        let name = name.trim();
        if name.starts_with(prefix_with_slash) {
            let take = match &best {
                None => true,
                Some(b) => name < b.as_str(),
            };
            if take {
                best = Some(name.to_owned());
            }
        }
    }
    Ok(best)
}

fn packed_ref_name_exists(git_dir: &Path, refname: &str) -> Result<bool> {
    Ok(read_packed_refs_map(git_dir)?.contains_key(refname))
}

fn refname_namespace_conflicts(existing: &str, candidate: &str) -> bool {
    if existing == candidate {
        return false;
    }
    existing
        .strip_prefix(candidate)
        .is_some_and(|rest| rest.starts_with('/'))
        || candidate
            .strip_prefix(existing)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn packed_ref_namespace_conflict(git_dir: &Path, refname: &str) -> Result<bool> {
    let map = read_packed_refs_map(git_dir)?;
    Ok(map
        .keys()
        .any(|existing| refname_namespace_conflicts(existing, refname)))
}

/// Returns true if `packed-refs` in the ref storage directory for `refname` contains that name.
///
/// Used to mirror Git's `is_packed_transaction_needed` behaviour: deleting a ref may open a nested
/// packed-refs transaction whose abort runs the `reference-transaction` hook in the `aborted`
/// state between `preparing` and `prepared` on the main transaction.
///
/// Returns `false` for reftable repositories and for `HEAD` (packed-refs does not store `HEAD`).
///
/// # Errors
///
/// Propagates I/O errors reading `packed-refs`.
pub fn packed_refs_entry_exists(git_dir: &Path, refname: &str) -> Result<bool> {
    if refname == "HEAD" {
        return Ok(false);
    }
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return Ok(false);
    }
    let storage_dir = ref_storage_dir(git_dir, refname);
    packed_ref_name_exists(&storage_dir, refname)
}

/// Why a reference name cannot be created (Git `refs_verify_refname_available` style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefnameUnavailable {
    /// An ancestor ref already exists in the store (e.g. `refs/foo` blocks `refs/foo/bar`).
    AncestorExists {
        /// Existing ref that blocks creation.
        blocking: String,
        /// Ref the caller tried to create.
        new_ref: String,
    },
    /// A descendant ref already exists (e.g. `refs/foo/bar` blocks `refs/foo`).
    DescendantExists {
        /// Existing ref under `new_ref/`.
        blocking: String,
        /// Ref the caller tried to create.
        new_ref: String,
    },
    /// Two refnames in the same batch are mutually incompatible (parent vs child).
    SameBatch {
        /// Ref being validated (Git prints this first).
        refname: String,
        /// Other ref in the batch (parent dirname or descendant name).
        other: String,
    },
}

impl RefnameUnavailable {
    /// Suffix after `cannot lock ref '<display_ref>': ` for stderr (no trailing newline).
    #[must_use]
    pub fn lock_message_suffix(&self) -> String {
        match self {
            RefnameUnavailable::AncestorExists { blocking, new_ref } => {
                format!("'{blocking}' exists; cannot create '{new_ref}'")
            }
            RefnameUnavailable::DescendantExists { blocking, new_ref } => {
                format!("'{blocking}' exists; cannot create '{new_ref}'")
            }
            RefnameUnavailable::SameBatch { refname, other } => {
                format!("cannot process '{refname}' and '{other}' at the same time")
            }
        }
    }
}

fn find_descendant_in_sorted_extras(
    dirname_with_slash: &str,
    extras: &BTreeSet<String>,
) -> Option<String> {
    let start = extras
        .range(dirname_with_slash.to_string()..)
        .next()
        .cloned()?;
    if start.starts_with(dirname_with_slash) {
        Some(start)
    } else {
        None
    }
}

/// Whether a network-advertised ref name is safe to accept during fetch.
///
/// Delegates to [`crate::check_ref_format::is_valid_fetch_advertised_ref`].
#[must_use]
pub fn is_valid_fetch_advertised_ref(name: &str) -> bool {
    crate::check_ref_format::is_valid_fetch_advertised_ref(name)
}

/// Whether a name from a protocol-v2 `ls-refs` response should be kept.
///
/// `HEAD` is valid without a `refs/` prefix; every other name must pass
/// [`is_valid_fetch_advertised_ref`].
#[must_use]
pub fn is_valid_ls_refs_advertised_name(name: &str) -> bool {
    name == "HEAD" || is_valid_fetch_advertised_ref(name)
}

/// Whether a ref name is safe to store under the ref backend (post-refspec mapping).
#[must_use]
pub fn is_valid_storable_ref_name(name: &str) -> bool {
    is_valid_fetch_advertised_ref(name)
}

/// Whether a symref target from a ref advertisement is safe to record.
#[must_use]
pub fn is_valid_advertised_symref_target(name: &str) -> bool {
    crate::check_ref_format::is_valid_advertised_symref_target(name)
}

/// Verify that `refname` can be created without directory/file conflicts with the ref store
/// and with other refnames queued in the same transaction (`extras`).
///
/// `skip` names are ignored when checking the filesystem (updates that delete or replace
/// those refs in the same batch). Matches Git's `refs_verify_refname_available`.
///
/// # Parameters
///
/// - `git_dir` — repository git directory.
/// - `refname` — full ref name to create.
/// - `extras` — other refnames touched in the same stdin batch / transaction (sorted set).
/// - `skip` — refnames that may be deleted or updated away in the same batch.
pub fn verify_refname_available_for_create(
    git_dir: &Path,
    refname: &str,
    extras: &BTreeSet<String>,
    skip: &HashSet<String>,
) -> std::result::Result<(), RefnameUnavailable> {
    // `Repository::git_dir` may be a relative path (e.g. `.git`); resolve so lookups match the
    // on-disk ref store regardless of process cwd (test harness runs from `trash/.git/...`).
    let git_dir = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let mut seen_dirnames: HashSet<String> = HashSet::new();
    let segments: Vec<&str> = refname.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() <= 1 {
        // No slash-separated parent prefixes (e.g. `HEAD`).
    } else {
        let mut dirname = String::new();
        for part in &segments[..segments.len() - 1] {
            if !dirname.is_empty() {
                dirname.push('/');
            }
            dirname.push_str(part);

            if !seen_dirnames.insert(dirname.clone()) {
                continue;
            }

            if skip.contains(&dirname) {
                continue;
            }

            match read_raw_ref(&git_dir, &dirname) {
                Ok(RawRefLookup::Exists) => {
                    return Err(RefnameUnavailable::AncestorExists {
                        blocking: dirname.clone(),
                        new_ref: refname.to_owned(),
                    });
                }
                // A directory at `refs/prefix` is normal when storing `refs/prefix/child`; only a
                // real ref (loose file or packed line) blocks creating `refs/prefix/...`.
                Ok(RawRefLookup::NotFound | RawRefLookup::IsDirectory) => {}
                Err(_) => {}
            }

            if extras.contains(&dirname) {
                return Err(RefnameUnavailable::SameBatch {
                    refname: refname.to_owned(),
                    other: dirname.clone(),
                });
            }
        }
    }

    let mut leaf_dir = String::with_capacity(refname.len() + 1);
    leaf_dir.push_str(refname);
    leaf_dir.push('/');

    // Loose descendants can only exist inside a `<refname>/` directory; packed
    // and stray-file descendants are checked separately below. Skipping the
    // listing when no such directory exists keeps batch creates linear: listing
    // a missing directory walks its parent, so a clone writing N tags would
    // otherwise read every sibling ref N times.
    let packed_dir = common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());
    let may_have_loose_descendants = packed_dir.join("reftable").is_dir()
        || git_dir.join(refname).is_dir()
        || packed_dir.join(refname).is_dir();
    let under = if may_have_loose_descendants {
        list_refs(&git_dir, &leaf_dir).unwrap_or_default()
    } else {
        Vec::new()
    };
    if under.is_empty() {
        if let Ok(Some(name)) = packed_ref_with_prefix(&packed_dir, &leaf_dir) {
            if !skip.contains(&name) {
                return Err(RefnameUnavailable::DescendantExists {
                    blocking: name,
                    new_ref: refname.to_owned(),
                });
            }
        }
        if packed_dir != git_dir {
            if let Ok(Some(name)) = packed_ref_with_prefix(&git_dir, &leaf_dir) {
                if !skip.contains(&name) {
                    return Err(RefnameUnavailable::DescendantExists {
                        blocking: name,
                        new_ref: refname.to_owned(),
                    });
                }
            }
        }
    }
    if under.is_empty()
        && fs::symlink_metadata(git_dir.join(refname))
            .map(|m| m.is_dir())
            .unwrap_or(false)
    {
        let mut blocking: Option<String> = None;
        let dir_path = git_dir.join(refname);
        if let Ok(read) = fs::read_dir(&dir_path) {
            for entry in read.flatten() {
                let path = entry.path();
                let Ok(meta) = fs::metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let full = format!("{refname}/{name}");
                blocking = Some(full);
                break;
            }
        }
        if let Some(b) = blocking {
            if !skip.contains(&b) {
                return Err(RefnameUnavailable::DescendantExists {
                    blocking: b,
                    new_ref: refname.to_owned(),
                });
            }
        }
    }

    for (existing, _) in under {
        if skip.contains(&existing) {
            continue;
        }
        return Err(RefnameUnavailable::DescendantExists {
            blocking: existing,
            new_ref: refname.to_owned(),
        });
    }

    if let Some(extra) = find_descendant_in_sorted_extras(&leaf_dir, extras) {
        if !skip.contains(&extra) {
            return Err(RefnameUnavailable::SameBatch {
                refname: refname.to_owned(),
                other: extra,
            });
        }
    }

    Ok(())
}

/// One ref create/update/delete in a batch transaction (see [`crate::gc::update_refs`]).
#[derive(Clone, Debug)]
pub struct RefBatchItem {
    /// Full ref name (e.g. `refs/heads/main`).
    pub name: String,
    /// New oid to write, or `None` to delete the ref.
    pub new_oid: Option<ObjectId>,
}

/// Verify that every *new* ref in a batch can be created without D/F or namespace
/// conflicts with the existing store or other items in the batch.
///
/// Matches Git's pre-lock `refs_verify_refnames_available` checks with an empty
/// `skip` list (refs scheduled for deletion in the same batch do not suppress
/// ancestor/descendant conflicts — see t1404 D/F batch tests).
///
/// # Errors
///
/// Returns [`RefnameUnavailable`] describing the first conflict.
pub fn verify_ref_transaction_batch(
    git_dir: &Path,
    items: &[RefBatchItem],
) -> std::result::Result<(), RefnameUnavailable> {
    if let Some((parent, child)) =
        batch_refname_prefix_conflict(items.iter().map(|i| i.name.as_str()))
    {
        return Err(RefnameUnavailable::SameBatch {
            refname: child,
            other: parent,
        });
    }

    if crate::ref_storage::detect_format(git_dir).is_ok_and(|f| f.is_reftable()) {
        return crate::reftable::verify_ref_transaction_batch_reftable(git_dir, items);
    }

    let extras: BTreeSet<String> = items.iter().map(|i| i.name.clone()).collect();
    let empty_skip = HashSet::new();
    for item in items {
        let Some(_oid) = item.new_oid else {
            continue;
        };
        if resolve_ref(git_dir, &item.name).is_ok() {
            continue;
        }
        verify_refname_available_for_create(git_dir, &item.name, &extras, &empty_skip)?;
    }
    Ok(())
}

/// First strict prefix conflict in a batch, if any (sorted adjacent scan).
pub(crate) fn batch_refname_prefix_conflict<'a>(
    names: impl Iterator<Item = &'a str>,
) -> Option<(String, String)> {
    let mut sorted: Vec<&str> = names.collect();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if refname_is_strict_prefix(a, b) {
            return Some((a.to_owned(), b.to_owned()));
        }
        if refname_is_strict_prefix(b, a) {
            return Some((b.to_owned(), a.to_owned()));
        }
    }
    None
}

fn refname_is_strict_prefix(parent: &str, child: &str) -> bool {
    child.len() > parent.len()
        && child.as_bytes().get(parent.len()) == Some(&b'/')
        && child.starts_with(parent)
}

/// Removes `lock` on drop unless [`disarm`](Self::disarm) was called after a successful delete.
#[allow(dead_code)]
struct LooseRefLockGuard<'a> {
    lock: &'a Path,
    armed: bool,
}

impl<'a> LooseRefLockGuard<'a> {
    fn new(lock: &'a Path) -> Self {
        Self { lock, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LooseRefLockGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            abort_loose_ref_lock(self.lock);
        }
    }
}

#[allow(dead_code)]
fn delete_loose_ref_after_lock(
    storage_dir: &Path,
    stor: &str,
    path: &Path,
    lock: &Path,
) -> Result<()> {
    let mut lock_guard = LooseRefLockGuard::new(lock);
    remove_packed_ref(storage_dir, stor)?;

    remove_empty_ref_directory(path);
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e)
            if e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::ENOTDIR) => {}
        Err(e)
            if e.raw_os_error() == Some(libc::EISDIR) || e.raw_os_error() == Some(libc::EPERM) => {}
        Err(e) => return Err(Error::Io(e)),
    }

    let log_path = storage_dir.join("logs").join(stor);
    let _ = fs::remove_file(&log_path);

    let logs_root = storage_dir.join("logs");
    let mut parent = log_path.parent();
    while let Some(p) = parent {
        if p == logs_root.as_path() || !p.starts_with(&logs_root) {
            break;
        }
        if fs::remove_dir(p).is_err() {
            break;
        }
        parent = p.parent();
    }

    fs::remove_file(lock)?;
    lock_guard.disarm();
    Ok(())
}

fn packed_refs_unexpected_line(path: &Path, line: impl Into<String>) -> Error {
    Error::PackedRefsUnexpectedLine {
        path: path.display().to_string(),
        line: line.into(),
    }
}

fn packed_refs_unterminated_line(path: &Path, line: impl Into<String>) -> Error {
    Error::PackedRefsUnexpectedLine {
        path: path.display().to_string(),
        line: line.into(),
    }
}

fn parse_packed_ref_peel_line(path: &Path, line: &str, expect_peel: bool) -> Result<()> {
    if !expect_peel {
        return Err(packed_refs_unexpected_line(path, line));
    }
    let inner = line
        .strip_prefix('^')
        .ok_or_else(|| packed_refs_unexpected_line(path, line))?;
    let mut j = 0usize;
    while j < inner.len() && inner.as_bytes()[j].is_ascii_hexdigit() {
        j += 1;
    }
    if !ObjectId::is_hex_len(j) || j != inner.len() {
        return Err(packed_refs_unexpected_line(path, line));
    }
    Ok(())
}

fn packed_ref_data_line_refname(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut j = 0usize;
    while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
        j += 1;
    }
    if !ObjectId::is_hex_len(j) || j + 1 >= bytes.len() || bytes[j] != b' ' {
        return None;
    }
    let refname = &line[j + 1..];
    if refname.is_empty() || refname.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some(refname)
}

fn parse_packed_ref_data_line(
    path: &Path,
    line: &str,
    ref_opts: &RefNameOptions,
) -> Result<(String, ObjectId)> {
    let bytes = line.as_bytes();
    let mut j = 0usize;
    while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
        j += 1;
    }
    if !ObjectId::is_hex_len(j) || j + 1 >= bytes.len() || bytes[j] != b' ' {
        return Err(packed_refs_unexpected_line(path, line));
    }
    let refname = &line[j + 1..];
    if refname.is_empty() || refname.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
        return Err(packed_refs_unexpected_line(path, line));
    }
    if check_refname_format(refname, ref_opts).is_err() {
        return Err(packed_refs_unexpected_line(path, line));
    }
    let oid: ObjectId = line[..j].parse()?;
    Ok((refname.to_owned(), oid))
}

/// Parse `packed-refs` bytes the way Git reads them for ref listing (strict).
fn parse_packed_refs_to_map(path: &Path, data: &[u8]) -> Result<HashMap<String, ObjectId>> {
    parse_packed_refs_to_map_with_prefix(path, data, None)
}

/// Whether the `packed-refs` header comment lists Git's `sorted` trait (`pack-refs --all`).
fn packed_refs_file_declares_sorted(text: &str) -> bool {
    for line in text.split('\n') {
        if line.is_empty() {
            break;
        }
        if !line.starts_with('#') {
            break;
        }
        let body = line[1..].trim();
        let Some(traits) = body.strip_prefix("pack-refs with:") else {
            continue;
        };
        return traits.split_whitespace().any(|t| t == "sorted");
    }
    false
}

fn parse_packed_refs_to_map_with_prefix(
    path: &Path,
    data: &[u8],
    prefix_filter: Option<&str>,
) -> Result<HashMap<String, ObjectId>> {
    if data.is_empty() {
        return Ok(HashMap::new());
    }
    if data.contains(&b'\r') {
        return Err(packed_refs_unexpected_line(
            path,
            "<contains carriage return>",
        ));
    }
    if !data.ends_with(b"\n") {
        let tail = std::str::from_utf8(data)
            .map(|s| s.rsplit('\n').next().unwrap_or(s))
            .unwrap_or("<invalid utf-8>");
        return Err(packed_refs_unterminated_line(path, tail));
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| packed_refs_unexpected_line(path, "<invalid utf-8>"))?;

    let ref_opts = RefNameOptions {
        allow_onelevel: false,
        refspec_pattern: false,
        normalize: false,
    };

    let mut map = HashMap::new();
    let mut expect_peel = false;
    let prefix_trim = prefix_filter.map(|p| p.trim_end_matches('/'));
    let packed_sorted = packed_refs_file_declares_sorted(text);
    let prefix_upper = if packed_sorted {
        prefix_trim.and_then(list_prefix_upper_bound)
    } else {
        None
    };
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            if lines.peek().is_some() {
                return Err(packed_refs_unexpected_line(path, line));
            }
            break;
        }
        if line.starts_with('#') {
            expect_peel = false;
            continue;
        }
        if line.starts_with('^') {
            parse_packed_ref_peel_line(path, line, expect_peel)?;
            expect_peel = false;
            continue;
        }

        if let Some(prefix) = prefix_trim {
            let Some(refname) = packed_ref_data_line_refname(line) else {
                return Err(packed_refs_unexpected_line(path, line));
            };
            if packed_sorted {
                if refname < prefix {
                    expect_peel = true;
                    continue;
                }
                if let Some(upper) = &prefix_upper {
                    if refname >= upper.as_str() {
                        break;
                    }
                }
            }
            if !ref_name_matches_list_prefix(refname, prefix) {
                expect_peel = true;
                continue;
            }
        }
        let (refname, oid) = parse_packed_ref_data_line(path, line, &ref_opts)?;
        map.insert(refname, oid);
        expect_peel = true;
    }
    Ok(map)
}

fn parse_packed_refs_to_map_with_names(
    path: &Path,
    data: &[u8],
    names: &HashSet<String>,
) -> Result<HashMap<String, ObjectId>> {
    if data.is_empty() {
        return Ok(HashMap::new());
    }
    if data.contains(&b'\r') {
        return Err(packed_refs_unexpected_line(
            path,
            "<contains carriage return>",
        ));
    }
    if !data.ends_with(b"\n") {
        let tail = std::str::from_utf8(data)
            .map(|s| s.rsplit('\n').next().unwrap_or(s))
            .unwrap_or("<invalid utf-8>");
        return Err(packed_refs_unterminated_line(path, tail));
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| packed_refs_unexpected_line(path, "<invalid utf-8>"))?;

    let ref_opts = RefNameOptions {
        allow_onelevel: false,
        refspec_pattern: false,
        normalize: false,
    };

    let mut map = HashMap::with_capacity(names.len());
    let mut expect_peel = false;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            if lines.peek().is_none() {
                break;
            }
            return Err(packed_refs_unexpected_line(path, line));
        }
        if line.starts_with('#') {
            expect_peel = false;
            continue;
        }
        if line.starts_with('^') {
            parse_packed_ref_peel_line(path, line, expect_peel)?;
            expect_peel = false;
            continue;
        }

        let Some(refname) = packed_ref_data_line_refname(line) else {
            return Err(packed_refs_unexpected_line(path, line));
        };
        if names.contains(refname) {
            let (refname, oid) = parse_packed_ref_data_line(path, line, &ref_opts)?;
            map.insert(refname, oid);
        }
        expect_peel = true;
    }
    Ok(map)
}

pub(crate) fn read_packed_refs_map(store: &Path) -> Result<HashMap<String, ObjectId>> {
    read_packed_refs_map_for_prefix(store, "")
}

pub(crate) fn read_packed_refs_map_for_names(
    store: &Path,
    names: &HashSet<String>,
) -> Result<HashMap<String, ObjectId>> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let packed_path = store::paths::packed_refs_path(store);
    let data = match fs::read(&packed_path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    parse_packed_refs_to_map_with_names(&packed_path, &data, names)
}

pub(crate) fn read_packed_refs_map_for_prefix(
    store: &Path,
    prefix: &str,
) -> Result<HashMap<String, ObjectId>> {
    let packed_path = store::paths::packed_refs_path(store);
    let data = match fs::read(&packed_path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let filter = if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    };
    parse_packed_refs_to_map_with_prefix(&packed_path, &data, filter)
}

fn acquire_packed_refs_lock(git_dir: &Path, lock: &Path, lock_display: &Path) -> Result<()> {
    let timeout_ms = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .ok()
    .and_then(|cfg| cfg.get("core.packedrefstimeout"))
    .and_then(|v| v.trim().parse::<i64>().ok())
    .unwrap_or(0);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock)
        {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if timeout_ms > 0 && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                return Err(Error::from(RefLockError::PackedRefsLockHeld {
                    lock_path: lock_display.display().to_string(),
                }));
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
}

/// Look up a refname in `packed-refs`.
pub(crate) fn lookup_packed_ref(git_dir: &Path, refname: &str) -> Result<Option<ObjectId>> {
    Ok(read_packed_refs_map(git_dir)?.get(refname).copied())
}

/// Write a symbolic ref (e.g. `NOTES_MERGE_REF` → `refs/notes/m`).
///
/// For reftable-backed repositories this dispatches to the reftable writer.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `refname` is not safe for ref storage (e.g. path
/// traversal under `refs/`), on a namespace conflict, or [`Error::Io`] on filesystem errors.
pub fn write_symbolic_ref(git_dir: &Path, refname: &str, target: &str) -> Result<()> {
    store_write_value(
        git_dir,
        refname,
        Some(StoreRawRef::Symbolic(target.to_owned())),
        Expected::Any,
        None,
    )
}

pub(crate) fn ensure_refname_safe_for_storage(refname: &str) -> Result<()> {
    if let Some(rest) = refname.strip_prefix("refs/") {
        if rest.is_empty() || !crate::check_ref_format::is_valid_fetch_advertised_ref(refname) {
            return Err(Error::InvalidRef(format!(
                "refusing to update ref with bad name '{refname}'"
            )));
        }
        return Ok(());
    }
    if crate::check_ref_format::refname_is_safe(refname) {
        Ok(())
    } else {
        Err(Error::InvalidRef(format!(
            "refusing to update ref with bad name '{refname}'"
        )))
    }
}

/// Write a ref, creating parent directories as needed.
///
/// Dispatches to the reftable backend when `extensions.refStorage = reftable`.
///
/// # Parameters
///
/// - `git_dir` — path to the git directory.
/// - `refname` — reference name (e.g. `"refs/heads/main"`).
/// - `oid` — the new target object ID.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `refname` is not safe for ref storage (e.g. path
/// traversal under `refs/`), on a namespace conflict, or [`Error::Io`] on filesystem errors.
pub fn write_ref(git_dir: &Path, refname: &str, oid: &ObjectId) -> Result<()> {
    store_write_value(
        git_dir,
        refname,
        Some(StoreRawRef::Direct(*oid)),
        Expected::Any,
        None,
    )
}

/// Per-ref update flags for [`RefUpdate`] (e.g. `REF_NO_DEREF` on `HEAD`).
#[must_use]
pub fn ref_update_flags(refname: &str) -> RefUpdateFlags {
    legacy_ref_update_flags(refname)
}

/// Create a new direct ref and its initial reflog line in one store transaction.
///
/// # Errors
///
/// Returns an error when the ref already exists, the name is invalid, or ref/reflog I/O fails.
pub fn create_ref_with_reflog(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    identity: &str,
    message: &str,
) -> Result<()> {
    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    let reflog = if should_autocreate_reflog_with_config(&config, git_dir, refname) {
        Some(ReflogUpdate {
            identity: identity.to_owned(),
            message: message.to_owned(),
            time: time::OffsetDateTime::now_utc(),
        })
    } else {
        None
    };
    store_write_value(
        git_dir,
        refname,
        Some(StoreRawRef::Direct(*oid)),
        Expected::Missing,
        reflog,
    )
}

/// Write a loose ref file under `storage_dir`: the body shared by [`write_ref`]
/// and [`write_ref_cached`], run after the packed-refs namespace-conflict check.
fn write_ref_at_storage(storage_dir: &Path, refname: &str, oid: &ObjectId) -> Result<()> {
    let stor = crate::ref_namespace::storage_ref_name(refname);
    let path = storage_dir.join(stor);
    // An empty directory left over from a previously deleted nested ref can sit exactly where
    // this ref file must go (e.g. `refs/e-create/foo` after `refs/e-create/foo/bar` was pruned).
    // Git removes such empty directory trees before locking the ref (see t0600 "empty directory
    // should not fool create/update"); mirror that so the rename below does not hit "Is a
    // directory". Non-empty directories (containing files or `*.lock`) are left in place so the
    // subsequent write surfaces the conflict, matching Git's behaviour.
    remove_empty_ref_directory(&path);
    // If a *non-empty* directory still sits where the ref file must go, locking cannot
    // proceed. Git reports this as a "non-empty directory ... blocking reference" rather
    // than letting the rename fail with a raw "Is a directory" I/O error (see t0600
    // "non-empty directory blocks create"). Mirror that message so callers display it
    // verbatim. This only applies to direct (non-deref) writes where `refname` is also the
    // user-visible ref; indirect symref writes are handled by the caller.
    if fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
    {
        let display = ref_path_for_display(&path);
        return Err(crate::error::RefLockError::DirectoryInTheWay {
            refname: refname.to_owned(),
            path: display,
        }
        .into());
    }
    if path.is_file() && matches!(read_ref_file(&path), Err(Error::InvalidRef(_))) {
        return Err(Error::InvalidRef(format!(
            "cannot update ref '{refname}': reference broken"
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = format!("{oid}\n");
    // Write via lock file for atomicity. Use `O_CREAT | O_EXCL` (create_new) so
    // that a pre-existing `<ref>.lock` surfaces an `AlreadyExists` error instead
    // of being silently clobbered, matching Git's `lock_ref_oid_basic`. Callers
    // (e.g. fetch) translate that into the
    // "cannot lock ref '<ref>': Unable to create '<lock>': File exists." message
    // and skip just that ref while still updating the rest.
    let lock = lock_path_for_ref(&path);
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)?;
        file.write_all(content.as_bytes())?;
    }
    fs::rename(&lock, &path)?;
    Ok(())
}

/// Like [`write_ref`], but requires `expected_old` to match the ref's current value
/// while the loose-ref lock is held (atomic CAS for concurrent updaters).
///
/// # Errors
///
/// Same as [`write_ref`], plus [`Error::Message`] when the CAS check fails under lock.
pub fn write_ref_cas(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    expected_old: ObjectId,
) -> Result<()> {
    store_write_value(
        git_dir,
        refname,
        Some(StoreRawRef::Direct(*oid)),
        Expected::Oid(expected_old),
        None,
    )
}

/// Like [`delete_ref`], but requires `expected_old` to match under the ref lock.
///
/// # Errors
///
/// Same as [`delete_ref`], plus [`Error::Message`] when the CAS check fails.
pub fn delete_ref_cas(git_dir: &Path, refname: &str, expected_old: ObjectId) -> Result<()> {
    store_write_value(git_dir, refname, None, Expected::Oid(expected_old), None)
}

/// Unlink a loose ref file after `packed-refs` was updated, when the file still contains
/// `expected`. Does not modify `packed-refs` (unlike [`delete_ref`] / [`delete_ref_cas`]).
///
/// Returns `Ok(true)` when the loose file was removed, `Ok(false)` when the OID differed or the
/// file was already absent.
///
/// # Errors
///
/// Propagates I/O failures and when the ref lock is already held.
pub(crate) fn prune_loose_ref_cas(
    git_dir: &Path,
    refname: &str,
    expected: ObjectId,
) -> Result<bool> {
    ensure_refname_safe_for_storage(refname)?;
    let storage_dir = ref_storage_dir(git_dir, refname);
    let stor = crate::ref_namespace::storage_ref_name(refname);
    let path = storage_dir.join(&stor);
    let lock = lock_path_for_ref(&path);
    if fs::symlink_metadata(&lock).is_ok() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("ref lock present: {}", lock.display()),
        )));
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .map_err(Error::Io)?;
    let outcome = (|| -> Result<bool> {
        if !path.is_file() {
            return Ok(true);
        }
        let Ok(Ref::Direct(oid)) = read_ref_file(&path) else {
            return Ok(false);
        };
        if oid != expected {
            return Ok(false);
        }
        remove_empty_ref_directory(&path);
        fs::remove_file(&path).map_err(Error::Io)?;
        if let Some(parent) = path.parent() {
            remove_empty_ref_directory(parent);
        }
        Ok(true)
    })();
    let _ = fs::remove_file(&lock);
    outcome
}

/// A one-time, in-memory snapshot of a ref store's `packed-refs` file.
///
/// Bulk ref operations — notably `fetch`'s apply phase — resolve and write
/// thousands of refs back-to-back. Going through [`resolve_ref`] / [`write_ref`]
/// for each one re-reads and re-scans the *entire* `packed-refs` file on every
/// call, making the apply O(refs × packed-refs size). On a repository with tens
/// of thousands of packed refs that dominates the whole fetch even when no
/// objects are transferred. Loading the file once into this snapshot and using
/// [`resolve_ref_cached`] / [`write_ref_cached`] makes the apply linear.
///
/// The snapshot is read-only and reflects `packed-refs` at load time. Loose-ref
/// writes performed during the same apply do not invalidate it: each ref's
/// previous value is independent, so a stale snapshot only ever omits a ref that
/// was loose (and thus resolved from disk, not the snapshot) to begin with.
pub struct PackedRefs {
    map: HashMap<String, ObjectId>,
}

impl PackedRefs {
    /// Load the `packed-refs` snapshot for the ref store backing `git_dir` (the
    /// shared common dir for linked worktrees). A missing file yields an empty
    /// snapshot; reftable repositories also yield an empty snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `packed-refs` exists but cannot be read, or
    /// [`Error::PackedRefsUnexpectedLine`] when the file contains lines Git would reject.
    pub fn load(git_dir: &Path) -> Result<Self> {
        let store = common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
        let map = read_packed_refs_map(&store)?;
        Ok(Self { map })
    }

    /// Validate the full `packed-refs` file but retain only entries listed in `names`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::load`].
    #[allow(dead_code)]
    pub(crate) fn load_for_ref_names(git_dir: &Path, names: &HashSet<String>) -> Result<Self> {
        if names.is_empty() {
            return Ok(Self {
                map: HashMap::new(),
            });
        }
        let store = common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
        let packed_path = store::paths::packed_refs_path(&store);
        let data = match fs::read(&packed_path) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    map: HashMap::new(),
                })
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let map = parse_packed_refs_to_map_with_names(&packed_path, &data, names)?;
        Ok(Self { map })
    }

    /// The packed value recorded for `refname`, if any.
    #[must_use]
    pub fn get(&self, refname: &str) -> Option<ObjectId> {
        self.map.get(refname).copied()
    }

    /// Whether any packed ref name conflicts with `refname` as a path prefix
    /// (e.g. an existing `refs/heads/x` blocks creating `refs/heads/x/y`).
    /// Mirrors the internal `packed_ref_namespace_conflict` helper against the snapshot.
    #[must_use]
    pub fn has_namespace_conflict(&self, refname: &str) -> bool {
        self.map
            .keys()
            .any(|existing| refname_namespace_conflicts(existing, refname))
    }
}

/// Resolve `refname` to its current object id using a preloaded [`PackedRefs`]
/// snapshot instead of re-reading `packed-refs`. Loose refs win over packed
/// (matching [`resolve_ref`]); a symbolic loose ref falls back to the full
/// [`resolve_ref`] resolver (rare for the tracking/tag refs this is used for).
/// Returns `Ok(None)` when the ref does not exist.
///
/// # Errors
///
/// Returns an error on ref-file I/O failures other than "not found".
pub fn resolve_ref_cached(
    git_dir: &Path,
    refname: &str,
    packed: &PackedRefs,
) -> Result<Option<ObjectId>> {
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return Ok(resolve_ref(git_dir, refname).ok());
    }

    let (store, stor_name) = crate::worktree_ref::resolve_ref_storage(git_dir, refname);
    let storage_owned = crate::ref_namespace::storage_ref_name(&stor_name);
    let try_names: Vec<&str> = if storage_owned != stor_name {
        vec![storage_owned.as_str(), stor_name.as_str()]
    } else {
        vec![stor_name.as_str()]
    };

    for name in &try_names {
        let path = store.join(name);
        match read_ref_file(&path) {
            Ok(Ref::Direct(oid)) => return Ok(Some(oid)),
            Ok(Ref::Symbolic(target)) => return Ok(resolve_ref(git_dir, &target).ok()),
            Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    for name in &try_names {
        if let Some(oid) = packed.get(name) {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

/// Like [`write_ref`], but checks the packed-refs namespace conflict against a
/// preloaded [`PackedRefs`] snapshot instead of re-reading `packed-refs` on
/// every call. Behaves identically to [`write_ref`] otherwise.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `refname` is not safe for ref storage, on a namespace
/// conflict, or [`Error::Io`] / [`Error::Message`] on filesystem errors.
pub fn write_ref_cached(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    packed: &PackedRefs,
) -> Result<()> {
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return write_ref(git_dir, refname, oid);
    }
    ensure_refname_safe_for_storage(refname)?;
    if packed.has_namespace_conflict(refname) {
        return Err(Error::InvalidRef(format!(
            "cannot update ref '{refname}': reference namespace conflict"
        )));
    }
    let storage_dir = ref_storage_dir(git_dir, refname);
    write_ref_at_storage(&storage_dir, refname, oid)
}

/// Render a ref-store path for user-facing diagnostics the way Git does: relative to the
/// current working directory when possible (e.g. `.git/refs/foo/bar`), otherwise the full
/// path. Git builds these paths relative to the worktree root, so under the normal in-tree
/// case this yields the `.git/...` form that the upstream tests expect.
fn ref_path_for_display(path: &Path) -> String {
    let cwd = crate::environment::Environment::empty().cwd.clone();
    if let Ok(rel) = path.strip_prefix(&cwd) {
        return rel.to_string_lossy().into_owned();
    }
    // On platforms where the cwd and the (canonicalized) ref path disagree only by a
    // symlinked prefix (e.g. macOS `/tmp` -> `/private/tmp`), retry with both sides
    // canonicalized so the relative form is still recovered.
    if let (Ok(cwd_c), Ok(path_c)) = (cwd.canonicalize(), path.canonicalize()) {
        if let Ok(rel) = path_c.strip_prefix(&cwd_c) {
            return rel.to_string_lossy().into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

/// Remove the directory at `path` if it is an empty directory tree (contains only empty
/// directories, no regular files or lock files). Best-effort: returns silently on any error or
/// if `path` is not a directory. Mirrors Git's `remove_empty_directories`
/// (`remove_dir_recursively(REMOVE_DIR_EMPTY_ONLY)`), used to clear stale directories that sit
/// where a ref file must be created or deleted.
pub(crate) fn remove_empty_ref_directory(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {}
        _ => return,
    }
    // `remove_dir_all` would also delete files; we only want to remove trees that are entirely
    // empty of files. `fs::remove_dir` fails (non-empty) unless we recurse, so walk first.
    if dir_tree_has_files(path) {
        return;
    }
    let _ = remove_dir_tree(path);
}

/// Return true if the directory tree rooted at `dir` contains any non-directory entry.
fn dir_tree_has_files(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        // Unreadable: treat conservatively as "has files" so we do not delete it.
        return true;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {
                if dir_tree_has_files(&entry.path()) {
                    return true;
                }
            }
            Ok(_) => return true, // a file, symlink, or `*.lock`
            Err(_) => return true,
        }
    }
    false
}

/// Walk upward from `ref_path`'s parent, removing empty directories until `refs/`.
pub(crate) fn prune_empty_loose_ref_parents(storage_dir: &Path, ref_path: &Path) {
    let refs_root = storage_dir.join("refs");
    let mut current = ref_path.parent();
    while let Some(dir) = current {
        if dir == refs_root.as_path() || !dir.starts_with(&refs_root) {
            break;
        }
        if dir_tree_has_files(dir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

/// Recursively remove an empty directory tree (assumes [`dir_tree_has_files`] returned false).
fn remove_dir_tree(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_dir_tree(&entry.path())?;
        }
    }
    fs::remove_dir(dir)
}

/// Delete a ref.
///
/// Dispatches to the reftable backend when `extensions.refStorage = reftable`.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `refname` is not safe for ref storage, or
/// [`Error::Io`] for errors other than "not found".
pub fn delete_ref(git_dir: &Path, refname: &str) -> Result<()> {
    if matches!(read_raw_ref(git_dir, refname)?, RawRefLookup::NotFound) {
        return Err(Error::InvalidRef(format!("ref not found: {refname}")));
    }
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        let store = open_ref_store(git_dir)?;
        let mut flags = legacy_ref_update_flags(refname);
        if refname != "HEAD" {
            if let Ok(Some(StoreRawRef::Symbolic(_))) = store.read_raw(refname) {
                flags.no_deref = true;
            }
        }
        return commit_store_update(
            git_dir,
            RefUpdate {
                name: refname.to_owned(),
                new_value: None,
                expected: Expected::Exists,
                reflog: None,
                flags,
            },
        )
        .map_err(|e| map_public_ref_store_error(e, refname));
    }
    ensure_refname_safe_for_storage(refname)?;
    let storage_dir = ref_storage_dir(git_dir, refname);
    let stor = crate::ref_namespace::storage_ref_name(refname);
    let path = storage_dir.join(&stor);
    let lock = lock_path_for_ref(&path);
    if fs::symlink_metadata(&lock).is_ok() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("ref lock present: {}", lock.display()),
        )));
    }

    // Remove the packed-refs entry *first* (acquiring the packed-refs lock). Git deletes the
    // packed version while holding the lock before unlinking the loose ref, so that a failure to
    // rewrite packed-refs (lock held, stale `packed-refs.new`, ...) leaves the reference fully
    // intact rather than dropping the loose file and exposing a stale packed value. See t0600
    // "delete fails cleanly if packed-refs file is locked / .new write fails".
    remove_packed_ref(&storage_dir, &stor)?;

    // Remove the loose ref file. An empty directory tree may occupy the ref path (a leftover
    // from a previously deleted nested ref); clear it so the delete succeeds rather than failing
    // with "Is a directory" (see t0600 "empty directory should not fool 0/1-arg delete").
    remove_empty_ref_directory(&path);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e)
            if e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::ENOTDIR) => {}
        // The path is (still) a directory — e.g. a non-empty tree we declined to remove.
        // Treat as "loose ref not present"; the packed-refs entry was already removed above.
        Err(e)
            if e.raw_os_error() == Some(libc::EISDIR) || e.raw_os_error() == Some(libc::EPERM) => {}
        Err(e) => return Err(Error::Io(e)),
    }

    prune_empty_loose_ref_parents(&storage_dir, &path);

    let log_path = storage_dir.join("logs").join(&stor);

    // Remove the ref's reflog and clean up empty parent directories, matching Git's
    // `files_transaction_finish` (which deletes the reflog of any deleted ref and then calls
    // `try_remove_empty_parents`). Leaving the reflog behind would block a later nested ref
    // (e.g. deleting `k/l` then creating `k/l/m`, which needs `logs/refs/heads/k/l` to be a
    // directory) — see t0601 and t1410 "stale dirs do not cause d/f conflicts".
    let _ = fs::remove_file(&log_path);

    let logs_root = storage_dir.join("logs");
    let mut parent = log_path.parent();
    while let Some(p) = parent {
        if p == logs_root.as_path() || !p.starts_with(&logs_root) {
            break;
        }
        if fs::remove_dir(p).is_err() {
            break; // not empty or other error
        }
        parent = p.parent();
    }

    Ok(())
}

/// Remove a single entry from the packed-refs file, rewriting it.
pub(crate) fn remove_packed_ref(git_dir: &Path, refname: &str) -> Result<()> {
    remove_packed_ref_inner(git_dir, refname, false)
}

pub(crate) fn remove_packed_ref_under_lock(git_dir: &Path, refname: &str) -> Result<()> {
    remove_packed_ref_inner(git_dir, refname, true)
}

fn remove_packed_ref_inner(git_dir: &Path, refname: &str, lock_already_held: bool) -> Result<()> {
    let packed_path = store::paths::packed_refs_path(git_dir);
    let content = match fs::read(&packed_path) {
        Ok(c) => c,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            return Ok(());
        }
        Err(e) => return Err(Error::Io(e)),
    };
    // Fail before mutating anything if the on-disk file is not readable like Git's.
    parse_packed_refs_to_map(&packed_path, &content)?;

    let content = String::from_utf8(content)
        .map_err(|_| packed_refs_unexpected_line(&packed_path, "<invalid utf-8>"))?;

    let mut out = String::new();
    let mut skip_peeled = false;
    let mut changed = false;
    // Write a fresh header (don't preserve old comment lines — real git
    // regenerates the header on every rewrite).
    let mut header_written = false;

    for line in content.lines() {
        if skip_peeled {
            if line.starts_with('^') {
                changed = true;
                continue;
            }
            skip_peeled = false;
        }

        if line.starts_with('#') {
            // Skip old header lines — we'll write a fresh one
            continue;
        }
        if line.starts_with('^') {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Write fresh header before the first data line
        if !header_written {
            out.insert_str(0, "# pack-refs with: peeled fully-peeled sorted\n");
            header_written = true;
        }

        // Check if this line matches the ref to remove
        let mut parts = line.splitn(2, ' ');
        let _hash = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("").trim();
        if name == refname {
            changed = true;
            skip_peeled = true;
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    if changed {
        // Git rewrites packed-refs under two files: it first takes the `packed-refs.lock`
        // lockfile (failing if another process holds it), then writes the new content to the
        // `packed-refs.new` tempfile and renames it over `packed-refs`. Mirror both so that
        // `update-ref -d` fails cleanly — leaving the reference intact — when either file is
        // already present (t0600 "delete fails cleanly if packed-refs file is locked / .new
        // write fails").
        let lock = lock_path_for_ref(&packed_path); // packed-refs.lock
        if !lock_already_held {
            let abs_lock = fs::canonicalize(git_dir)
                .map(|d| d.join("packed-refs.lock"))
                .unwrap_or_else(|_| lock.clone());
            acquire_packed_refs_lock(git_dir, &lock, &abs_lock)?;
        }

        let tmp = packed_path.with_extension("new");
        let mut created_tmp = false;
        let write_result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(Error::Io)?;
            created_tmp = true;
            use std::io::Write as _;
            file.write_all(out.as_bytes()).map_err(Error::Io)?;
            drop(file);
            fs::rename(&tmp, &packed_path).map_err(Error::Io)?;
            created_tmp = false; // consumed by rename
            Ok(())
        })();

        // Release the lock unless the caller is holding it through a transaction.
        if !lock_already_held {
            let _ = fs::remove_file(&lock);
        }
        if write_result.is_err() && created_tmp {
            let _ = fs::remove_file(&tmp);
        }
        write_result?;
    }

    Ok(())
}

/// Read the symbolic ref target of `HEAD`.
///
/// Returns `None` if HEAD is detached (points directly to a commit hash).
///
/// # Errors
///
/// Returns [`Error::Io`] or [`Error::InvalidRef`] on failures.
pub fn read_head(git_dir: &Path) -> Result<Option<String>> {
    match read_ref_file(&git_dir.join("HEAD"))? {
        Ref::Symbolic(target) => Ok(Some(target)),
        Ref::Direct(_) => Ok(None),
    }
}

/// Read symbolic target of any ref.
///
/// Dispatches to the reftable backend when `extensions.refStorage = reftable`.
///
/// Returns `Ok(Some(target))` when `refname` exists and is symbolic,
/// `Ok(None)` when it is direct or missing.
pub fn read_symbolic_ref(git_dir: &Path, refname: &str) -> Result<Option<String>> {
    match open_ref_store(git_dir)?.read_raw(refname)? {
        Some(StoreRawRef::Symbolic(target)) => Ok(Some(target)),
        Some(StoreRawRef::Direct(_)) | None => Ok(None),
    }
}

/// Core `logAllRefUpdates` modes (after config lookup), matching Git's `log_refs_config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogRefsConfig {
    /// `core.logAllRefUpdates` not set; resolved per-repo (bare vs non-bare).
    Unset,
    /// Explicitly disabled.
    None,
    /// `true` — log branch-like refs only (see [`should_autocreate_reflog`]).
    Normal,
    /// `always` — log updates to any ref.
    Always,
}

/// Read `[core] logAllRefUpdates` from the repository config.
///
/// Returns [`LogRefsConfig::Unset`] when the key is absent.
pub fn read_log_refs_config(git_dir: &Path) -> LogRefsConfig {
    ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .map(|cfg| log_refs_config_from_set(&cfg))
    .unwrap_or(LogRefsConfig::Unset)
}

fn log_refs_config_from_set(cfg: &ConfigSet) -> LogRefsConfig {
    let Some(v) = cfg.get("core.logAllRefUpdates") else {
        return LogRefsConfig::Unset;
    };
    let lower = v.trim().to_ascii_lowercase();
    match lower.as_str() {
        "always" => LogRefsConfig::Always,
        "1" | "true" | "yes" | "on" => LogRefsConfig::Normal,
        "0" | "false" | "no" | "off" | "never" => LogRefsConfig::None,
        _ => LogRefsConfig::Unset,
    }
}

/// Effective `logAllRefUpdates` after applying Git's `LOG_REFS_UNSET` rule.
pub fn effective_log_refs_config(git_dir: &Path) -> LogRefsConfig {
    ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .map(|cfg| cfg.effective_log_refs_config(git_dir))
    .unwrap_or(LogRefsConfig::Normal)
}

/// Whether a new reflog file may be auto-created for `refname` given an already-resolved
/// `core.logAllRefUpdates` mode (including command-line config).
#[must_use]
pub fn should_autocreate_reflog_for_mode(refname: &str, mode: LogRefsConfig) -> bool {
    match mode {
        LogRefsConfig::Always => true,
        LogRefsConfig::Normal => {
            refname == "HEAD"
                || refname.starts_with("refs/heads/")
                || refname.starts_with("refs/remotes/")
                || refname.starts_with("refs/notes/")
        }
        LogRefsConfig::None | LogRefsConfig::Unset => false,
    }
}

/// Whether a new reflog file may be auto-created for `refname` (Git `should_autocreate_reflog`).
#[must_use]
pub fn should_autocreate_reflog(git_dir: &Path, refname: &str) -> bool {
    let cfg = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    should_autocreate_reflog_with_config(&cfg, git_dir, refname)
}

/// Like [`should_autocreate_reflog`], using an already-loaded repository config snapshot.
#[must_use]
pub fn should_autocreate_reflog_with_config(
    config: &ConfigSet,
    git_dir: &Path,
    refname: &str,
) -> bool {
    should_autocreate_reflog_for_mode(refname, config.effective_log_refs_config(git_dir))
}

/// Write a reflog entry.
///
/// Dispatches to the reftable backend when `extensions.refStorage = reftable`.
///
/// # Parameters
///
/// - `git_dir` — path to the git directory.
/// - `refname` — reference name (e.g. `"refs/heads/main"`).
/// - `old_oid` — previous OID (use `ObjectId::from_bytes(&[0;20])` for a new ref).
/// - `new_oid` — new OID.
/// - `identity` — `"Name <email> <timestamp> <tz>"` formatted string.
/// - `message` — short log message.
/// - `force_create` — if true, create the log file even when [`should_autocreate_reflog`] would not.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem errors.
/// Remove stale reflog *files* that occupy path components which must become directories.
///
/// When a branch is deleted we keep its reflog file (so `branch -D` + later recreate can
/// retain history). If a later nested branch (e.g. `k/l/m`) needs `logs/refs/heads/k/l` to be
/// a directory, the leftover file at that path blocks `create_dir_all`. Walk from `logs_root`
/// down toward `target` and remove any regular file sitting where a directory is required, so
/// the directory can be created. Best-effort: filesystem errors are ignored (the subsequent
/// `create_dir_all` surfaces any real problem).
fn clear_conflicting_reflog_files(logs_root: &Path, target: &Path) {
    let Ok(rel) = target.strip_prefix(logs_root) else {
        return;
    };
    let mut cur = logs_root.to_path_buf();
    for component in rel.components() {
        cur.push(component);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                // A non-directory (stale reflog file or symlink) is in the way of a needed
                // directory. Remove it so the directory hierarchy can be created.
                let _ = fs::remove_file(&cur);
            }
            Err(_) => break, // does not exist yet (or unreadable); nothing more to clear
        }
    }
}

/// Render a reflog entry's `(old, new)` OIDs as hex, widening a null OID to the
/// width of the non-null side so the line matches the repository hash algorithm.
pub(crate) fn reflog_oid_hex_pair(old: &ObjectId, new: &ObjectId) -> (String, String) {
    let width = if !old.is_zero() {
        old.algo().hex_len()
    } else if !new.is_zero() {
        new.algo().hex_len()
    } else {
        old.algo().hex_len()
    };
    let render = |oid: &ObjectId| {
        if oid.is_zero() {
            "0".repeat(width)
        } else {
            oid.to_hex()
        }
    };
    (render(old), render(new))
}

/// Append one line to the reflog for `refname`.
///
/// Creates the reflog file when `force_create` is true or when the ref already
/// has a reflog. Identity and message are written in Git's standard reflog format.
///
/// # Errors
///
/// Propagates I/O failures and invalid refname errors.
pub fn append_reflog(
    git_dir: &Path,
    refname: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    identity: &str,
    message: &str,
    force_create: bool,
) -> Result<()> {
    append_reflog_with_config(
        git_dir,
        refname,
        old_oid,
        new_oid,
        identity,
        message,
        force_create,
        None,
    )
}

/// Like [`append_reflog`], reusing `config` for reflog auto-create policy when provided.
#[allow(clippy::too_many_arguments)]
pub fn append_reflog_with_config(
    git_dir: &Path,
    refname: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    identity: &str,
    message: &str,
    force_create: bool,
    config: Option<&ConfigSet>,
) -> Result<()> {
    test_inject_reflog_fail(refname)?;
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return crate::reftable::reftable_append_reflog(
            git_dir,
            refname,
            old_oid,
            new_oid,
            identity,
            message,
            force_create,
        );
    }
    let storage_dir = ref_storage_dir(git_dir, refname);
    let stor = crate::ref_namespace::storage_ref_name(refname);
    let log_path = storage_dir.join("logs").join(&stor);
    let may_create = force_create
        || match config {
            Some(cfg) => should_autocreate_reflog_with_config(cfg, git_dir, refname),
            None => should_autocreate_reflog(git_dir, refname),
        };
    if !may_create && !log_path.exists() {
        return Ok(());
    }
    if let Some(parent) = log_path.parent() {
        // A stale reflog *file* left behind by a deleted branch (e.g. `logs/refs/heads/k/l`)
        // can occupy a path component that must now become a directory (for `k/l/m`). Git
        // removes such leftovers while creating the reflog; mirror that so `create_dir_all`
        // does not fail with "File exists" / "Not a directory".
        let logs_root = storage_dir.join("logs");
        clear_conflicting_reflog_files(&logs_root, parent);
        fs::create_dir_all(parent)?;
    }
    // A reflog records `<old> <new>` at the repository's hash width. The null
    // ("0000…") side of a ref creation/deletion is stored at SHA-1 width by
    // callers; widen it to match the non-null side so sha256 reflogs are
    // internally consistent (and re-parseable).
    let (old_hex, new_hex) = reflog_oid_hex_pair(old_oid, new_oid);
    let line = if message.is_empty() {
        format!("{old_hex} {new_hex} {identity}\n")
    } else {
        format!("{old_hex} {new_hex} {identity}\t{message}\n")
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    use io::Write;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn pack_ref_include_pattern(all: bool) -> &'static [u8] {
    if all {
        b"*"
    } else {
        b"refs/tags/*"
    }
}

fn ref_matches_pack_include(refname: &str, all: bool) -> bool {
    crate::wildmatch::wildmatch(pack_ref_include_pattern(all), refname.as_bytes(), 0)
}

fn collect_loose_ref_files(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            return Ok(());
        }
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let refname = format!("{prefix}{name}");
        let path = entry.path();
        if path.is_dir() {
            collect_loose_ref_files(&path, &format!("{refname}/"), out)?;
        } else if path.is_file() {
            out.push((refname, path));
        }
    }
    Ok(())
}

fn load_packed_refs_with_peel(
    packed_path: &Path,
    merged: &mut HashMap<String, (ObjectId, Option<ObjectId>)>,
) -> Result<()> {
    if !packed_path.is_file() {
        return Ok(());
    }
    let data = fs::read(packed_path)?;
    parse_packed_refs_to_map(packed_path, &data)?;
    let content = String::from_utf8(data)
        .map_err(|_| packed_refs_unexpected_line(packed_path, "<invalid utf-8>"))?;
    let mut last_name: Option<String> = None;
    for line in content.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(hex) = line.strip_prefix('^') {
            let Ok(peel_oid) = ObjectId::from_hex(hex.trim()) else {
                continue;
            };
            if let Some(name) = &last_name {
                if let Some(entry) = merged.get_mut(name) {
                    entry.1 = Some(peel_oid);
                }
            }
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let Some(hex) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        let name = name.trim();
        let Ok(oid) = ObjectId::from_hex(hex) else {
            continue;
        };
        merged.insert(name.to_owned(), (oid, None));
        last_name = Some(name.to_owned());
    }
    Ok(())
}

/// Pack loose refs for `repo` (files `packed-refs` or reftable compaction).
pub(crate) fn pack_refs_for_repository(
    repo: &crate::repo::Repository,
    all: bool,
) -> Result<(PathBuf, usize, usize)> {
    let git_dir = repo.git_dir();
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return pack_refs_reftable(git_dir);
    }
    pack_refs_files(git_dir, repo.odb(), all)
}

fn pack_refs_reftable(git_dir: &Path) -> Result<(PathBuf, usize, usize)> {
    use crate::reftable::ReftableStack;
    let store_git_dir = common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    let rt_dir = store_git_dir.join("reftable");
    let tables_before = fs::read_to_string(rt_dir.join("tables.list"))
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    let mut stack = ReftableStack::open(&store_git_dir)?;
    stack.compact()?;
    let tables_after = fs::read_to_string(rt_dir.join("tables.list"))
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    Ok((rt_dir, 0, tables_before.saturating_sub(tables_after)))
}

fn pack_refs_files(
    git_dir: &Path,
    odb: &crate::odb::Odb,
    all: bool,
) -> Result<(PathBuf, usize, usize)> {
    let storage_git_dir = common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    let packed_path = store::paths::packed_refs_path(&storage_git_dir);

    let mut merged: HashMap<String, (ObjectId, Option<ObjectId>)> = HashMap::new();
    load_packed_refs_with_peel(&packed_path, &mut merged)?;

    let mut loose_files = Vec::new();
    collect_loose_ref_files(&storage_git_dir.join("refs"), "refs/", &mut loose_files)?;

    let mut packed_loose = 0usize;
    let mut to_prune: Vec<(String, ObjectId)> = Vec::new();

    for (refname, path) in loose_files {
        if crate::worktree_ref::is_per_worktree_ref(&refname) {
            continue;
        }
        if !ref_matches_pack_include(&refname, all) {
            continue;
        }
        let Ok(Ref::Direct(oid)) = read_ref_file(&path) else {
            continue;
        };
        if odb.read(&oid).is_err() {
            continue;
        }
        let peel = if refname.starts_with("refs/tags/") {
            crate::commit_graph_write::peel_fully_for_packed_tag(odb, oid)
        } else {
            None
        };
        merged.insert(refname.clone(), (oid, peel));
        to_prune.push((refname, oid));
        let _ = path;
        packed_loose += 1;
    }

    if packed_loose == 0 && merged.is_empty() {
        return Ok((packed_path, 0, 0));
    }

    let mut names: Vec<String> = merged.keys().cloned().collect();
    names.sort();

    let mut body = String::from("# pack-refs with: peeled fully-peeled sorted\n");
    for name in &names {
        let Some((oid, peel)) = merged.get(name) else {
            continue;
        };
        body.push_str(&oid.to_hex());
        body.push(' ');
        body.push_str(name);
        body.push('\n');
        if let Some(p) = peel {
            body.push('^');
            body.push_str(&p.to_hex());
            body.push('\n');
        }
    }

    atomic_rewrite_packed_refs(&storage_git_dir, &body)?;

    #[cfg(test)]
    pack_refs_test_hooks::run_before_prune();

    let mut pruned = 0usize;
    for (refname, expected) in &to_prune {
        match prune_loose_ref_cas(git_dir, refname, *expected) {
            Ok(true) => pruned += 1,
            Ok(false) => {
                remove_packed_ref(git_dir, refname)?;
            }
            Err(e) => return Err(e),
        }
    }

    Ok((packed_path, packed_loose, pruned))
}

/// Test hook: runs once after `packed-refs` is rewritten and before loose refs are pruned.
#[cfg(test)]
mod pack_refs_test_hooks {
    use std::cell::RefCell;

    thread_local! {
        static BEFORE_PRUNE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    }

    pub fn set_before_prune(f: Option<Box<dyn Fn()>>) {
        BEFORE_PRUNE.with(|c| *c.borrow_mut() = f);
    }

    pub fn run_before_prune() {
        BEFORE_PRUNE.with(|c| {
            if let Some(f) = c.borrow_mut().take() {
                f();
            }
        });
    }
}

/// Test hook: when set to a ref name, the next [`append_reflog`] for that ref fails.
///
/// Only active in debug builds; used by regression tests for commit ref/reflog atomicity.
#[cfg(test)]
mod reflog_fail_inject {
    use std::cell::RefCell;

    // hygiene: test-only reflog failure injection hook
    thread_local! {
        pub static INJECT: RefCell<Option<String>> = const { RefCell::new(None) };
    }
}

/// Test hook: fail the next reflog append for `refname` (or any ref when `None`).
#[cfg(test)]
pub fn set_test_inject_reflog_fail(refname: Option<&str>) {
    reflog_fail_inject::INJECT.with(|c| {
        *c.borrow_mut() = refname.map(str::to_owned);
    });
}

#[cfg(test)]
fn test_inject_reflog_fail(refname: &str) -> Result<()> {
    reflog_fail_inject::INJECT.with(|c| {
        if c.borrow().as_deref() == Some(refname) {
            return Err(Error::Io(std::io::Error::other("injected reflog failure")));
        }
        Ok(())
    })
}

#[cfg(not(test))]
fn test_inject_reflog_fail(_refname: &str) -> Result<()> {
    Ok(())
}

/// Move a checked-out branch to `new_oid` and append matching branch and `HEAD` reflogs.
///
/// Verifies `HEAD` is a symbolic ref to `branch_ref`, then runs a storage transaction:
/// on the files backend the branch ref lock is held from CAS through reflog writes and
/// ref commit; on reftable, branch and `HEAD` logs are written in one transaction.
/// Reflog failures before the ref commit do not advance the branch tip.
///
/// # Errors
///
/// Returns [`Error::Message`] when `HEAD` does not point at `branch_ref`, when the
/// CAS check fails, or when ref/reflog I/O fails.
pub struct BranchCommitRefUpdate<'a> {
    /// Branch ref to update (e.g. `refs/heads/main`).
    pub branch_ref: &'a str,
    /// Expected current tip; `None` when the branch must not exist yet (unborn).
    pub expected_old: Option<ObjectId>,
    /// New commit OID for the branch tip.
    pub new_oid: ObjectId,
    /// Committer identity for reflog lines.
    pub identity: &'a str,
    /// Reflog message (without trailing newline).
    pub reflog_message: &'a str,
}

/// Compare expected and current branch tips for compare-and-swap ref updates.
///
/// # Errors
///
/// Returns an error when `current` does not match `expected`.
pub fn verify_branch_commit_cas(
    expected: Option<ObjectId>,
    current: Option<ObjectId>,
    refname: &str,
) -> Result<()> {
    verify_branch_cas(expected, current, refname)
}

fn verify_branch_cas(
    expected: Option<ObjectId>,
    current: Option<ObjectId>,
    refname: &str,
) -> Result<()> {
    match (expected, current) {
        (Some(exp), Some(cur)) if exp == cur => Ok(()),
        (None, None) => Ok(()),
        (Some(exp), None) => Err(Error::Message(format!(
            "ref transaction rejected: '{refname}' expected {} but found <absent>",
            exp.to_hex()
        ))),
        (None, Some(cur)) => Err(Error::Message(format!(
            "ref transaction rejected: '{refname}' expected <absent> but found {}",
            cur.to_hex()
        ))),
        (Some(exp), Some(cur)) => Err(Error::Message(format!(
            "ref transaction rejected: '{refname}' expected {} but found {}",
            exp.to_hex(),
            cur.to_hex()
        ))),
    }
}

fn read_loose_or_packed_oid(git_dir: &Path, refname: &str) -> Result<Option<ObjectId>> {
    let (store, stor_name) = crate::worktree_ref::resolve_ref_storage(git_dir, refname);
    let storage_owned = crate::ref_namespace::storage_ref_name(&stor_name);
    let try_names: Vec<&str> = if storage_owned != stor_name {
        vec![storage_owned.as_str(), stor_name.as_str()]
    } else {
        vec![stor_name.as_str()]
    };
    for name in try_names {
        let path = store.join(name);
        if let Ok(Ref::Direct(oid)) = read_ref_file(&path) {
            return Ok(Some(oid));
        }
        if let Some(oid) = lookup_packed_ref(&store, name)? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn abort_loose_ref_lock(lock: &Path) {
    let _ = fs::remove_file(lock);
}

#[cfg(test)]
mod branch_ref_pause_inject {
    use std::sync::{Condvar, Mutex, OnceLock};

    pub struct PauseGate {
        pub wait: Mutex<bool>,
        pub cv: Condvar,
    }

    // hygiene: test-only branch-ref commit pause injection
    static PAUSE_BEFORE_COMMIT: OnceLock<Mutex<Option<std::sync::Arc<PauseGate>>>> =
        OnceLock::new();

    pub fn pause_slot() -> &'static Mutex<Option<std::sync::Arc<PauseGate>>> {
        PAUSE_BEFORE_COMMIT.get_or_init(|| Mutex::new(None))
    }
}

/// Synchronization gate used with [`set_test_inject_branch_ref_pause_before_commit`].
#[cfg(test)]
pub type BranchRefPauseGate = std::sync::Arc<branch_ref_pause_inject::PauseGate>;

/// Test-only: pause [`update_branch_for_commit`] on the files backend after reflogs, before committing the ref.
#[cfg(test)]
pub fn set_test_inject_branch_ref_pause_before_commit(gate: Option<BranchRefPauseGate>) {
    *branch_ref_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = gate;
}

#[cfg(test)]
fn test_branch_ref_pause_before_commit() {
    let gate = branch_ref_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(gate) = gate {
        let mut wait = gate.wait.lock().unwrap_or_else(|e| e.into_inner());
        *wait = true;
        gate.cv.notify_all();
        while *wait {
            wait = gate.cv.wait(wait).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(not(test))]
fn test_branch_ref_pause_before_commit() {}

#[cfg(test)]
mod ref_store_batch_pause_inject {
    use std::sync::{Condvar, Mutex, OnceLock};

    pub struct PauseGate {
        pub wait: Mutex<bool>,
        pub cv: Condvar,
    }

    // hygiene: test-only ref-store batch commit pause injection
    /// Armed pause: ref index, the repository it applies to, and the gate.
    pub type PauseSlot = Option<(usize, std::path::PathBuf, std::sync::Arc<PauseGate>)>;

    static PAUSE_AFTER_REF_WRITE: OnceLock<Mutex<PauseSlot>> = OnceLock::new();

    pub fn pause_slot() -> &'static Mutex<PauseSlot> {
        PAUSE_AFTER_REF_WRITE.get_or_init(|| Mutex::new(None))
    }
}

/// Synchronization gate used with [`set_test_inject_ref_store_batch_pause_after_apply`].
#[cfg(test)]
pub type RefStoreBatchPauseGate = std::sync::Arc<ref_store_batch_pause_inject::PauseGate>;

/// Test-only: build a pause gate for [`set_test_inject_ref_store_batch_pause_after_apply`].
#[cfg(test)]
pub fn new_ref_store_batch_pause_gate() -> RefStoreBatchPauseGate {
    std::sync::Arc::new(ref_store_batch_pause_inject::PauseGate {
        wait: std::sync::Mutex::new(false),
        cv: std::sync::Condvar::new(),
    })
}

/// Test-only: pause files ref-store batch commit after persisting the ref at `apply_index` (0-based).
///
/// Only ref stores for `git_dir` pause, so tests running in parallel against
/// other repositories are unaffected.
#[cfg(test)]
pub fn set_test_inject_ref_store_batch_pause_after_apply(
    apply_index: usize,
    git_dir: &Path,
    gate: Option<RefStoreBatchPauseGate>,
) {
    *ref_store_batch_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = gate.map(|g| (apply_index, git_dir.to_path_buf(), g));
}

#[cfg(test)]
struct RefStorePauseTestSerial {
    /// Held while one test owns ref-store batch pause injection.
    holder: Option<std::thread::ThreadId>,
}

#[cfg(test)]
static REF_STORE_PAUSE_TEST_SERIAL: std::sync::Mutex<RefStorePauseTestSerial> =
    std::sync::Mutex::new(RefStorePauseTestSerial { holder: None });

/// Serialize ref-store batch pause injection tests and clear injection on drop.
#[cfg(test)]
pub(crate) struct RefStoreBatchPauseTestScope {
    _serial: std::sync::MutexGuard<'static, RefStorePauseTestSerial>,
}

#[cfg(test)]
pub(crate) fn ref_store_batch_pause_test_scope() -> RefStoreBatchPauseTestScope {
    let mut serial = REF_STORE_PAUSE_TEST_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        serial.holder.is_none(),
        "ref_store_batch_pause_test_scope must not nest"
    );
    serial.holder = Some(std::thread::current().id());
    set_test_inject_ref_store_batch_pause_after_apply(0, Path::new(""), None);
    RefStoreBatchPauseTestScope { _serial: serial }
}

#[cfg(test)]
impl Drop for RefStoreBatchPauseTestScope {
    fn drop(&mut self) {
        set_test_inject_ref_store_batch_pause_after_apply(0, Path::new(""), None);
        self._serial.holder = None;
    }
}

#[cfg(test)]
pub(crate) fn test_ref_store_batch_pause_after_ref_write(idx: usize, git_dir: &Path) {
    let slot = ref_store_batch_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some((target, target_dir, gate)) = slot {
        let same_repo = |a: &Path, b: &Path| {
            a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
        };
        if idx != target || !same_repo(git_dir, &target_dir) {
            return;
        }
        let mut wait = gate.wait.lock().unwrap_or_else(|e| e.into_inner());
        *wait = true;
        gate.cv.notify_all();
        while *wait {
            wait = gate.cv.wait(wait).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(not(test))]
pub(crate) fn test_ref_store_batch_pause_after_ref_write(_idx: usize, _git_dir: &Path) {}

/// Loose branch ref lock held between CAS validation and the ref file commit.
///
/// Dropping without [`commit_branch_commit_ref_lock`](Self::commit) aborts the lock file.
pub struct BranchCommitRefLock {
    lock_path: PathBuf,
    disarmed: bool,
}

impl BranchCommitRefLock {
    /// Acquire the branch ref lock and verify `expected_old` before worktree mutations.
    ///
    /// # Errors
    ///
    /// Same as [`update_branch_for_commit_with_config`], except no reflogs or ref write occur yet.
    pub fn acquire(
        git_dir: &Path,
        branch_ref: &str,
        expected_old: Option<ObjectId>,
    ) -> Result<Self> {
        ensure_refname_safe_for_storage(branch_ref)?;
        let storage_dir = ref_storage_dir(git_dir, branch_ref);
        if packed_ref_namespace_conflict(&storage_dir, branch_ref)? {
            return Err(Error::InvalidRef(format!(
                "cannot update ref '{}': reference namespace conflict",
                branch_ref
            )));
        }
        let stor = crate::ref_namespace::storage_ref_name(branch_ref);
        let path = storage_dir.join(&stor);
        remove_empty_ref_directory(&path);
        if fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
        {
            let display = ref_path_for_display(&path);
            return Err(crate::error::RefLockError::DirectoryInTheWay {
                refname: branch_ref.to_owned(),
                path: display,
            }
            .into());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let lock = lock_path_for_ref(&path);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)?;

        let current = read_loose_or_packed_oid(git_dir, branch_ref)?;
        if let Err(err) = verify_branch_cas(expected_old, current, branch_ref) {
            abort_loose_ref_lock(&lock);
            return Err(err);
        }

        Ok(Self {
            lock_path: lock,
            disarmed: false,
        })
    }

    /// Append branch and `HEAD` reflog entries while holding the lock from [`Self::acquire`].
    ///
    /// Intended to run before worktree checkout so reflog I/O failures do not leave the
    /// index or working tree ahead of `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns I/O or reflog failures; the ref lock is aborted and any branch reflog line
    /// written in this call is truncated.
    pub fn write_reflogs(
        &mut self,
        git_dir: &Path,
        update: &BranchCommitRefUpdate<'_>,
        reflog_old: &ObjectId,
        config: &ConfigSet,
    ) -> Result<()> {
        write_branch_commit_reflogs(git_dir, self, update, reflog_old, config)
    }

    /// Write the new OID into the held lock file and rename it over the branch ref.
    ///
    /// Call after the worktree matches `update.new_oid` when splitting reflog work from checkout.
    ///
    /// # Errors
    ///
    /// Returns I/O failures; the ref lock is aborted unless the rename succeeds.
    pub fn finish_ref_commit(
        mut self,
        git_dir: &Path,
        update: &BranchCommitRefUpdate<'_>,
    ) -> Result<()> {
        finish_branch_commit_ref_lock_files(git_dir, &mut self, update)?;
        self.disarmed = true;
        Ok(())
    }

    /// Write reflogs and commit the branch ref while holding the lock from [`Self::acquire`].
    ///
    /// # Errors
    ///
    /// Returns I/O or reflog failures; the lock is aborted unless the ref commit succeeds.
    pub fn commit(
        mut self,
        git_dir: &Path,
        update: &BranchCommitRefUpdate<'_>,
        reflog_old: &ObjectId,
        config: &ConfigSet,
    ) -> Result<()> {
        write_branch_commit_reflogs(git_dir, &mut self, update, reflog_old, config)?;
        finish_branch_commit_ref_lock_files(git_dir, &mut self, update)?;
        self.disarmed = true;
        Ok(())
    }
}

impl Drop for BranchCommitRefLock {
    fn drop(&mut self) {
        if !self.disarmed {
            abort_loose_ref_lock(&self.lock_path);
        }
    }
}

fn write_branch_commit_reflogs(
    git_dir: &Path,
    lock: &mut BranchCommitRefLock,
    update: &BranchCommitRefUpdate<'_>,
    reflog_old: &ObjectId,
    config: &ConfigSet,
) -> Result<()> {
    let mut branch_reflog_written = false;
    if should_autocreate_reflog_with_config(config, git_dir, update.branch_ref) {
        match append_reflog_with_config(
            git_dir,
            update.branch_ref,
            reflog_old,
            &update.new_oid,
            update.identity,
            update.reflog_message,
            false,
            Some(config),
        ) {
            Ok(()) => branch_reflog_written = true,
            Err(err) => {
                abort_loose_ref_lock(&lock.lock_path);
                return Err(err);
            }
        }
    }

    if should_autocreate_reflog_with_config(config, git_dir, "HEAD") {
        if let Err(err) = append_reflog_with_config(
            git_dir,
            "HEAD",
            reflog_old,
            &update.new_oid,
            update.identity,
            update.reflog_message,
            false,
            Some(config),
        ) {
            if branch_reflog_written {
                let _ = crate::reflog::truncate_last_reflog_line(git_dir, update.branch_ref);
            }
            abort_loose_ref_lock(&lock.lock_path);
            return Err(err);
        }
    }

    Ok(())
}

fn finish_branch_commit_ref_lock_files(
    git_dir: &Path,
    lock: &mut BranchCommitRefLock,
    update: &BranchCommitRefUpdate<'_>,
) -> Result<()> {
    test_branch_ref_pause_before_commit();

    let storage_dir = ref_storage_dir(git_dir, update.branch_ref);
    let stor = crate::ref_namespace::storage_ref_name(update.branch_ref);
    let path = storage_dir.join(&stor);
    let content = format!("{}\n", update.new_oid);
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new().write(true).open(&lock.lock_path)?;
        file.write_all(content.as_bytes())?;
    }
    fs::rename(&lock.lock_path, &path).inspect_err(|_err| {
        abort_loose_ref_lock(&lock.lock_path);
    })?;
    Ok(())
}

fn update_branch_for_commit_files(
    git_dir: &Path,
    update: &BranchCommitRefUpdate<'_>,
    reflog_old: &ObjectId,
    config: &ConfigSet,
) -> Result<()> {
    let lock = BranchCommitRefLock::acquire(git_dir, update.branch_ref, update.expected_old)?;
    lock.commit(git_dir, update, reflog_old, config)
}

fn update_branch_for_commit_reftable(
    git_dir: &Path,
    update: &BranchCommitRefUpdate<'_>,
    reflog_old: &ObjectId,
) -> Result<()> {
    let current = resolve_ref(git_dir, update.branch_ref).ok();
    verify_branch_cas(update.expected_old, current, update.branch_ref)?;

    let mut updates = Vec::new();
    let branch_log = if should_autocreate_reflog(git_dir, update.branch_ref) {
        Some(crate::reftable::reftable_log_record_for_commit(
            git_dir,
            update.branch_ref,
            reflog_old,
            &update.new_oid,
            update.identity,
            update.reflog_message,
        )?)
    } else {
        None
    };
    updates.push(crate::reftable::ReftableTransactionUpdate {
        refname: update.branch_ref.to_owned(),
        value: Some(crate::reftable::RefValue::Val1(update.new_oid)),
        log: branch_log,
        expected_old: update.expected_old,
    });

    if should_autocreate_reflog(git_dir, "HEAD") {
        updates.push(crate::reftable::ReftableTransactionUpdate {
            refname: "HEAD".to_owned(),
            value: None,
            log: Some(crate::reftable::reftable_log_record_for_commit(
                git_dir,
                "HEAD",
                reflog_old,
                &update.new_oid,
                update.identity,
                update.reflog_message,
            )?),
            expected_old: None,
        });
    }

    crate::reftable::reftable_write_transaction(git_dir, updates)
}

/// Apply [`BranchCommitRefUpdate`]: branch CAS, ref write, branch + `HEAD` reflogs.
pub fn update_branch_for_commit(git_dir: &Path, update: &BranchCommitRefUpdate<'_>) -> Result<()> {
    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    update_branch_for_commit_with_config(git_dir, update, &config)
}

/// Like [`update_branch_for_commit`], using a repository config snapshot for reflog policy.
pub fn update_branch_for_commit_with_config(
    git_dir: &Path,
    update: &BranchCommitRefUpdate<'_>,
    config: &ConfigSet,
) -> Result<()> {
    let head_target = read_head(git_dir)?;
    if head_target.as_deref() != Some(update.branch_ref) {
        return Err(Error::Message(format!(
            "HEAD does not point at branch '{}'",
            update.branch_ref
        )));
    }

    let reflog_old = update.expected_old.unwrap_or_else(crate::diff::zero_oid);

    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return update_branch_for_commit_reftable(git_dir, update, &reflog_old);
    }

    update_branch_for_commit_files(git_dir, update, &reflog_old, config)
}

/// Filesystem path to the reflog file for `refname` (same layout as [`append_reflog`]).
///
/// Branch and tag reflogs live under the shared [`common_dir`] when the repository uses a
/// `commondir` link (linked worktrees / `git clone --shared` member repos); `HEAD` stays under
/// `git_dir`.
#[must_use]
pub fn reflog_file_path(git_dir: &Path, refname: &str) -> PathBuf {
    let (store, stor_name) = crate::worktree_ref::resolve_ref_storage(git_dir, refname);
    store.join("logs").join(stor_name)
}

fn ref_storage_dir(git_dir: &Path, refname: &str) -> PathBuf {
    crate::worktree_ref::resolve_ref_storage(git_dir, refname).0
}

/// Normalize a ref prefix for filesystem traversal and packed-ref filtering.
///
/// Loose refs live in a directory tree mirroring ref names. A prefix like
/// `refs/remotes/origin` (no trailing slash) must map to the `origin/` directory
/// under `refs/remotes/`, not to a sibling file named `origin`. When the prefix
/// already names a **single loose ref file** (e.g. `refs/heads/main`), keep it
/// without a trailing slash so we read that file instead of a non-existent
/// directory.
fn normalize_list_refs_prefix(git_dir: &Path, prefix: &str) -> String {
    if prefix.is_empty() {
        return String::new();
    }
    if prefix.ends_with('/') {
        return prefix.to_string();
    }
    let candidate = ref_storage_dir(git_dir, prefix).join(prefix);
    if candidate.is_file() {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// After the first clone fetch, batch remote-tracking refs into `packed-refs`
/// (Git's post-clone layout). Leaves `refs/remotes/{remote}/HEAD` as a loose
/// symbolic ref.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures, or when a tag peel cannot be read.
pub fn pack_remote_tracking_refs_for_clone(git_dir: &Path, remote: &str) -> Result<()> {
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return Ok(());
    }
    let prefix = format!("refs/remotes/{remote}/");
    let head_sym = format!("refs/remotes/{remote}/HEAD");
    let mut entries = list_refs(git_dir, &prefix)?;
    entries.retain(|(name, _)| name != &head_sym);
    if entries.is_empty() {
        return Ok(());
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let odb =
        crate::odb::Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf());
    let mut merged: HashMap<String, (ObjectId, Option<ObjectId>)> = HashMap::new();
    let packed_path = store::paths::packed_refs_path(git_dir);
    if let Ok(data) = fs::read(&packed_path) {
        parse_packed_refs_to_map(&packed_path, &data)?;
        let content = String::from_utf8(data)
            .map_err(|_| packed_refs_unexpected_line(&packed_path, "<invalid utf-8>"))?;
        let mut last_name: Option<String> = None;
        for line in content.split('\n') {
            if line.starts_with('#') {
                continue;
            }
            if let Some(hex) = line.strip_prefix('^') {
                let Ok(peel_oid) = ObjectId::from_hex(hex.trim()) else {
                    continue;
                };
                if let Some(name) = &last_name {
                    if let Some(entry) = merged.get_mut(name) {
                        entry.1 = Some(peel_oid);
                    }
                }
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            let Some(hex) = parts.next() else {
                continue;
            };
            let Some(name) = parts.next() else {
                continue;
            };
            let name = name.trim();
            let Ok(oid) = ObjectId::from_hex(hex) else {
                continue;
            };
            if name.starts_with(&prefix) {
                last_name = None;
                continue;
            }
            merged.insert(name.to_owned(), (oid, None));
            last_name = Some(name.to_owned());
        }
    }

    for (name, oid) in &entries {
        let mut peel = None;
        if let Ok(obj) = odb.read(oid) {
            if obj.kind == crate::objects::ObjectKind::Tag {
                if let Ok(tag) = crate::objects::parse_tag(&obj.data) {
                    peel = Some(tag.object);
                }
            }
        }
        merged.insert(name.clone(), (*oid, peel));
    }

    let mut names: Vec<String> = merged.keys().cloned().collect();
    names.sort();

    let mut body = String::from("# pack-refs with: peeled fully-peeled sorted\n");
    for name in &names {
        let Some((oid, peel)) = merged.get(name) else {
            continue;
        };
        body.push_str(&oid.to_hex());
        body.push(' ');
        body.push_str(name);
        body.push('\n');
        if let Some(p) = peel {
            body.push('^');
            body.push_str(&p.to_hex());
            body.push('\n');
        }
    }

    atomic_rewrite_packed_refs(git_dir, &body)?;

    for (name, _) in &entries {
        let storage_dir = ref_storage_dir(git_dir, name);
        let stor = crate::ref_namespace::storage_ref_name(name);
        let path = storage_dir.join(&stor);
        if path.is_file() {
            let _ = fs::remove_file(&path);
        }
        if let Some(parent) = path.parent() {
            remove_empty_ref_directory(parent);
        }
    }
    Ok(())
}

/// Rewrite `packed-refs` under the packed-refs lock (tempfile + rename).
fn atomic_rewrite_packed_refs(git_dir: &Path, body: &str) -> Result<()> {
    let packed_path = store::paths::packed_refs_path(git_dir);
    let lock = lock_path_for_ref(&packed_path);
    let abs_lock = fs::canonicalize(git_dir)
        .map(|d| d.join("packed-refs.lock"))
        .unwrap_or_else(|_| lock.clone());
    acquire_packed_refs_lock(git_dir, &lock, &abs_lock)?;

    let tmp = packed_path.with_extension("new");
    let mut created_tmp = false;
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(Error::Io)?;
        created_tmp = true;
        use std::io::Write as _;
        file.write_all(body.as_bytes()).map_err(Error::Io)?;
        drop(file);
        fs::rename(&tmp, &packed_path).map_err(Error::Io)?;
        created_tmp = false;
        Ok(())
    })();
    let _ = fs::remove_file(&lock);
    if write_result.is_err() && created_tmp {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// One reference line (and optional peel) for [`write_packed_refs_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedRefRecord {
    /// Full ref name (for example `refs/heads/feature`).
    pub refname: String,
    /// Object id stored at this ref.
    pub oid: ObjectId,
    /// When set, emit a following `^` peeled line (annotated tags).
    pub peeled: Option<ObjectId>,
}

/// Replace `packed-refs` with `records`, sorted by ref name.
///
/// Existing loose ref files for names present in `records` are removed so the
/// packed file is authoritative (Git's `pack-refs` behaviour).
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] for unsafe ref names, or [`Error::Io`] on failure.
pub fn write_packed_refs_file(git_dir: &Path, records: &[PackedRefRecord]) -> Result<()> {
    let mut sorted: Vec<&PackedRefRecord> = records.iter().collect();
    sorted.sort_by(|a, b| a.refname.cmp(&b.refname));
    for rec in &sorted {
        ensure_refname_safe_for_storage(&rec.refname)?;
    }
    let mut body = String::from("# pack-refs with: peeled fully-peeled sorted\n");
    for rec in &sorted {
        body.push_str(&rec.oid.to_hex());
        body.push(' ');
        body.push_str(&rec.refname);
        body.push('\n');
        if let Some(peeled) = rec.peeled {
            body.push('^');
            body.push_str(&peeled.to_hex());
            body.push('\n');
        }
    }
    atomic_rewrite_packed_refs(git_dir, &body)?;
    for rec in &sorted {
        let storage_dir = ref_storage_dir(git_dir, &rec.refname);
        let stor = crate::ref_namespace::storage_ref_name(&rec.refname);
        let loose = storage_dir.join(stor);
        if loose.is_file() {
            fs::remove_file(&loose).map_err(Error::Io)?;
        }
        if let Some(parent) = loose.parent() {
            remove_empty_ref_directory(parent);
        }
    }
    Ok(())
}

/// List all refs under a given prefix (e.g. `"refs/heads/"`).
///
/// Dispatches to the reftable backend when `extensions.refStorage = reftable`.
///
/// Returns a sorted list of `(refname, ObjectId)` pairs.
///
/// # Errors
///
/// Returns [`Error::Io`] on directory traversal errors.
/// List refs under `prefix` using the given repository's ref store.
pub fn list_refs_for_repository(
    repo: &crate::repo::Repository,
    prefix: &str,
) -> Result<Vec<(String, ObjectId)>> {
    let git_dir = &repo.git_dir();
    let prefix_norm = normalize_list_refs_prefix(git_dir, prefix);
    let mut results = collect_resolved_refs(repo.refs(), prefix_norm.as_str())?;
    if crate::worktree_ref::is_linked_worktree_git_dir(git_dir) {
        results.retain(|(name, _)| crate::worktree_ref::ref_visible_from_worktree(git_dir, name));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(results)
}

/// List resolved refs under `git_dir` whose full names start with `prefix`.
///
/// Uses the on-disk ref backend (files or reftable) and applies linked-worktree
/// visibility rules when `git_dir` is a linked worktree git directory.
///
/// # Errors
///
/// Propagates ref-store open/read failures.
pub fn list_refs(git_dir: &Path, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    let prefix_norm = normalize_list_refs_prefix(git_dir, prefix);
    let prefix = prefix_norm.as_str();
    let store = open_ref_store(git_dir)?;
    if store.format() != RefStorageFormat::Files {
        return crate::reftable::reftable_list_refs(git_dir, prefix);
    }
    let mut results = collect_resolved_refs(store.as_ref(), prefix)?;
    if crate::worktree_ref::is_linked_worktree_git_dir(git_dir) {
        results.retain(|(name, _)| crate::worktree_ref::ref_visible_from_worktree(git_dir, name));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(results)
}

/// Resolve a ref using Git DWIM rules (`expand_ref` / `repo_dwim_ref`).
pub fn resolve_ref_dwim(git_dir: &Path, spec: &str) -> (usize, Option<ObjectId>) {
    crate::worktree_ref::resolve_ref_dwim(|candidate| resolve_ref(git_dir, candidate).ok(), spec)
}

/// List refs under `prefix` using **literal** on-disk paths (ignores `GIT_NAMESPACE`).
///
/// Used by `receive-pack` when advertising: the server must see every physical ref so refs outside
/// the active namespace can be offered as `.have` lines (matches Git `show_ref_cb`).
pub fn list_refs_physical(git_dir: &Path, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    if open_ref_store(git_dir)?.format() != RefStorageFormat::Files {
        return crate::reftable::reftable_list_refs(git_dir, prefix);
    }
    let mut by_name: HashMap<String, ObjectId> = HashMap::new();
    let stored_prefix = prefix.to_owned();

    if let Some(cdir) = common_dir(git_dir) {
        if cdir != git_dir {
            collect_packed_refs_into_map(&cdir, &stored_prefix, true, &mut by_name)?;
            let (loose_root, loose_prefix) = loose_ref_list_walk_root(&cdir, &stored_prefix);
            collect_loose_refs_into_map(
                &loose_root,
                &loose_prefix,
                &stored_prefix,
                &cdir,
                true,
                &mut by_name,
            )?;
        }
    }

    collect_packed_refs_into_map(git_dir, &stored_prefix, true, &mut by_name)?;
    let (loose_root, loose_prefix) = loose_ref_list_walk_root(git_dir, &stored_prefix);
    collect_loose_refs_into_map(
        &loose_root,
        &loose_prefix,
        &stored_prefix,
        git_dir,
        true,
        &mut by_name,
    )?;

    let mut results: Vec<(String, ObjectId)> = by_name.into_iter().collect();
    results.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(results)
}

/// Collect commit OIDs from alternate repositories' refs, matching Git's
/// `git for-each-ref --format=%(objectname)` on each alternate (with optional
/// `core.alternateRefsPrefixes` arguments).
///
/// Order is preserved: alternates file order, then ref iteration order from
/// [`list_refs`] under each configured prefix (or all of `refs/` when no
/// prefixes are set). Duplicate OIDs are skipped while preserving first-seen
/// order.
pub fn collect_alternate_ref_oids(receiving_git_dir: &Path) -> Result<Vec<ObjectId>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(receiving_git_dir),
        true,
    )?;
    let objects_dir = receiving_git_dir.join("objects");
    let alternates = pack::read_alternates_recursive(&objects_dir).unwrap_or_default();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for alt_objects in alternates {
        let Some(alt_git_dir) = alt_objects.parent().map(PathBuf::from) else {
            continue;
        };
        if !alt_git_dir.join("refs").is_dir() {
            continue;
        }
        if let Some(prefixes) = config
            .get("core.alternaterefsprefixes")
            .or_else(|| config.get("core.alternateRefsPrefixes"))
        {
            for part in prefixes.split_whitespace() {
                if let Ok(oid) = resolve_ref(&alt_git_dir, part) {
                    if seen.insert(oid) {
                        out.push(oid);
                    }
                    continue;
                }
                for (_, oid) in list_refs(&alt_git_dir, part)? {
                    if seen.insert(oid) {
                        out.push(oid);
                    }
                }
            }
        } else {
            for (_, oid) in list_refs(&alt_git_dir, "refs/")? {
                if seen.insert(oid) {
                    out.push(oid);
                }
            }
        }
    }
    Ok(out)
}

/// List refs matching a glob pattern (e.g. `refs/heads/topic/*`) using `repo`'s ref store.
pub fn list_refs_glob_for_repository(
    repo: &crate::repo::Repository,
    pattern: &str,
) -> Result<Vec<(String, ObjectId)>> {
    let glob_pos = pattern.find(['*', '?', '[']);
    let prefix_owned: String = match glob_pos {
        Some(pos) => pattern[..pos].to_owned(),
        None => {
            let mut p = pattern.trim_end_matches('/').to_owned();
            if !p.is_empty() {
                p.push('/');
            }
            p
        }
    };
    let all = list_refs_for_repository(repo, prefix_owned.as_str())?;
    let mut results = Vec::new();
    for (refname, oid) in all {
        if ref_matches_glob(&refname, pattern) {
            results.push((refname, oid));
        }
    }
    Ok(results)
}

/// List refs matching a glob pattern (e.g. `refs/heads/topic/*`).
pub fn list_refs_glob(git_dir: &Path, pattern: &str) -> Result<Vec<(String, ObjectId)>> {
    let glob_pos = pattern.find(['*', '?', '[']);
    let prefix_owned: String = match glob_pos {
        // Literal segment before the first wildcard (e.g. `refs/heads/b05*` → `refs/heads/b05`).
        Some(pos) => pattern[..pos].to_owned(),
        None => {
            let mut p = pattern.trim_end_matches('/').to_owned();
            if !p.is_empty() {
                p.push('/');
            }
            p
        }
    };
    let prefix = prefix_owned.as_str();
    let all = list_refs(git_dir, prefix)?;
    let mut results = Vec::new();
    for (refname, oid) in all {
        if ref_matches_glob(&refname, pattern) {
            results.push((refname, oid));
        }
    }
    Ok(results)
}

/// Check whether a ref name matches a glob pattern.
///
/// Supports `*`, `?`, and `[…]` wildcards. An exact string match is also accepted.
pub fn ref_matches_glob(refname: &str, pattern: &str) -> bool {
    // For exact matches (no glob characters), check suffix match
    if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
        return refname == pattern
            || refname.ends_with(&format!("/{pattern}"))
            || refname.starts_with(&format!("{pattern}/"));
    }
    glob_match(pattern, refname)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pat = pattern.as_bytes();
    let txt = text.as_bytes();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);
    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// OID stored directly in a loose ref file (40 hex), ignoring symbolic targets.
fn loose_ref_file_direct_oid(path: &Path) -> Option<ObjectId> {
    let content = fs::read_to_string(path).ok()?;
    let content = content.trim_end_matches('\n').trim();
    if ObjectId::is_full_hex(content) {
        content.parse().ok()
    } else {
        None
    }
}

fn collect_loose_refs_into_map(
    dir: &Path,
    prefix: &str,
    filter_prefix: &str,
    resolve_git_dir: &Path,
    physical_keys: bool,
    out: &mut HashMap<String, ObjectId>,
) -> Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::NotADirectory
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            return Ok(());
        }
        Err(e) => return Err(Error::Io(e)),
    };

    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let refname = format!("{prefix}{name_str}");
        let path = entry.path();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            collect_loose_refs_into_map(
                &path,
                &format!("{refname}/"),
                filter_prefix,
                resolve_git_dir,
                physical_keys,
                out,
            )?;
        } else if meta.is_file() {
            if physical_keys {
                if let Some(oid) = loose_ref_file_direct_oid(&path) {
                    out.insert(refname, oid);
                } else if let Ok(Ref::Symbolic(target)) = read_ref_file(&path) {
                    if let Ok(oid) = resolve_ref(resolve_git_dir, target.trim()) {
                        out.insert(refname, oid);
                    }
                }
            } else {
                let logical = crate::ref_namespace::logical_ref_name_from_storage(&refname)
                    .unwrap_or_else(|| refname.clone());
                if !ref_name_matches_list_prefix(&logical, filter_prefix.trim_end_matches('/')) {
                    continue;
                }
                if let Some(oid) = loose_ref_file_direct_oid(&path) {
                    out.insert(logical, oid);
                } else if let Ok(Ref::Symbolic(target)) = read_ref_file(&path) {
                    if let Ok(oid) = resolve_ref(resolve_git_dir, target.trim()) {
                        out.insert(logical, oid);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Resolve `@{-N}` syntax to the branch name (not an OID).
/// Returns the branch name of the Nth previously checked out branch.
pub fn resolve_at_n_branch(git_dir: &Path, spec: &str) -> Result<String> {
    // Parse the N from @{-N}
    let inner = spec
        .strip_prefix("@{-")
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| Error::InvalidRef(format!("not an @{{-N}} ref: {spec}")))?;
    let n: usize = inner
        .parse()
        .map_err(|_| Error::InvalidRef(format!("invalid N in {spec}")))?;
    if n == 0 {
        return Err(Error::InvalidRef("@{-0} is not valid".to_string()));
    }
    let entries = crate::reflog::read_reflog(git_dir, "HEAD")?;
    let mut count = 0usize;
    for entry in entries.iter().rev() {
        let msg = &entry.message;
        if let Some(rest) = msg.strip_prefix("checkout: moving from ") {
            count += 1;
            if count == n {
                if let Some(to_pos) = rest.find(" to ") {
                    return Ok(rest[..to_pos].to_string());
                }
            }
        }
    }
    Err(Error::InvalidRef(format!(
        "{spec}: only {count} checkout(s) in reflog"
    )))
}

fn ref_name_matches_list_prefix(refname: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    if refname.starts_with(prefix) {
        return true;
    }
    if refname == prefix {
        return true;
    }
    false
}

fn list_prefix_upper_bound(prefix: &str) -> Option<String> {
    let base = prefix.trim_end_matches('/');
    let (head, tail) = base.rsplit_once('/')?;
    let mut chars: Vec<char> = tail.chars().collect();
    while let Some(last) = chars.pop() {
        if last != '\u{10ffff}' {
            chars.push(char::from_u32(last as u32 + 1).unwrap_or(last));
            return Some(format!("{head}/{}", chars.into_iter().collect::<String>()));
        }
    }
    None
}

/// When `prefix` names a partial ref (e.g. `refs/heads/b05/` with no `b05/` directory),
/// start the walk at the parent directory (`refs/heads/`) and filter by prefix.
pub(crate) fn loose_ref_list_walk_root(git_dir: &Path, prefix: &str) -> (PathBuf, String) {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return (git_dir.join("refs"), "refs/".to_owned());
    }
    let direct = git_dir.join(trimmed);
    if direct.is_dir() {
        return (direct, format!("{trimmed}/"));
    }
    if let Some(slash) = trimmed.rfind('/') {
        let parent = &trimmed[..slash];
        return (git_dir.join(parent), format!("{parent}/"));
    }
    (git_dir.to_path_buf(), String::new())
}

fn collect_packed_refs_into_map(
    git_dir: &Path,
    prefix: &str,
    physical_keys: bool,
    out: &mut HashMap<String, ObjectId>,
) -> Result<()> {
    let map = read_packed_refs_map_for_prefix(git_dir, prefix)?;
    for (refname, oid) in map {
        let key = if physical_keys {
            refname.clone()
        } else {
            crate::ref_namespace::logical_ref_name_from_storage(&refname).unwrap_or(refname)
        };
        out.insert(key, oid);
    }
    Ok(())
}

#[cfg(test)]
mod update_branch_for_commit_tests {
    use super::*;
    use crate::reflog::read_reflog;
    use crate::repo::init_repository;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use tempfile::TempDir;

    fn sample_oid(hex: &str) -> ObjectId {
        hex.parse().unwrap()
    }

    #[test]
    fn update_branch_for_commit_holds_lock_until_ref_commit() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        let git_dir = repo.git_dir().to_path_buf();
        let oid1 = sample_oid("67bf698f3ab735e92fb011a99cff3497c44d30c1");
        let oid2 = sample_oid("1111111111111111111111111111111111111111");
        let oid3 = sample_oid("2222222222222222222222222222222222222222");
        write_ref(&git_dir, "refs/heads/main", &oid1).unwrap();

        let gate = Arc::new(branch_ref_pause_inject::PauseGate {
            wait: Mutex::new(false),
            cv: Condvar::new(),
        });
        set_test_inject_branch_ref_pause_before_commit(Some(gate.clone()));

        let git_dir_thread = git_dir.clone();
        let handle = thread::spawn(move || {
            update_branch_for_commit(
                &git_dir_thread,
                &BranchCommitRefUpdate {
                    branch_ref: "refs/heads/main",
                    expected_old: Some(oid1),
                    new_oid: oid2,
                    identity: "T <t@e.com> 1 +0000",
                    reflog_message: "commit: test",
                },
            )
        });

        let mut wait = gate.wait.lock().unwrap();
        while !*wait {
            wait = gate.cv.wait(wait).unwrap();
        }

        assert!(
            write_ref(&git_dir, "refs/heads/main", &oid3).is_err(),
            "concurrent branch update must block on the held ref lock"
        );

        *wait = false;
        gate.cv.notify_all();
        drop(wait);
        handle.join().unwrap().unwrap();
        set_test_inject_branch_ref_pause_before_commit(None);

        assert_eq!(resolve_ref(&git_dir, "refs/heads/main").unwrap(), oid2);
    }

    #[test]
    fn update_branch_for_commit_reftable_transaction_is_all_or_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .unwrap();
        let git_dir = repo.git_dir().to_path_buf();
        let new_oid = sample_oid("3333333333333333333333333333333333333333");

        crate::reftable::set_test_inject_reftable_transaction_fail(true);
        let err = update_branch_for_commit(
            &git_dir,
            &BranchCommitRefUpdate {
                branch_ref: "refs/heads/main",
                expected_old: None,
                new_oid,
                identity: "T <t@e.com> 1 +0000",
                reflog_message: "commit (initial): test",
            },
        )
        .unwrap_err();
        crate::reftable::set_test_inject_reftable_transaction_fail(false);
        assert!(!matches!(err, Error::Message(_)));

        assert!(resolve_ref(&git_dir, "refs/heads/main").is_err());
        assert!(read_reflog(&git_dir, "refs/heads/main").unwrap().is_empty());
        assert!(read_reflog(&git_dir, "HEAD").unwrap().is_empty());
    }
}

#[cfg(test)]
mod refname_available_tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};
    use tempfile::tempdir;

    #[test]
    fn verify_ref_transaction_batch_rejects_same_batch_parent_child() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        let items = vec![
            RefBatchItem {
                name: "refs/heads/c".to_owned(),
                new_oid: Some(sample_oid()),
            },
            RefBatchItem {
                name: "refs/heads/c/x".to_owned(),
                new_oid: Some(sample_oid()),
            },
        ];
        let err = verify_ref_transaction_batch(git_dir, &items).unwrap_err();
        assert!(matches!(
            err,
            RefnameUnavailable::SameBatch { refname, .. } if refname == "refs/heads/c/x"
        ));
    }

    fn sample_oid() -> ObjectId {
        "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
    }

    #[test]
    fn loose_parent_blocks_child_create() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/1l")).unwrap();
        fs::write(
            git_dir.join("refs/1l/c"),
            "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
        )
        .unwrap();
        assert_eq!(
            read_raw_ref(git_dir, "refs/1l/c").unwrap(),
            RawRefLookup::Exists
        );
        let extras = BTreeSet::from([
            "refs/1l/b".to_string(),
            "refs/1l/c/x".to_string(),
            "refs/1l/d".to_string(),
        ]);
        let skip = HashSet::new();
        let err = verify_refname_available_for_create(git_dir, "refs/1l/c/x", &extras, &skip)
            .unwrap_err();
        assert!(matches!(
            err,
            RefnameUnavailable::AncestorExists { ref blocking, .. } if blocking == "refs/1l/c"
        ));
    }

    #[test]
    fn verify_sees_loose_ref_after_canonical_git_dir() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(git_dir.join("refs/1l")).unwrap();
        fs::write(
            git_dir.join("refs/1l/c"),
            "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
        )
        .unwrap();
        let skip = HashSet::new();
        let extras = BTreeSet::new();
        let err = verify_refname_available_for_create(&git_dir, "refs/1l/c/x", &extras, &skip)
            .unwrap_err();
        assert!(matches!(
            err,
            RefnameUnavailable::AncestorExists { ref blocking, .. } if blocking == "refs/1l/c"
        ));
    }

    #[test]
    fn list_refs_glob_b05_star_matches_one_thousand_packed_refs() {
        let git_dir = Path::new("/tmp/grit-bench-refs-cache/refs-100k-packed");
        if !git_dir.is_dir() {
            return;
        }
        let listed = list_refs_glob(git_dir, "refs/heads/b05*").unwrap();
        assert_eq!(listed.len(), 1000, "expected 1k b05* refs");
        assert!(listed
            .iter()
            .all(|(name, _)| name.starts_with("refs/heads/b05")));
    }

    #[test]
    fn list_refs_finds_sibling_under_parent_directory() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/ns/p")).unwrap();
        fs::write(
            git_dir.join("refs/ns/p/x"),
            "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
        )
        .unwrap();
        let listed = list_refs(git_dir, "refs/ns/p/").unwrap();
        assert!(
            listed.iter().any(|(n, _)| n == "refs/ns/p/x"),
            "got {listed:?}"
        );
    }

    #[test]
    fn verify_blocks_parent_when_child_ref_exists() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/ns/p")).unwrap();
        fs::write(
            git_dir.join("refs/ns/p/x"),
            "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
        )
        .unwrap();
        let extras = BTreeSet::from(["refs/ns/p".to_string()]);
        let skip = HashSet::new();
        let err =
            verify_refname_available_for_create(git_dir, "refs/ns/p", &extras, &skip).unwrap_err();
        assert!(matches!(
            err,
            RefnameUnavailable::DescendantExists { ref blocking, .. }
                if blocking == "refs/ns/p/x"
        ));
    }

    #[test]
    fn verify_blocks_parent_when_only_a_packed_child_exists() {
        // No `refs/ns/q/` directory on disk, so the loose listing is skipped;
        // the packed-refs check must still find the descendant.
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs")).unwrap();
        fs::write(
            store::paths::packed_refs_path(git_dir),
            "# pack-refs with: peeled fully-peeled\n\
             67bf698f3ab735e92fb011a99cff3497c44d30c1 refs/ns/q/x\n",
        )
        .unwrap();
        let err = verify_refname_available_for_create(
            git_dir,
            "refs/ns/q",
            &BTreeSet::new(),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            RefnameUnavailable::DescendantExists { ref blocking, .. }
                if blocking == "refs/ns/q/x"
        ));
    }

    #[test]
    fn verify_allows_name_whose_siblings_share_a_prefix() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/tags")).unwrap();
        for name in ["v1.0.1", "v1.0-rc1"] {
            fs::write(
                git_dir.join("refs/tags").join(name),
                "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
            )
            .unwrap();
        }
        verify_refname_available_for_create(
            git_dir,
            "refs/tags/v1.0",
            &BTreeSet::new(),
            &HashSet::new(),
        )
        .expect("siblings are not descendants");
    }

    #[test]
    fn verify_blocks_parent_git_style_nested_path() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/3l/c")).unwrap();
        fs::write(
            git_dir.join("refs/3l/c/x"),
            "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
        )
        .unwrap();
        let extras = BTreeSet::from(["refs/3l/c".to_string()]);
        let skip = HashSet::new();
        let err =
            verify_refname_available_for_create(git_dir, "refs/3l/c", &extras, &skip).unwrap_err();
        assert!(matches!(
            err,
            RefnameUnavailable::DescendantExists { ref blocking, .. }
                if blocking == "refs/3l/c/x"
        ));
    }

    #[test]
    fn intermediate_directory_does_not_block_nested_create() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/ns")).unwrap();
        fs::write(
            git_dir.join("refs/ns/existing"),
            "67bf698f3ab735e92fb011a99cff3497c44d30c1\n",
        )
        .unwrap();
        assert_eq!(
            read_raw_ref(git_dir, "refs/ns").unwrap(),
            RawRefLookup::IsDirectory
        );
        let extras = BTreeSet::from(["refs/ns/newchild".to_string()]);
        let skip = HashSet::new();
        verify_refname_available_for_create(git_dir, "refs/ns/newchild", &extras, &skip).unwrap();
    }
}

#[cfg(test)]
mod ref_storage_traversal_guard_tests {
    use super::*;
    use crate::objects::ObjectId;
    use tempfile::tempdir;

    const TRAVERSAL_REF: &str = "refs/heads/../../config";

    fn bare_repo_with_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().to_path_buf();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(
            git_dir.join("config"),
            "[core]\nrepositoryformatversion = 0\n",
        )
        .unwrap();
        (dir, git_dir)
    }

    fn sample_oid() -> ObjectId {
        "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
    }

    fn assert_invalid_ref_preserves_config(git_dir: &Path, result: Result<()>) {
        assert!(matches!(result, Err(Error::InvalidRef(_))));
        assert!(
            git_dir.join("config").is_file(),
            "repository config must not be touched"
        );
    }

    #[test]
    fn write_ref_rejects_traversal_refname() {
        let (_dir, git_dir) = bare_repo_with_config();
        let result = write_ref(&git_dir, TRAVERSAL_REF, &sample_oid());
        assert_invalid_ref_preserves_config(&git_dir, result);
    }

    #[test]
    fn write_ref_cached_rejects_traversal_refname() {
        let (_dir, git_dir) = bare_repo_with_config();
        let packed = PackedRefs::load(&git_dir).unwrap();
        let result = write_ref_cached(&git_dir, TRAVERSAL_REF, &sample_oid(), &packed);
        assert_invalid_ref_preserves_config(&git_dir, result);
    }

    #[test]
    fn write_symbolic_ref_rejects_traversal_refname() {
        let (_dir, git_dir) = bare_repo_with_config();
        let result = write_symbolic_ref(&git_dir, TRAVERSAL_REF, "refs/heads/main");
        assert_invalid_ref_preserves_config(&git_dir, result);
    }

    #[test]
    fn delete_ref_rejects_traversal_refname() {
        let (_dir, git_dir) = bare_repo_with_config();
        let result = delete_ref(&git_dir, TRAVERSAL_REF);
        assert_invalid_ref_preserves_config(&git_dir, result);
    }

    #[test]
    fn write_ref_rejects_illegal_branch_refname() {
        let (_dir, git_dir) = bare_repo_with_config();
        for refname in [
            "refs/heads/bad name",
            "refs/heads/x..y",
            "refs/heads/a~b",
            "refs/heads/a.lock",
        ] {
            let result = write_ref(&git_dir, refname, &sample_oid());
            assert_invalid_ref_preserves_config(&git_dir, result);
            assert!(
                !git_dir
                    .join(refname.strip_prefix("refs/").unwrap())
                    .exists(),
                "must not write loose ref for {refname}"
            );
        }
    }

    #[test]
    fn write_ref_accepts_refs_heads_head() {
        let (_dir, git_dir) = bare_repo_with_config();
        write_ref(&git_dir, "refs/heads/HEAD", &sample_oid())
            .expect("refs/heads/HEAD is valid ref syntax");
        assert!(git_dir.join("refs/heads/HEAD").is_file());
    }
}

#[cfg(test)]
mod read_raw_ref_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loose_ref_file_is_exists() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(
            git_dir.join("refs/heads/side"),
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap();
        assert_eq!(
            read_raw_ref(git_dir, "refs/heads/side").unwrap(),
            RawRefLookup::Exists
        );
    }

    #[test]
    fn missing_ref_is_not_found() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        assert_eq!(
            read_raw_ref(git_dir, "refs/heads/nope").unwrap(),
            RawRefLookup::NotFound
        );
    }

    #[test]
    fn directory_where_ref_expected_is_is_directory() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        assert_eq!(
            read_raw_ref(git_dir, "refs/heads").unwrap(),
            RawRefLookup::IsDirectory
        );
    }

    #[test]
    fn packed_ref_name_is_exists() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        fs::write(
            store::paths::packed_refs_path(git_dir),
            "# pack-refs with: peeled fully-peeled \n\
             0000000000000000000000000000000000000000 refs/heads/packed\n",
        )
        .unwrap();
        assert_eq!(
            read_raw_ref(git_dir, "refs/heads/packed").unwrap(),
            RawRefLookup::Exists
        );
    }
}

#[cfg(test)]
mod packed_refs_parse_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_rejects_garbage_line() {
        let dir = tempdir().unwrap();
        let path = store::paths::packed_refs_path(dir.path());
        fs::write(
            &path,
            "# pack-refs with: peeled\n\
             0000000000000000000000000000000000000000 refs/heads/main\n\
             not a ref line\n",
        )
        .unwrap();
        let data = fs::read(&path).unwrap();
        assert!(matches!(
            parse_packed_refs_to_map(&path, &data),
            Err(Error::PackedRefsUnexpectedLine { .. })
        ));
    }

    #[test]
    fn parse_rejects_carriage_return_bytes() {
        let dir = tempdir().unwrap();
        let path = store::paths::packed_refs_path(dir.path());
        fs::write(
            &path,
            "# pack-refs with: peeled\n\
             0000000000000000000000000000000000000000 refs/heads/main\r\n",
        )
        .unwrap();
        let data = fs::read(&path).unwrap();
        assert!(matches!(
            parse_packed_refs_to_map(&path, &data),
            Err(Error::PackedRefsUnexpectedLine { .. })
        ));
    }

    #[test]
    fn prefix_filter_unsorted_packed_refs_scans_past_later_names() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        let oid = "1111111111111111111111111111111111111111";
        fs::write(
            store::paths::packed_refs_path(git_dir),
            format!(
                "# pack-refs with: peeled fully-peeled\n{oid} refs/heads/zzz\n{oid} refs/heads/aaa\n{oid} refs/heads/main\n"
            ),
        )
        .unwrap();
        let listed = list_refs_glob(git_dir, "refs/heads/a*").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "refs/heads/aaa");
    }

    #[test]
    fn parse_rejects_blank_line_and_missing_final_newline() {
        let dir = tempdir().unwrap();
        let path = store::paths::packed_refs_path(dir.path());
        fs::write(
            &path,
            "# pack-refs with: peeled\n\n\
             0000000000000000000000000000000000000000 refs/heads/main\n",
        )
        .unwrap();
        let data = fs::read(&path).unwrap();
        assert!(matches!(
            parse_packed_refs_to_map(&path, &data),
            Err(Error::PackedRefsUnexpectedLine { .. })
        ));

        fs::write(
            &path,
            "# pack-refs with: peeled\n\
             0000000000000000000000000000000000000000 refs/heads/main",
        )
        .unwrap();
        let data = fs::read(&path).unwrap();
        assert!(matches!(
            parse_packed_refs_to_map(&path, &data),
            Err(Error::PackedRefsUnexpectedLine { .. })
        ));
    }

    #[test]
    fn parse_rejects_orphan_peel_and_double_space() {
        let dir = tempdir().unwrap();
        let path = store::paths::packed_refs_path(dir.path());
        fs::write(
            &path,
            "# pack-refs with: peeled\n\
             ^0000000000000000000000000000000000000000\n\
             0000000000000000000000000000000000000000 refs/heads/main\n",
        )
        .unwrap();
        let data = fs::read(&path).unwrap();
        assert!(matches!(
            parse_packed_refs_to_map(&path, &data),
            Err(Error::PackedRefsUnexpectedLine { .. })
        ));

        fs::write(
            &path,
            "# pack-refs with: peeled\n\
             0000000000000000000000000000000000000000  refs/heads/main\n",
        )
        .unwrap();
        let data = fs::read(&path).unwrap();
        assert!(matches!(
            parse_packed_refs_to_map(&path, &data),
            Err(Error::PackedRefsUnexpectedLine { .. })
        ));
    }
}

#[cfg(test)]
mod pack_refs_tests {
    use super::*;
    use crate::objects::{serialize_commit, CommitData, ObjectKind};
    use crate::repo::{init_repository, Repository};
    use std::fs;
    use std::sync::Arc;

    fn write_commit(repo: &Repository, parents: Vec<ObjectId>) -> ObjectId {
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let raw = serialize_commit(&CommitData {
            tree,
            parents,
            author: "T <t@example.com> 1 +0000".into(),
            committer: "T <t@example.com> 1 +0000".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "m\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        repo.odb().write(ObjectKind::Commit, &raw).expect("commit")
    }

    #[test]
    fn pack_refs_skips_concurrent_loose_update() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let git_dir = repo.git_dir().to_path_buf();
        let c1 = write_commit(&repo, vec![]);
        let c2 = write_commit(&repo, vec![c1]);
        write_ref(&git_dir, "refs/heads/race", &c1).expect("seed loose");

        let git_dir_hook = git_dir.clone();
        let c2 = Arc::new(c2);
        let c2_hook = Arc::clone(&c2);
        pack_refs_test_hooks::set_before_prune(Some(Box::new(move || {
            write_ref(&git_dir_hook, "refs/heads/race", &c2_hook).expect("concurrent update");
        })));

        let (_path, packed, pruned) = pack_refs_files(&git_dir, repo.odb(), true).expect("pack");
        pack_refs_test_hooks::set_before_prune(None);
        assert!(packed >= 1);
        assert_eq!(pruned, 0, "stale CAS must not delete the updated loose ref");

        let resolved = resolve_ref(&git_dir, "refs/heads/race").expect("resolve");
        assert_eq!(resolved, *c2);
        let packed_body =
            fs::read_to_string(store::paths::packed_refs_path(&git_dir)).unwrap_or_default();
        assert!(
            !packed_body.contains("refs/heads/race"),
            "packed entry for concurrently updated ref must be removed"
        );
    }

    #[test]
    fn pack_refs_nested_tag_peel_line_points_at_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let commit = write_commit(&repo, vec![]);
        let inner_tag_body = format!(
            "object {}\ntype commit\ntagger T <t@example.com> 1 +0000\n\ninner\n",
            commit.to_hex()
        );
        let inner = repo
            .odb()
            .write(ObjectKind::Tag, inner_tag_body.as_bytes())
            .expect("inner tag");
        let outer_tag_body = format!(
            "object {}\ntype tag\ntagger T <t@example.com> 2 +0000\n\nouter\n",
            inner.to_hex()
        );
        let outer = repo
            .odb()
            .write(ObjectKind::Tag, outer_tag_body.as_bytes())
            .expect("outer tag");
        write_ref(repo.git_dir(), "refs/tags/outer", &outer).expect("tag ref");

        pack_refs_files(repo.git_dir(), repo.odb(), false).expect("pack");
        let body =
            fs::read_to_string(store::paths::packed_refs_path(repo.git_dir())).expect("packed");
        let mut lines = body.lines();
        let tag_line = lines
            .find(|l| l.contains("refs/tags/outer"))
            .expect("tag line");
        assert!(tag_line.starts_with(&outer.to_hex()));
        let peel_line = lines.find(|l| l.starts_with('^')).expect("peel line");
        assert!(
            peel_line.contains(&commit.to_hex()),
            "peel must reach commit through nested tags, got {peel_line}"
        );
    }

    #[test]
    fn pack_refs_tree_target_tag_has_peel_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let tag_body = format!(
            "object {}\ntype tree\ntagger T <t@example.com> 1 +0000\n\ntree tag\n",
            tree.to_hex()
        );
        let tag = repo
            .odb()
            .write(ObjectKind::Tag, tag_body.as_bytes())
            .expect("tag");
        write_ref(repo.git_dir(), "refs/tags/tree-tag", &tag).expect("tag ref");

        pack_refs_files(repo.git_dir(), repo.odb(), true).expect("pack");
        let body =
            fs::read_to_string(store::paths::packed_refs_path(repo.git_dir())).expect("packed");
        assert!(body.contains("refs/tags/tree-tag"));
        assert!(
            body.contains(&format!("^{}", tree.to_hex())),
            "expected peel to tree: {body}"
        );
    }

    #[test]
    fn write_packed_refs_file_survives_publish_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/heads")).expect("refs/heads");
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
        fs::write(git_dir.join("refs/heads/keep"), format!("{oid}\n")).expect("loose ref");
        fs::create_dir_all(git_dir.join("packed-refs")).expect("block packed-refs file");
        let err = write_packed_refs_file(
            git_dir,
            &[PackedRefRecord {
                refname: "refs/heads/keep".into(),
                oid,
                peeled: None,
            }],
        )
        .expect_err("publish should fail");
        assert!(matches!(err, Error::Io(_)));
        assert!(git_dir.join("refs/heads/keep").is_file());
    }

    #[test]
    fn write_packed_refs_file_replaces_loose_and_sorts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path();
        fs::create_dir_all(git_dir.join("refs/heads")).expect("refs/heads");
        let oid_a = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid a");
        let oid_b = ObjectId::from_hex("0000000000000000000000000000000000000002").expect("oid b");
        fs::write(git_dir.join("refs/heads/a-first"), format!("{oid_b}\n")).expect("loose ref");
        write_packed_refs_file(
            git_dir,
            &[
                PackedRefRecord {
                    refname: "refs/heads/z-last".into(),
                    oid: oid_b,
                    peeled: None,
                },
                PackedRefRecord {
                    refname: "refs/heads/a-first".into(),
                    oid: oid_a,
                    peeled: None,
                },
            ],
        )
        .expect("write packed-refs");
        assert!(!git_dir.join("refs/heads/a-first").exists());
        let body =
            fs::read_to_string(store::paths::packed_refs_path(git_dir)).expect("packed-refs");
        assert!(body.contains("refs/heads/a-first"));
        assert!(body.find("a-first").expect("a-first") < body.find("z-last").expect("z-last"));
    }
}
