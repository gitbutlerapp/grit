//! Reftable-backed [`super::RefStore`] over [`crate::reftable::ReftableStack`].

use std::borrow::ToOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::reftable::{
    read_write_options, reftable_storage_location, LogRecord, RefValue, ReftableError,
    ReftableStack, ReftableTransactionUpdate, TablesListLock, WriteOptions,
};
use crate::repo::init_repository;

use super::apply::{apply_reflog_rename, RefBatchState};
use super::backend::check_transaction_updates;
use super::error::RefStoreError;
use super::semantics::{
    apply_update_to_map, deref_reflog_target, format_reflog_identity, reflog_new_oid_before,
    reflog_old_oid_before, reftable_write_target,
};
use super::transaction::RefTransaction;
use super::types::RefReflogRename;
use super::{
    Expected, PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefUpdate,
};

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// On-disk ref storage using the reftable backend (`extensions.refStorage = reftable`).
///
/// # Atomicity
///
/// Refs live in one reftable stack per location: the common stack in the repository's git
/// directory, and in a linked worktree a second stack under `worktrees/<id>/` for `HEAD` and
/// other per-worktree refs. A transaction touching one stack commits as a single new table,
/// so readers see all of it or none of it.
///
/// A transaction touching both stacks (for example updating `HEAD` in a linked worktree,
/// which writes the branch in the common stack and logs `HEAD` in the worktree stack) takes
/// the `tables.list` lock of every stack in [`RefStore::prepare`], so no other writer can
/// interleave, and all checks pass before anything is written. Commit then writes one table
/// per stack, common stack first. These writes are not atomic together: if a later write
/// fails (an I/O error), [`PreparedRefTransaction::commit`] returns the error, the earlier
/// stacks keep their new table, and every remaining lock is released. For a `HEAD` update
/// this means the branch already points at the new commit with its reflog line, `HEAD` still
/// resolves to it through the symbolic ref, and only `HEAD`'s own reflog line is missing.
pub struct ReftableRefStore {
    git_dir: PathBuf,
    _keepalive: Option<tempfile::TempDir>,
    prepared: Mutex<Option<PreparedReftableBatch>>,
}

struct PreparedReftableBatch {
    stacks: Vec<LockedStackBatch>,
    reflog_renames: Vec<RefReflogRename>,
}

struct LockedStackBatch {
    _store_git_dir: PathBuf,
    stack: ReftableStack,
    lock: TablesListLock,
    updates: Vec<ReftableTransactionUpdate>,
    opts: WriteOptions,
}

impl fmt::Debug for ReftableRefStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReftableRefStore")
            .field("git_dir", &self.git_dir)
            .finish_non_exhaustive()
    }
}

impl ReftableRefStore {
    /// Open the reftable store for an existing repository git directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the reftable stack cannot be read.
    pub fn open(git_dir: PathBuf) -> Result<Self> {
        let store_git_dir = stack_root(&git_dir);
        let _ = ReftableStack::open(&store_git_dir)?;
        Ok(Self {
            git_dir,
            _keepalive: None,
            prepared: Mutex::new(None),
        })
    }

