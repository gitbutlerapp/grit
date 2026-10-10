//! Reftable-backed [`super::RefStore`] over [`crate::reftable::ReftableStack`].

use std::borrow::ToOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::reftable::{
    read_write_options, reftable_storage_location, LogRecord, RefValue, ReftableStack,
    ReftableTransactionUpdate, TablesListLock, WriteOptions,
};
use crate::repo::init_repository;

use super::error::RefStoreError;
use super::semantics::{
    apply_update_to_map, format_reflog_identity, reflog_new_oid_before, reflog_old_oid_before,
    reftable_write_target, simulate_batch_apply,
};
use super::transaction::RefTransaction;
use super::validation::verify_create_conflicts;
use super::{
    Expected, PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefUpdate,
};

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// On-disk ref storage using the reftable backend (`extensions.refStorage = reftable`).
pub struct ReftableRefStore {
    git_dir: PathBuf,
    _keepalive: Option<tempfile::TempDir>,
    prepared: Mutex<Option<PreparedReftableBatch>>,
}

struct PreparedReftableBatch {
    logical_updates: Vec<RefUpdate>,
    stacks: Vec<LockedStackBatch>,
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
                name: "refs/heads/main".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: super::RefUpdateFlags::default(),
            })
            .map_err(|e| Error::Message(e.to_string()))?;
        store
            .prepare(txn)
            .map_err(|e| Error::Message(e.to_string()))?
            .commit()
            .map_err(|e| Error::Message(e.to_string()))?;
        let _ = fs::remove_file(git_dir.join("HEAD"));
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
            return read_head_file_raw(&self.git_dir);
        }
        let (store_git_dir, storage_refname) = reftable_storage_location(&self.git_dir, refname);
        let stack = ReftableStack::open(&store_git_dir)?;
        Ok(stack
            .lookup_ref(&storage_refname)?
            .and_then(|rec| record_to_raw(&rec)))
    }

    fn merged_refs_from_locked_stack(
        git_dir: &Path,
        stack: &ReftableStack,
    ) -> Result<BTreeMap<String, RawRef>> {
        let mut map = BTreeMap::new();
        for rec in stack.read_refs()? {
            if let Some(raw) = record_to_raw(&rec) {
                map.insert(rec.name, raw);
            }
        }
        if let Some(head) = read_head_file_raw(git_dir)? {
            map.insert("HEAD".to_owned(), head);
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
        let tz_sign = if log.tz_offset >= 0 { '+' } else { '-' };
        let tz_abs = log.tz_offset.unsigned_abs();
        let tz_hours = tz_abs / 60;
        let tz_mins = tz_abs % 60;
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

fn read_head_file_raw(git_dir: &Path) -> Result<Option<RawRef>> {
    let head_path = git_dir.join("HEAD");
    let content = match fs::read_to_string(&head_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let content = content.trim();
    if let Some(target) = content.strip_prefix("ref: ") {
        return Ok(Some(RawRef::Symbolic(target.trim().to_owned())));
    }
    if content.len() >= 40 && content.chars().all(|c| c.is_ascii_hexdigit()) {
        let oid: ObjectId = content.parse()?;
        return Ok(Some(RawRef::Direct(oid)));
    }
    Err(Error::InvalidRef(format!("invalid HEAD: {content}")))
}

fn apply_head_file_side_effect(git_dir: &Path, update: &RefUpdate) -> Result<()> {
    if update.name != "HEAD" || update.flags.log_only {
        return Ok(());
    }
    let head_path = git_dir.join("HEAD");
    match &update.new_value {
        Some(RawRef::Symbolic(target)) => {
            fs::write(head_path, format!("ref: {target}\n")).map_err(Error::Io)?;
        }
        Some(RawRef::Direct(oid)) => {
            let current = read_head_file_raw(git_dir)?;
            let deref_sym = matches!(current, Some(RawRef::Symbolic(_))) && !update.flags.no_deref;
            if !deref_sym {
                fs::write(head_path, format!("{oid}\n")).map_err(Error::Io)?;
            }
        }
        None if update.flags.no_deref => {
            fs::write(head_path, "ref: refs/heads/.invalid\n").map_err(Error::Io)?;
        }
        None => {}
    }
    Ok(())
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
        Error::InvalidRef(msg) if msg.contains("cannot lock references") => {
            RefStoreError::LockHeld {
                name: "refs".to_owned(),
            }
        }
        Error::Message(msg) if msg.contains("ref transaction rejected") => {
            RefStoreError::Corrupt(msg)
        }
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
        let mut value = target.value.clone();
        if update.name == "HEAD" {
            value = head_stack_value(git_dir, update, value);
        }
        let old_oid = reflog_old_oid_before(&trial, refs, update);
        let new_oid = reflog_new_oid_before(&trial, refs, update);
        let (_, log_refname) = reftable_storage_location(git_dir, &update.name);
        if let Some(log) = update.reflog.as_ref() {
            let log = log_record_from_update(git_dir, &log_refname, old_oid, new_oid, log)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            log_updates.push(ReftableTransactionUpdate {
                refname: log_refname,
                value: None,
                log: Some(log),
                expected_old: None,
            });
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

fn head_stack_value(
    git_dir: &Path,
    update: &RefUpdate,
    value: Option<RefValue>,
) -> Option<RefValue> {
    if update.name != "HEAD" {
        return value;
    }
    match &update.new_value {
        Some(RawRef::Symbolic(_)) => None,
        Some(RawRef::Direct(_)) => {
            let current = read_head_file_raw(git_dir).ok().flatten();
            if matches!(current, Some(RawRef::Symbolic(_))) && !update.flags.no_deref {
                value
            } else {
                None
            }
        }
        None if update.flags.no_deref => None,
        None => value,
    }
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
            if let Some(value) = read_head_file_raw(&self.git_dir)? {
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
                if record_to_raw(&rec).is_some()
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
        RefTransaction::validate_no_duplicates(txn.updates())?;
        let updates = txn.into_updates();
        let mut guard_state = self.prepared.lock().unwrap_or_else(|e| e.into_inner());
        if guard_state.is_some() {
            return Err(RefStoreError::LockHeld {
                name: "refs".to_owned(),
            });
        }

        let logical_updates = updates.clone();
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
            let refs = Self::merged_refs_from_locked_stack(&self.git_dir, &stack)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            verify_create_conflicts(&refs, &group)?;
            simulate_batch_apply(&refs, &group)?;
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
            logical_updates,
            stacks,
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
                        &stack_batch.lock,
                        stack_batch.updates,
                        &stack_batch.opts,
                    )
                    .map_err(map_reftable_err)?;
                stack_batch
                    .stack
                    .maybe_auto_compact(&stack_batch.opts)
                    .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            }
        }
        for update in &batch.logical_updates {
            apply_head_file_side_effect(&self.store.git_dir, update)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
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