    /// Create a new ephemeral reftable repository (tests).
    ///
    /// # Errors
    ///
    /// Returns an error when initialization fails.
    pub fn open_ephemeral() -> Result<Self> {
        let root = tempfile::tempdir().map_err(Error::Io)?;
        init_repository(
            root.path(),
            false,
            "main",
            None,
            crate::ref_storage::RefStorageFormat::Reftable,
        )?;
        let git_dir = root.path().join(".git");
        let store = Self {
            git_dir: git_dir.clone(),
            _keepalive: Some(root),
            prepared: Mutex::new(None),
        };
        // Match [`MemoryRefStore`]: conformance seeds start from an empty namespace.
        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: "HEAD".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: super::RefUpdateFlags {
                    no_deref: true,
                    log_only: false,
                },
            })
            .map_err(|e| Error::Message(e.to_string()))?;
        store
            .prepare(txn)
            .map_err(|e| Error::Message(e.to_string()))?
            .commit()
            .map_err(|e| Error::Message(e.to_string()))?;
        Ok(store)
    }

    /// Write low-level reftable transaction updates (used by legacy `reftable_*` helpers).
    ///
    /// # Errors
    ///
    /// Propagates lock, CAS, and I/O failures from the underlying stack.
    pub fn write_reftable_updates(
        &self,
        updates: Vec<ReftableTransactionUpdate>,
    ) -> StoreResult<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut grouped: BTreeMap<PathBuf, Vec<ReftableTransactionUpdate>> = BTreeMap::new();
        for mut update in updates {
            let (store_git_dir, storage_refname) =
                reftable_storage_location(&self.git_dir, &update.refname);
            update.refname = storage_refname.clone();
            if let Some(log) = update.log.as_mut() {
                log.refname = storage_refname;
            }
            grouped.entry(store_git_dir).or_default().push(update);
        }
        for (store_git_dir, group) in grouped {
            let mut stack = ReftableStack::open(&store_git_dir)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            let opts = read_write_options(&store_git_dir);
            stack
                .write_transaction(group, &opts)
                .map_err(map_reftable_err)?;
        }
        Ok(())
    }

    fn read_raw_at(&self, refname: &str) -> Result<Option<RawRef>> {
        if refname == "HEAD" {
            return read_head_raw(&self.git_dir);
        }
        let (store_git_dir, storage_refname) = reftable_storage_location(&self.git_dir, refname);
        let stack = ReftableStack::open(&store_git_dir)?;
        Ok(stack
            .lookup_ref(&storage_refname)?
            .and_then(|rec| record_to_raw(&rec)))
    }

    /// Rewrite updates through a symbolic ref whose target lives in another stack (a linked
    /// worktree's `HEAD` pointing at a branch in the common stack) as two updates: the write
    /// of the target, checked and logged in its own stack, and a log-only line for the
    /// symbolic ref in its stack.
    ///
    /// The two halves commit to different stacks; see the atomicity notes on
    /// [`ReftableRefStore`] for what a failed second write leaves behind.
    fn split_cross_stack_symref_updates(
        &self,
        updates: Vec<RefUpdate>,
    ) -> StoreResult<Vec<RefUpdate>> {
        let common = stack_root(&self.git_dir);
        let mut out = Vec::with_capacity(updates.len());
        for update in updates {
            let (update_stack, _) = reftable_storage_location(&self.git_dir, &update.name);
            // Only per-worktree refs (HEAD, refs/worktree/*, …) can point across stacks.
            let target = if update_stack == common {
                None
            } else {
                self.cross_stack_deref_target(&update, &update_stack)?
            };
            let Some(target) = target else {
                out.push(update);
                continue;
            };
            let old_oid = match self.read_raw_at(&target) {
                Ok(Some(RawRef::Direct(oid))) => oid,
                Ok(_) => ObjectId::zero(),
                Err(e) => return Err(RefStoreError::Corrupt(e.to_string())),
            };
            let new_oid = match &update.new_value {
                Some(RawRef::Direct(oid)) => *oid,
                _ => ObjectId::zero(),
            };
            let (symref_expected, target_expected) = match &update.expected {
                Expected::Symref(_) => (update.expected.clone(), Expected::Any),
                other => (Expected::Any, other.clone()),
            };
            out.push(RefUpdate {
                name: update.name.clone(),
                new_value: update.new_value.clone(),
                expected: symref_expected,
                reflog: update.reflog.clone().map(|log| super::ReflogUpdate {
                    old_oid: log.old_oid.or(Some(old_oid)),
                    new_oid: log.new_oid.or(Some(new_oid)),
                    ..log
                }),
                flags: super::RefUpdateFlags {
                    log_only: true,
                    no_deref: true,
                },
            });
            out.push(RefUpdate {
                name: target,
                new_value: update.new_value,
                expected: target_expected,
                reflog: update.reflog,
                flags: super::RefUpdateFlags {
                    log_only: update.flags.log_only,
                    no_deref: true,
                },
            });
        }
        Ok(out)
    }

    /// End of the symbolic chain `update` writes through, when it lies outside `update_stack`.
    fn cross_stack_deref_target(
        &self,
        update: &RefUpdate,
        update_stack: &Path,
    ) -> StoreResult<Option<String>> {
        if update.flags.no_deref || matches!(update.new_value, Some(RawRef::Symbolic(_))) {
            return Ok(None);
        }
        let read = |name: &str| {
            self.read_raw_at(name)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))
        };
        let Some(RawRef::Symbolic(mut target)) = read(&update.name)? else {
            return Ok(None);
        };
        for _ in 0..crate::refs::SYMREF_MAXDEPTH {
            match read(&target)? {
                Some(RawRef::Symbolic(next)) => target = next,
                _ => {
                    let (target_stack, _) = reftable_storage_location(&self.git_dir, &target);
                    return Ok((target_stack != update_stack).then_some(target));
                }
            }
        }
        Err(RefStoreError::SymrefLoop)
    }

    fn merged_refs_from_locked_stack(
        git_dir: &Path,
        stack_git_dir: &Path,
        stack: &ReftableStack,
    ) -> Result<BTreeMap<String, RawRef>> {
        let mut map = BTreeMap::new();
        for rec in stack.read_refs()? {
            if let Some(raw) = record_to_raw(&rec) {
                map.insert(rec.name, raw);
            }
        }
        let (head_git_dir, _) = reftable_storage_location(git_dir, "HEAD");
        if head_git_dir != stack_git_dir {
            // A shared-stack snapshot must use this worktree's HEAD, not the main one's.
            map.remove("HEAD");
            if let Some(head) = read_head_raw(git_dir)? {
                map.insert("HEAD".to_owned(), head);
            }
        }
        Ok(map)
    }
}

fn read_reflog_entries_from_stack(git_dir: &Path, refname: &str) -> Result<Vec<ReflogEntry>> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let stack = ReftableStack::open(&store_git_dir)?;
    let logs = stack.read_logs_for_ref(&storage_refname)?;
    let mut entries = Vec::new();
    for log in logs {
        // `tz_offset` holds the `HHMM` digits as a decimal number (Git's on-disk form).
        let tz_sign = if log.tz_offset >= 0 { '+' } else { '-' };
        let tz_abs = log.tz_offset.unsigned_abs();
        let tz_hours = tz_abs / 100;
        let tz_mins = tz_abs % 100;
        let identity = format!(
            "{} <{}> {} {}{:02}{:02}",
            log.name, log.email, log.time_seconds, tz_sign, tz_hours, tz_mins
        );
        let message = log
            .message
            .strip_suffix('\n')
            .map(ToOwned::to_owned)
            .unwrap_or(log.message);
        entries.push(ReflogEntry {
            old_oid: log.old_id,
            new_oid: log.new_id,
            identity,
            message,
        });
    }
    entries.reverse();
    Ok(entries)
}

/// HEAD is authoritative in this worktree's stack; its loose file is only a discovery stub.
fn read_head_raw(git_dir: &Path) -> Result<Option<RawRef>> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, "HEAD");
    let stack = ReftableStack::open(&store_git_dir)?;
    Ok(stack
        .lookup_ref(&storage_refname)?
        .and_then(|rec| record_to_raw(&rec)))
}

fn empty_reflog_markers_path(git_dir: &Path) -> PathBuf {
    git_dir.join("reftable").join("empty-reflogs")
}

fn read_empty_reflog_markers(git_dir: &Path) -> BTreeSet<String> {
    fs::read_to_string(empty_reflog_markers_path(git_dir))
        .map(|content| {
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn write_empty_reflog_markers(git_dir: &Path, markers: &BTreeSet<String>) -> Result<()> {
    let path = empty_reflog_markers_path(git_dir);
    let content = markers.iter().cloned().collect::<Vec<_>>().join("\n");
    fs::write(
        path,
        if content.is_empty() {
            content
        } else {
            content + "\n"
        },
    )
    .map_err(Error::Io)
}

fn stack_root(git_dir: &Path) -> PathBuf {
    crate::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf())
}

fn record_to_raw(rec: &crate::reftable::RefRecord) -> Option<RawRef> {
    match rec.value {
        RefValue::Deletion => None,
        RefValue::Val1(oid) | RefValue::Val2(oid, _) => Some(RawRef::Direct(oid)),
        RefValue::Symref(ref target) => Some(RawRef::Symbolic(target.clone())),
    }
}

fn map_reftable_err(err: Error) -> RefStoreError {
    match err {
        Error::Reftable(ReftableError::Locked { .. }) => RefStoreError::LockHeld {
            name: "refs".to_owned(),
        },
        Error::Reftable(ReftableError::ExpectedMismatch {
            refname,
            expected,
            actual,
        }) => RefStoreError::ExpectedMismatch {
            name: refname,
            expected: Expected::Oid(expected),
            actual: actual.map(RawRef::Direct),
        },
        other => RefStoreError::Corrupt(other.to_string()),
    }
}

fn log_record_from_update(
    git_dir: &Path,
    storage_refname: &str,
    old_oid: ObjectId,
    new_oid: ObjectId,
    log: &super::ReflogUpdate,
) -> Result<LogRecord> {
    let (store_git_dir, _) = reftable_storage_location(git_dir, storage_refname);
    let opts = read_write_options(&store_git_dir);
    let identity = format_reflog_identity(&log.identity, log.time);
    let (name, email, time_secs, tz) = crate::reftable::parse_identity_string(&identity);
    Ok(LogRecord {
        refname: storage_refname.to_owned(),
        update_index: 0,
        old_id: crate::reftable::widen_oid_to(old_oid, opts.hash_size),
        new_id: crate::reftable::widen_oid_to(new_oid, opts.hash_size),
        name,
        email,
        time_seconds: time_secs,
        tz_offset: tz,
        message: log.message.clone(),
    })
}

fn build_reftable_updates_for_stack(
    git_dir: &Path,
    refs: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
) -> StoreResult<Vec<ReftableTransactionUpdate>> {
    let mut trial = refs.clone();
    let mut ref_values: BTreeMap<String, Option<RefValue>> = BTreeMap::new();
    let mut expected_old_for_ref: BTreeMap<String, Option<ObjectId>> = BTreeMap::new();
    let mut log_updates: Vec<ReftableTransactionUpdate> = Vec::new();

    for update in updates {
        let target = reftable_write_target(&trial, git_dir, update)?;
        let value = target.value.clone();
        let old_oid = reflog_old_oid_before(&trial, refs, update);
        let new_oid = reflog_new_oid_before(&trial, refs, update);
        let (_, log_refname) = reftable_storage_location(git_dir, &update.name);
        if let Some(log) = update.reflog.as_ref() {
            let mut log_refnames = vec![log_refname];
            if let Some(target_name) = deref_reflog_target(&trial, update) {
                let (target_store, target_storage) =
                    reftable_storage_location(git_dir, &target_name);
                let (update_store, _) = reftable_storage_location(git_dir, &update.name);
                if target_store == update_store {
                    log_refnames.push(target_storage);
                }
            }
            for refname in log_refnames {
                let record = log_record_from_update(git_dir, &refname, old_oid, new_oid, log)
                    .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
                log_updates.push(ReftableTransactionUpdate {
                    refname,
                    value: None,
                    log: Some(record),
                    expected_old: None,
                });
            }
        }
        if value.is_some() {
            let expected_old = match &update.expected {
                Expected::Oid(oid) => Some(*oid),
                _ => None,
            };
            ref_values.insert(target.storage_refname.clone(), value);
            expected_old_for_ref.insert(target.storage_refname.clone(), expected_old);
        }
        apply_update_to_map(&mut trial, refs, update)?;
    }

    let mut out = Vec::new();
    for (refname, value) in ref_values {
        out.push(ReftableTransactionUpdate {
            refname: refname.clone(),
            value,
            log: None,
            expected_old: expected_old_for_ref.remove(&refname).flatten(),
        });
    }
    out.extend(log_updates);
    Ok(out)
}

impl RefStore for ReftableRefStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Reftable
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        self.read_raw_at(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        if "HEAD".starts_with(prefix) {
            if let Some(value) = read_head_raw(&self.git_dir)? {
                let peeled = match &value {
                    RawRef::Direct(_) => None,
                    RawRef::Symbolic(_) => self.resolve("HEAD").ok(),
                };
                let entry = RefEntry {
                    name: "HEAD".to_owned(),
                    value,
                    peeled,
                };
                if f(&entry).is_break() {
                    return Ok(());
                }
            }
        }
        let store_git_dir = stack_root(&self.git_dir);
        let stack = ReftableStack::open(&store_git_dir)?;
        let mut names: Vec<String> = stack
            .read_refs()?
            .into_iter()
            .filter_map(|rec| {
                if rec.name != "HEAD"
                    && record_to_raw(&rec).is_some()
                    && (rec.name.starts_with(prefix)
                        || (prefix.ends_with('/') && rec.name == prefix.trim_end_matches('/')))
                {
                    Some(rec.name)
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        for name in names {
            let Some(value) = self.read_raw_at(&name)? else {
                continue;
            };
            let peeled = match &value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(_) => self.resolve(&name).ok(),
            };
            let entry = RefEntry {
                name,
                value,
                peeled,
            };
            if f(&entry).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        txn.validate()?;
        let (updates, reflog_renames, _case_only_renames) = txn.into_parts();
        let mut guard_state = self.prepared.lock().unwrap_or_else(|e| e.into_inner());
        if guard_state.is_some() {
            return Err(RefStoreError::LockHeld {
                name: "refs".to_owned(),
            });
        }

        let updates = self.split_cross_stack_symref_updates(updates)?;
        let mut grouped: BTreeMap<PathBuf, Vec<RefUpdate>> = BTreeMap::new();
        for update in updates {
            let (store_git_dir, _) = reftable_storage_location(&self.git_dir, &update.name);
            grouped.entry(store_git_dir).or_default().push(update);
        }

        let mut stacks = Vec::new();
        for (store_git_dir, group) in grouped {
            let mut stack = ReftableStack::open(&store_git_dir)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            let lock = stack
                .acquire_tables_list_lock_for_store()
                .map_err(map_reftable_err)?;
            stack.reload_table_names();
            let refs = Self::merged_refs_from_locked_stack(&self.git_dir, &store_git_dir, &stack)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            let snapshot = RefBatchState {
                refs,
                reflogs: BTreeMap::new(),
            };
            check_transaction_updates(&snapshot, &group)?;
            let refs = snapshot.refs;
            let rt_updates = build_reftable_updates_for_stack(&self.git_dir, &refs, &group)?;
            let opts = read_write_options(&store_git_dir);
            stacks.push(LockedStackBatch {
                _store_git_dir: store_git_dir,
                stack,
                lock,
                updates: rt_updates,
                opts,
            });
        }

        *guard_state = Some(PreparedReftableBatch {
            stacks,
            reflog_renames,
        });

        Ok(Box::new(ReftablePrepared {
            store: self,
            committed: false,
        }))
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        let (store_git_dir, storage_refname) = reftable_storage_location(&self.git_dir, name);
        if read_empty_reflog_markers(&store_git_dir).contains(&storage_refname) {
            return Ok(true);
        }
        let stack = ReftableStack::open(&store_git_dir)?;
        Ok(!stack.read_logs_for_ref(&storage_refname)?.is_empty())
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let entries = read_reflog_entries_from_stack(&self.git_dir, name)?;
        if reverse {
            for entry in entries.iter().rev() {
                if f(entry).is_break() {
                    break;
                }
            }
        } else {
            for entry in &entries {
                if f(entry).is_break() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        let (store_git_dir, storage_refname) = reftable_storage_location(&self.git_dir, name);
        let mut markers = read_empty_reflog_markers(&store_git_dir);
        markers.insert(storage_refname);
        write_empty_reflog_markers(&store_git_dir, &markers)
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        let (store_git_dir, storage_refname) = reftable_storage_location(&self.git_dir, name);
        let mut markers = read_empty_reflog_markers(&store_git_dir);
        markers.remove(&storage_refname);
        write_empty_reflog_markers(&store_git_dir, &markers)?;
        let mut stack = ReftableStack::open(&store_git_dir)?;
        stack.replace_logs_for_ref(&storage_refname, &[])
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        let (store_git_dir, storage_refname) = reftable_storage_location(&self.git_dir, name);
        let mut markers = read_empty_reflog_markers(&store_git_dir);
        if entries.is_empty() {
            markers.insert(storage_refname.clone());
        } else {
            markers.remove(&storage_refname);
        }
        write_empty_reflog_markers(&store_git_dir, &markers)?;
        let mut stack = ReftableStack::open(&store_git_dir)?;
        stack.replace_logs_for_ref(&storage_refname, &entries)
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let store_git_dir = stack_root(&self.git_dir);
        let stack = ReftableStack::open(&store_git_dir)?;
        let mut refs = read_empty_reflog_markers(&store_git_dir);
        for log in stack.read_all_logs()? {
            refs.insert(log.refname);
        }
        for name in refs {
            if f(&name).is_break() {
                break;
            }
        }
        Ok(())
    }
}

fn commit_reflog_rename_on_stack(
    git_dir: &Path,
    stack: &mut ReftableStack,
    rename: &RefReflogRename,
) -> StoreResult<()> {
    let entries = read_reflog_entries_from_stack(git_dir, &rename.from)
        .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
    let mut batch = RefBatchState {
        refs: BTreeMap::new(),
        reflogs: BTreeMap::from([(rename.from.clone(), entries)]),
    };
    apply_reflog_rename(&mut batch, rename)?;
    let new_entries = batch.reflogs.get(&rename.to).cloned().unwrap_or_default();
    let (_, from_storage) = reftable_storage_location(git_dir, &rename.from);
    let (_, to_storage) = reftable_storage_location(git_dir, &rename.to);
    stack
        .replace_logs_for_ref(&to_storage, &new_entries)
        .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
    stack
        .replace_logs_for_ref(&from_storage, &[])
        .map_err(|e| RefStoreError::Corrupt(e.to_string()))
}

struct ReftablePrepared<'a> {
    store: &'a ReftableRefStore,
    committed: bool,
}

impl PreparedRefTransaction for ReftablePrepared<'_> {
    fn commit(mut self: Box<Self>) -> StoreResult<()> {
        let mut guard_state = self
            .store
            .prepared
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(batch) = guard_state.take() else {
            return Ok(());
        };
        for mut stack_batch in batch.stacks {
            if !stack_batch.updates.is_empty() {
                stack_batch
                    .stack
                    .write_transaction_under_lock(
                        &mut stack_batch.lock,
                        stack_batch.updates,
                        &stack_batch.opts,
                    )
                    .map_err(map_reftable_err)?;
            }
            for rename in &batch.reflog_renames {
                let (store_git_dir, _) =
                    reftable_storage_location(&self.store.git_dir, &rename.from);
                if store_git_dir != stack_batch._store_git_dir {
                    continue;
                }
                commit_reflog_rename_on_stack(&self.store.git_dir, &mut stack_batch.stack, rename)
                    .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            }
        }
        self.committed = true;
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> StoreResult<()> {
        let mut guard_state = self
            .store
            .prepared
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard_state.take();
        self.committed = true;
        Ok(())
    }
}

impl Drop for ReftablePrepared<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut guard_state) = self.store.prepared.lock() {
            guard_state.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_reftable_err_keeps_lock_and_cas_failures_typed() {
        let locked = map_reftable_err(
            ReftableError::Locked {
                lock_path: PathBuf::from("reftable/tables.list.lock"),
            }
            .into(),
        );
        assert!(matches!(locked, RefStoreError::LockHeld { name } if name == "refs"));

        let expected = ObjectId::from_bytes(&[0x31; 20]).expect("oid");
        let actual = ObjectId::from_bytes(&[0x32; 20]).expect("oid");
        let mismatch = map_reftable_err(
            ReftableError::ExpectedMismatch {
                refname: "refs/heads/main".to_owned(),
                expected,
                actual: Some(actual),
            }
            .into(),
        );
        match mismatch {
            RefStoreError::ExpectedMismatch {
                name,
                expected: Expected::Oid(want),
                actual: Some(RawRef::Direct(found)),
            } => {
                assert_eq!(name, "refs/heads/main");
                assert_eq!(want, expected);
                assert_eq!(found, actual);
            }
            other => panic!("unexpected {other:?}"),
        }

        let corrupt = map_reftable_err(ReftableError::BadMagic.into());
        assert!(matches!(corrupt, RefStoreError::Corrupt(msg) if msg.contains("bad magic")));
    }
    #[test]
    fn head_values_use_stack_transactions_and_ignore_loose_files() {
        let store = ReftableRefStore::open_ephemeral().expect("store");
        let update = |value, expected| {
            RefTransaction::new()
                .update(RefUpdate {
                    name: "HEAD".into(),
                    new_value: value,
                    expected,
                    reflog: None,
                    flags: super::super::RefUpdateFlags {
                        no_deref: true,
                        log_only: false,
                    },
                })
                .expect("transaction")
        };
        let symbolic = RawRef::Symbolic("refs/heads/topic".into());
        store
            .prepare(update(Some(symbolic.clone()), Expected::Missing))
            .expect("prepare")
            .commit()
            .expect("commit");
        let stub = fs::read(store.git_dir.join("HEAD")).expect("stub");
        assert_eq!(stub, b"ref: refs/heads/.invalid\n");
        assert_eq!(store.read_raw("HEAD").expect("read"), Some(symbolic));
        let mut names = Vec::new();
        store
            .for_each_ref("", &mut |entry| {
                names.push(entry.name.clone());
                ControlFlow::Continue(())
            })
            .expect("list");
        assert_eq!(names, ["HEAD"]);
        let oid = ObjectId::from_bytes(&[0x31; 20]).expect("oid");
        let detached = RawRef::Direct(oid);
        let txn = update(
            Some(detached.clone()),
            Expected::Symref("refs/heads/topic".into()),
        );
        store
            .prepare(txn.clone())
            .expect("prepare")
            .abort()
            .expect("abort");
        assert!(matches!(
            store.read_raw("HEAD").expect("read"),
            Some(RawRef::Symbolic(_))
        ));
        store
            .prepare(txn)
            .expect("prepare")
            .commit()
            .expect("commit");
        fs::write(store.git_dir.join("HEAD"), "ref: refs/heads/stale\n").expect("stale HEAD");
        assert_eq!(
            store.read_raw("HEAD").expect("read"),
            Some(detached.clone())
        );
        assert_eq!(
            crate::reftable::reftable_resolve_ref(&store.git_dir, "HEAD").expect("resolve"),
            oid
        );
        assert!(store
            .prepare(update(None, Expected::Oid(ObjectId::zero())))
            .is_err());
        assert_eq!(store.read_raw("HEAD").expect("read"), Some(detached));
        store
            .prepare(update(None, Expected::Oid(oid)))
            .expect("prepare")
            .commit()
            .expect("commit");
        assert_eq!(store.read_raw("HEAD").expect("deleted"), None);
        ReftableStack::open(&store.git_dir)
            .expect("stack")
            .compact()
            .expect("compact");
        assert_eq!(
            store.read_raw("HEAD").expect("deleted after compaction"),
            None
        );
    }
}
