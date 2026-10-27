//! Reftable format — binary reference storage.
//!
//! Implements the [reftable file format](https://git-scm.com/docs/reftable)
//! for efficient, sorted reference storage.  A reftable file contains
//! ref blocks (sorted ref records with prefix compression), optional log
//! blocks (reflog entries), optional index blocks, and a footer.
//!
//! # Architecture
//!
//! - [`ReftableWriter`] writes a single `.ref` (or `.log`) reftable file.
//! - [`ReftableReader`] reads and searches a single reftable file.
//! - [`ReftableStack`] manages the `tables.list` stack, providing a
//!   merged view of all tables and auto-compaction on writes.
//!
//! # On-disk layout
//!
//! ```text
//! first_block { header, first_ref_block }
//! ref_block*
//! ref_index?
//! obj_block*    (not yet implemented)
//! obj_index?    (not yet implemented)
//! log_block*
//! log_index?
//! footer
//! ```

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::refs::store::RefStoreError;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic bytes at the start of every reftable file.
const REFTABLE_MAGIC: &[u8; 4] = b"REFT";

/// File header size (version 1): magic(4) + version(1) + block_size(3)
/// + min_update_index(8) + max_update_index(8) = 24 bytes.
const HEADER_SIZE: usize = 24;

/// Footer size for version 1.
const FOOTER_V1_SIZE: usize = 68;

/// Block type: ref block.
const BLOCK_TYPE_REF: u8 = b'r';
/// Block type: index block.
const BLOCK_TYPE_INDEX: u8 = b'i';
/// Block type: log block (zlib-compressed).
const BLOCK_TYPE_LOG: u8 = b'g';
/// Block type: object index block.
const BLOCK_TYPE_OBJ: u8 = b'o';

/// Value types encoded in the low 3 bits of the suffix_length varint.
const VALUE_DELETION: u8 = 0;
const VALUE_ONE_OID: u8 = 1;
const VALUE_TWO_OID: u8 = 2;
const VALUE_SYMREF: u8 = 3;

/// Hash size (SHA-1).
const HASH_SIZE: usize = 20;

/// Default block size when none is configured (4 KiB).
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// How many records between restart points.
const RESTART_INTERVAL: usize = 16;

/// A reftable failure: malformed table bytes, a locked stack, or a rejected update.
///
/// Wrapped by [`Error::Reftable`]. Corruption variants describe what was wrong
/// with the bytes; [`Self::Locked`] and [`Self::ExpectedMismatch`] are the
/// conditions callers usually want to react to (retry, or re-read and redo).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReftableError {
    /// The file is shorter than its header and footer require.
    #[error("reftable: file is too small ({len} bytes)")]
    TooSmall {
        /// Number of bytes available.
        len: usize,
    },

    /// The header or footer does not start with the `REFT` magic.
    #[error("reftable: bad magic bytes")]
    BadMagic,

    /// The header names a format version other than 1 (SHA-1) or 2 (SHA-256).
    #[error("reftable: unsupported format version {version}")]
    UnsupportedVersion {
        /// Version byte from the header.
        version: u8,
    },

    /// The footer repeats a different version than the header.
    #[error("reftable: footer version {footer} does not match header version {header}")]
    FooterVersionMismatch {
        /// Version byte from the header.
        header: u8,
        /// Version byte from the footer.
        footer: u8,
    },

    /// The footer's CRC-32 does not match its contents.
    #[error("reftable: footer checksum mismatch (stored {stored:08x}, computed {computed:08x})")]
    FooterChecksumMismatch {
        /// CRC-32 recorded in the footer.
        stored: u32,
        /// CRC-32 computed over the footer bytes.
        computed: u32,
    },

    /// The data ends inside a field.
    #[error("reftable: truncated {field}")]
    Truncated {
        /// Field that was cut short.
        field: ReftableField,
    },

    /// A text field is not valid UTF-8.
    #[error("reftable: {field} is not valid UTF-8")]
    InvalidUtf8 {
        /// Field holding the invalid bytes.
        field: ReftableField,
    },

    /// A prefix-compressed key claims more shared bytes than the previous key has.
    #[error("reftable: {field} prefix is longer than the previous key")]
    PrefixTooLong {
        /// Key whose prefix length is out of range.
        field: ReftableField,
    },

    /// A ref record uses a value type outside the format's four kinds.
    #[error("reftable: unknown ref value type {value_type}")]
    UnknownValueType {
        /// The 3-bit value type found in the record.
        value_type: u8,
    },

    /// A block's length points outside the file.
    #[error("reftable: invalid block length")]
    InvalidBlockLength,

    /// A block's restart table is out of range or overlaps its records.
    #[error("reftable: invalid restart table")]
    InvalidRestartTable,

    /// A log record key lacks the `refname NUL update_index` layout.
    #[error("reftable: malformed log record key")]
    InvalidLogKey,

    /// `tables.list` names a table that is not part of the loaded stack.
    #[error("reftable: tables.list names missing table {name}")]
    MissingTable {
        /// Table file name from `tables.list`.
        name: String,
    },

    /// [`ReftableWriter::add_ref`] was called out of name order.
    #[error("reftable: refs must be added in sorted order ('{next}' after '{previous}')")]
    UnsortedRefs {
        /// Name of the ref added before.
        previous: String,
        /// Name of the ref that did not sort after it.
        next: String,
    },

    /// A record does not fit in an empty block of the configured size.
    #[error("reftable: record does not fit in a block")]
    EntryTooLarge,

    /// A record with an empty key was written.
    #[error("reftable: record has an empty key")]
    EmptyRecordKey,

    /// Another writer holds `tables.list.lock`.
    #[error("reftable: stack is locked by another writer ({})", lock_path.display())]
    Locked {
        /// Path of the held lock file.
        lock_path: PathBuf,
    },

    /// The `tables.list.lock` guard was used after its lock had been committed.
    #[error("reftable: tables.list lock was already released")]
    LockReleased,

    /// A compare-and-swap update found a different value than it required.
    #[error(
        "reftable: '{refname}' expected {expected} but found {}",
        actual.map_or_else(|| "<absent>".to_owned(), |oid| oid.to_hex())
    )]
    ExpectedMismatch {
        /// Ref the update targeted.
        refname: String,
        /// Object id the update required.
        expected: ObjectId,
        /// Object id found, or `None` when the ref does not exist.
        actual: Option<ObjectId>,
    },
}

/// A field of a reftable file, named by [`ReftableError`] variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReftableField {
    /// A variable-length integer.
    Varint,
    /// The 4-byte block header (type and length).
    BlockHeader,
    /// The key of an index record.
    IndexKey,
    /// A ref name (in a ref record or a log key).
    RefName,
    /// The object id of a one-value ref record.
    ObjectId,
    /// The two object ids of a peeled ref record or a log record.
    ObjectIdPair,
    /// The target of a symbolic ref record.
    SymrefTarget,
    /// The key of a log record.
    LogKey,
    /// The committer name of a log record.
    LogName,
    /// The committer email of a log record.
    LogEmail,
    /// The timezone offset of a log record.
    LogTimezone,
    /// The message of a log record.
    LogMessage,
    /// The file footer.
    Footer,
}

impl std::fmt::Display for ReftableField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Varint => "varint",
            Self::BlockHeader => "block header",
            Self::IndexKey => "index key",
            Self::RefName => "ref name",
            Self::ObjectId => "object id",
            Self::ObjectIdPair => "object id pair",
            Self::SymrefTarget => "symref target",
            Self::LogKey => "log key",
            Self::LogName => "log name",
            Self::LogEmail => "log email",
            Self::LogTimezone => "log timezone offset",
            Self::LogMessage => "log message",
            Self::Footer => "footer",
        })
    }
}

#[cold]
#[inline(never)]
fn truncated(field: ReftableField) -> Error {
    ReftableError::Truncated { field }.into()
}

#[cold]
#[inline(never)]
fn invalid_utf8(field: ReftableField) -> Error {
    ReftableError::InvalidUtf8 { field }.into()
}

#[cold]
#[inline(never)]
fn corrupt(err: ReftableError) -> Error {
    err.into()
}

// ---------------------------------------------------------------------------
// Varint encoding (Git pack-style)
// ---------------------------------------------------------------------------

/// Encode a u64 as a varint into `out`. Returns number of bytes written.
fn put_varint(mut val: u64, out: &mut Vec<u8>) -> usize {
    // First, collect 7-bit groups.
    let mut buf = [0u8; 10];
    let mut i = 0;
    buf[i] = (val & 0x7f) as u8;
    i += 1;
    val >>= 7;
    while val > 0 {
        val -= 1;
        buf[i] = (val & 0x7f) as u8;
        i += 1;
        val >>= 7;
    }
    // Write in reverse, with continuation bits.
    let len = i;
    for j in (1..len).rev() {
        out.push(buf[j] | 0x80);
    }
    out.push(buf[0]);
    len
}

/// Decode a varint from `data` starting at `pos`. Returns (value, new_pos).
fn get_varint(data: &[u8], mut pos: usize) -> Result<(u64, usize)> {
    if pos >= data.len() {
        return Err(truncated(ReftableField::Varint));
    }
    let mut val = (data[pos] & 0x7f) as u64;
    while data[pos] & 0x80 != 0 {
        pos += 1;
        if pos >= data.len() {
            return Err(truncated(ReftableField::Varint));
        }
        val = ((val + 1) << 7) | (data[pos] & 0x7f) as u64;
    }
    Ok((val, pos + 1))
}

// ---------------------------------------------------------------------------
// Ref record types
// ---------------------------------------------------------------------------

/// A single reference record as stored in a reftable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefValue {
    /// Deletion tombstone (value_type 0x0).
    Deletion,
    /// A direct ref pointing to one OID (value_type 0x1).
    Val1(ObjectId),
    /// An annotated tag: value + peeled target (value_type 0x2).
    Val2(ObjectId, ObjectId),
    /// A symbolic reference (value_type 0x3).
    Symref(String),
}

/// A decoded ref record.
#[derive(Debug, Clone)]
pub struct RefRecord {
    /// Full reference name.
    pub name: String,
    /// Update index (absolute).
    pub update_index: u64,
    /// The value.
    pub value: RefValue,
}

/// A decoded log record.
#[derive(Debug, Clone)]
pub struct LogRecord {
    /// Reference name.
    pub refname: String,
    /// Update index.
    pub update_index: u64,
    /// Old object ID.
    pub old_id: ObjectId,
    /// New object ID.
    pub new_id: ObjectId,
    /// Committer name.
    pub name: String,
    /// Committer email (without angle brackets).
    pub email: String,
    /// Time in seconds since epoch.
    pub time_seconds: u64,
    /// Timezone offset as Git stores it on disk: the `±HHMM` identity field read as a
    /// signed decimal number (`+0530` is `530`, `-0800` is `-800`), not minutes.
    pub tz_offset: i16,
    /// Log message.
    pub message: String,
}

/// Write options for reftable creation.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Block size in bytes. 0 means use the default.
    pub block_size: u32,
    /// Restart interval (number of records between restart points).
    pub restart_interval: usize,
    /// Whether to write log blocks.
    pub write_log: bool,
    /// Skip writing the object index (config `reftable.indexObjects=false`).
    pub skip_index_objects: bool,
    /// Write blocks without padding to the block size.
    pub unpadded: bool,
    /// Object-id width in bytes: 20 for SHA-1 (reftable version 1), 32 for
    /// SHA-256 (reftable version 2). Defaults to SHA-1.
    pub hash_size: usize,
    /// Geometric factor for stack auto-compaction (`reftable.geometricFactor`).
    /// Defaults to 2 when unset.
    pub auto_compaction_factor: u8,
    /// Whether to run automatic stack compaction after writes (Git test knob via
    /// [`Environment::reftable_autocompaction_enabled`](crate::environment::Environment::reftable_autocompaction_enabled)).
    pub autocompaction_enabled: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            restart_interval: RESTART_INTERVAL,
            write_log: true,
            skip_index_objects: false,
            unpadded: false,
            hash_size: HASH_SIZE,
            auto_compaction_factor: 2,
            autocompaction_enabled: true,
        }
    }
}

/// A ref update that should be written to a reftable transaction.
///
/// The `refname` must already be the backend storage refname (for example a
/// namespaced or per-worktree ref after storage routing). All updates passed to
/// one transaction are written with the same update index, matching Git's
/// reftable backend for `update-ref --stdin` batches.
#[derive(Debug, Clone)]
pub struct ReftableTransactionUpdate {
    /// Full storage refname to update.
    pub refname: String,
    /// New ref value when this item updates a ref; `None` writes only a reflog line.
    pub value: Option<RefValue>,
    /// Optional reflog entry to record in the same table and update index.
    pub log: Option<LogRecord>,
    /// When set, the ref's current oid must match before the update is applied (checked
    /// while the stack lock is held).
    pub expected_old: Option<ObjectId>,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Writes a single reftable file.
///
/// Usage:
/// ```ignore
/// let mut w = ReftableWriter::new(opts, min_idx, max_idx);
/// w.add_ref(&RefRecord { .. })?;
/// w.add_log(&LogRecord { .. })?;
/// let bytes = w.finish()?;
/// ```
pub struct ReftableWriter {
    opts: WriteOptions,
    min_update_index: u64,
    max_update_index: u64,

    // Accumulated ref records (must be added in sorted order).
    refs: Vec<RefRecord>,
    // Accumulated log records.
    logs: Vec<LogRecord>,
}

impl ReftableWriter {
    /// Create a new writer.
    pub fn new(opts: WriteOptions, min_update_index: u64, max_update_index: u64) -> Self {
        Self {
            opts,
            min_update_index,
            max_update_index,
            refs: Vec::new(),
            logs: Vec::new(),
        }
    }

    /// Add a ref record. Records **must** be added in sorted name order.
    pub fn add_ref(&mut self, rec: RefRecord) -> Result<()> {
        if let Some(last) = self.refs.last() {
            if rec.name <= last.name {
                return Err(ReftableError::UnsortedRefs {
                    previous: last.name.clone(),
                    next: rec.name,
                }
                .into());
            }
        }
        self.refs.push(rec);
        Ok(())
    }

    /// Add a log record.
    pub fn add_log(&mut self, rec: LogRecord) -> Result<()> {
        self.logs.push(rec);
        Ok(())
    }

    /// Finish writing and return the complete reftable file bytes.
    ///
    /// This implements the reftable on-disk format so that the
    /// on-disk layout (block boundaries, restart points, padding,
    /// index/object sections, footer offsets) is byte-identical to git.
    pub fn finish(self) -> Result<Vec<u8>> {
        let refs = self.refs;
        let logs = self.logs;
        let opts = self.opts;
        let mut w = WriterState::new(opts, self.min_update_index, self.max_update_index);

        // Refs are added in sorted order; index objects as we go.
        for rec in &refs {
            w.add_ref(rec)?;
        }

        // Logs: sort by (refname asc, update_index desc) — matches
        // reftable_log_record_compare_key.
        let mut logs = logs;
        logs.sort_by(|a, b| {
            a.refname
                .cmp(&b.refname)
                .then_with(|| b.update_index.cmp(&a.update_index))
        });
        if w.opts.write_log {
            for log in &logs {
                w.add_log(log)?;
            }
        }

        w.close()
    }
}

// ---------------------------------------------------------------------------
// Faithful low-level writer (ports git/reftable/{block,writer,record}.c)
// ---------------------------------------------------------------------------

/// Default reftable block size (per the reftable format specification).
const REFTABLE_DEFAULT_BLOCK_SIZE: u32 = 4096;
/// Maximum number of restart points per block (`MAX_RESTARTS`).
const MAX_RESTARTS: usize = (1 << 16) - 1;

/// A record to encode: produces a key and a value body.
enum EncRecord<'a> {
    Ref(&'a RefRecord, u64),
    Log(&'a LogRecord),
    Obj { prefix: Vec<u8>, offsets: Vec<u64> },
    Index { last_key: Vec<u8>, offset: u64 },
}

impl EncRecord<'_> {
    fn block_type(&self) -> u8 {
        match self {
            EncRecord::Ref(..) => BLOCK_TYPE_REF,
            EncRecord::Log(_) => BLOCK_TYPE_LOG,
            EncRecord::Obj { .. } => BLOCK_TYPE_OBJ,
            EncRecord::Index { .. } => BLOCK_TYPE_INDEX,
        }
    }

    /// The record key (used for prefix compression and restart points).
    fn key(&self) -> Vec<u8> {
        match self {
            EncRecord::Ref(r, _) => r.name.as_bytes().to_vec(),
            EncRecord::Log(l) => {
                let mut k = Vec::with_capacity(l.refname.len() + 9);
                k.extend_from_slice(l.refname.as_bytes());
                k.push(0);
                let ts = u64::MAX - l.update_index;
                k.extend_from_slice(&ts.to_be_bytes());
                k
            }
            EncRecord::Obj { prefix, .. } => prefix.clone(),
            EncRecord::Index { last_key, .. } => last_key.clone(),
        }
    }

    /// The `extra` value-type bits stored in the key varint.
    fn val_type(&self) -> u8 {
        match self {
            EncRecord::Ref(r, _) => match r.value {
                RefValue::Deletion => VALUE_DELETION,
                RefValue::Val1(_) => VALUE_ONE_OID,
                RefValue::Val2(..) => VALUE_TWO_OID,
                RefValue::Symref(_) => VALUE_SYMREF,
            },
            // grit only writes reflog updates (value_type 1), never the
            // explicit-deletion form (value_type 0).
            EncRecord::Log(_) => 1,
            EncRecord::Obj { offsets, .. } => {
                if !offsets.is_empty() && offsets.len() < 8 {
                    offsets.len() as u8
                } else {
                    0
                }
            }
            EncRecord::Index { .. } => 0,
        }
    }

    /// Encode the value body (everything after the key).
    fn encode_value(&self, opts: &WriteOptions, out: &mut Vec<u8>) {
        match self {
            EncRecord::Ref(r, update_index_delta) => {
                put_varint(*update_index_delta, out);
                match &r.value {
                    RefValue::Deletion => {}
                    RefValue::Val1(oid) => out.extend_from_slice(oid.as_bytes()),
                    RefValue::Val2(oid, peeled) => {
                        out.extend_from_slice(oid.as_bytes());
                        out.extend_from_slice(peeled.as_bytes());
                    }
                    RefValue::Symref(target) => {
                        put_varint(target.len() as u64, out);
                        out.extend_from_slice(target.as_bytes());
                    }
                }
            }
            EncRecord::Log(l) => {
                out.extend_from_slice(l.old_id.as_bytes());
                out.extend_from_slice(l.new_id.as_bytes());
                put_varint(l.name.len() as u64, out);
                out.extend_from_slice(l.name.as_bytes());
                put_varint(l.email.len() as u64, out);
                out.extend_from_slice(l.email.as_bytes());
                put_varint(l.time_seconds, out);
                out.extend_from_slice(&l.tz_offset.to_be_bytes());
                let msg = clean_log_message(&l.message, opts);
                put_varint(msg.len() as u64, out);
                out.extend_from_slice(&msg);
            }
            EncRecord::Obj { offsets, .. } => {
                if offsets.is_empty() || offsets.len() >= 8 {
                    put_varint(offsets.len() as u64, out);
                }
                if offsets.is_empty() {
                    return;
                }
                put_varint(offsets[0], out);
                let mut last = offsets[0];
                for &o in &offsets[1..] {
                    put_varint(o - last, out);
                    last = o;
                }
            }
            EncRecord::Index { offset, .. } => {
                put_varint(*offset, out);
            }
        }
    }
}

/// Clean a reflog message the way `reftable_writer_add_log` does (unless the
/// writer is in `exact_log_message` mode, which grit never uses): strip
/// trailing newlines and append exactly one.
///
/// Git applies this cleaning whenever the message field is non-NULL, including
/// the empty string: `""` becomes `"\n"` (a single trailing newline), not an
/// empty value. grit's `LogRecord` always carries a (possibly empty) `String`,
/// so the cleaning always runs — matching git's `msglen == 1` for reflog entries
/// written without an explicit message (e.g. `update-ref` with no `-m`,
/// t0613 'restart interval at every single record').
fn clean_log_message(msg: &str, opts: &WriteOptions) -> Vec<u8> {
    // Git's reftable backend truncates the reflog message to `block_size / 2`
    // bytes before storing it (reftable-backend.c: `xstrndup(u->msg,
    // block_size / 2)`) so that an oversized message still fits inside a log
    // block instead of failing the whole transaction with "entry too large"
    // (t0610 'basic: can write large commit message'). Mirror that bound,
    // clamping to a UTF-8 char boundary so the resulting string stays valid.
    let limit = (opts.block_size as usize / 2).max(1);
    let msg = if msg.len() > limit {
        let mut end = limit;
        while end > 0 && !msg.is_char_boundary(end) {
            end -= 1;
        }
        &msg[..end]
    } else {
        msg
    };
    let trimmed = msg.trim_end_matches('\n');
    let mut out = trimmed.as_bytes().to_vec();
    out.push(b'\n');
    out
}

/// Encode a key (prefix/suffix compression) into `out`, returning whether this
/// was a restart point. Mirrors `reftable_encode_key`.
fn encode_key(prev: &[u8], key: &[u8], extra: u8, out: &mut Vec<u8>) -> bool {
    let prefix_len = common_prefix_len(prev, key);
    let suffix_len = key.len() - prefix_len;
    put_varint(prefix_len as u64, out);
    put_varint(((suffix_len as u64) << 3) | (extra as u64), out);
    out.extend_from_slice(&key[prefix_len..]);
    prefix_len == 0
}

/// In-progress block being filled by the writer.
struct BlockWriter {
    typ: u8,
    /// Bytes from `header_off` onwards (block type byte + 3 reserved length
    /// bytes are at the start; record payload follows).
    buf: Vec<u8>,
    header_off: usize,
    block_size: usize,
    restart_interval: usize,
    restarts: Vec<u32>,
    last_key: Vec<u8>,
    entries: usize,
}

impl BlockWriter {
    fn new(typ: u8, block_size: usize, header_off: usize, restart_interval: usize) -> Self {
        // buf is laid out starting at header_off: [type][len:3][records...]
        let mut buf = vec![0u8; header_off + 4];
        buf[header_off] = typ;
        Self {
            typ,
            buf,
            header_off,
            block_size,
            restart_interval,
            restarts: Vec::new(),
            last_key: Vec::new(),
            entries: 0,
        }
    }

    /// `w->next` equivalent: number of bytes written so far (within the block,
    /// counting from offset 0 which includes header_off).
    fn next(&self) -> usize {
        self.buf.len()
    }

    /// Try to add a record. Returns Ok(true) if added, Ok(false) if it does not
    /// fit (entry-too-big), or Err on other failure.
    fn add(&mut self, rec: &EncRecord, opts: &WriteOptions) -> Result<bool> {
        let key = rec.key();
        if key.is_empty() {
            return Err(ReftableError::EmptyRecordKey.into());
        }
        let restart = self.entries.is_multiple_of(self.restart_interval);
        let prev: &[u8] = if restart { &[] } else { &self.last_key };

        let mut encoded = Vec::new();
        let is_restart = encode_key(prev, &key, rec.val_type(), &mut encoded);
        rec.encode_value(opts, &mut encoded);
        let n = encoded.len();

        // register_restart overflow check: 2 + 3*rlen + n > block_size - next
        let mut rlen = self.restarts.len();
        let mut is_restart = is_restart;
        if rlen >= MAX_RESTARTS {
            is_restart = false;
        }
        if is_restart {
            rlen += 1;
        }
        if self.block_size > 0 && 2 + 3 * rlen + n > self.block_size - self.next() {
            return Ok(false);
        }

        if is_restart {
            self.restarts.push(self.next() as u32);
        }
        self.buf.extend_from_slice(&encoded);
        self.last_key = key;
        self.entries += 1;
        Ok(true)
    }

    /// Finalize the block in memory: append restart table + count, write the
    /// 3-byte block length, and (for log blocks) compress. Returns the raw byte
    /// length written (`raw_bytes`).
    fn finish(&mut self) -> Result<usize> {
        for &r in &self.restarts {
            self.buf.push(((r >> 16) & 0xff) as u8);
            self.buf.push(((r >> 8) & 0xff) as u8);
            self.buf.push((r & 0xff) as u8);
        }
        let rc = self.restarts.len() as u16;
        self.buf.push((rc >> 8) as u8);
        self.buf.push((rc & 0xff) as u8);

        // block length (uncompressed) goes into the 3 bytes after the type.
        let block_len = self.buf.len();
        self.buf[self.header_off + 1] = ((block_len >> 16) & 0xff) as u8;
        self.buf[self.header_off + 2] = ((block_len >> 8) & 0xff) as u8;
        self.buf[self.header_off + 3] = (block_len & 0xff) as u8;

        if self.typ == BLOCK_TYPE_LOG {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            let skip = 4 + self.header_off;
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
            enc.write_all(&self.buf[skip..])
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let compressed = enc.finish().map_err(|e| Error::Zlib(e.to_string()))?;
            self.buf.truncate(skip);
            self.buf.extend_from_slice(&compressed);
        }
        Ok(self.buf.len())
    }
}

/// Per-section statistics accumulated while writing a reftable.
#[derive(Default, Clone)]
struct SectionStats {
    blocks: usize,
    index_blocks: usize,
    offset: u64,
    index_offset: u64,
}

/// An object-index entry collected while writing refs.
struct ObjEntry {
    hash: Vec<u8>,
    offsets: Vec<u64>,
}

/// The full writer state needed to emit a reftable file.
struct WriterState {
    opts: WriteOptions,
    min_update_index: u64,
    max_update_index: u64,

    out: Vec<u8>,
    next: u64,
    pending_padding: usize,

    block: Option<BlockWriter>,
    block_type: u8,

    /// Index records for the current section (last_key, offset).
    index: Vec<(Vec<u8>, u64)>,

    /// Object-index tree (kept sorted by hash).
    obj_entries: Vec<ObjEntry>,
    object_id_len: usize,

    ref_stats: SectionStats,
    obj_stats: SectionStats,
    log_stats: SectionStats,
    idx_blocks_total: usize,
}

impl WriterState {
    fn new(mut opts: WriteOptions, min: u64, max: u64) -> Self {
        if opts.restart_interval == 0 {
            opts.restart_interval = RESTART_INTERVAL;
        }
        if opts.block_size == 0 {
            opts.block_size = REFTABLE_DEFAULT_BLOCK_SIZE;
        }
        Self {
            opts,
            min_update_index: min,
            max_update_index: max,
            out: Vec::new(),
            next: 0,
            pending_padding: 0,
            block: None,
            block_type: 0,
            index: Vec::new(),
            obj_entries: Vec::new(),
            object_id_len: 0,
            ref_stats: SectionStats::default(),
            obj_stats: SectionStats::default(),
            log_stats: SectionStats::default(),
            idx_blocks_total: 0,
        }
    }

    fn header_size(&self) -> usize {
        // Version 1 (SHA-1) is 24 bytes; version 2 (SHA-256) adds a 4-byte
        // hash-id field for 28.
        if self.opts.hash_size == 32 {
            28
        } else {
            24
        }
    }

    fn write_header(&self, dest: &mut [u8]) {
        dest[0..4].copy_from_slice(REFTABLE_MAGIC);
        dest[4] = if self.opts.hash_size == 32 { 2 } else { 1 };
        dest[5] = ((self.opts.block_size >> 16) & 0xff) as u8;
        dest[6] = ((self.opts.block_size >> 8) & 0xff) as u8;
        dest[7] = (self.opts.block_size & 0xff) as u8;
        dest[8..16].copy_from_slice(&self.min_update_index.to_be_bytes());
        dest[16..24].copy_from_slice(&self.max_update_index.to_be_bytes());
        // Version 2 records the hash function id (`sha1` / `s256`, Git
        // `GIT_SHA{1,256}_FORMAT_ID`).
        if self.opts.hash_size == 32 {
            dest[24..28].copy_from_slice(b"s256");
        }
    }

    fn stats_mut(&mut self, typ: u8) -> &mut SectionStats {
        match typ {
            BLOCK_TYPE_REF => &mut self.ref_stats,
            BLOCK_TYPE_OBJ => &mut self.obj_stats,
            BLOCK_TYPE_LOG => &mut self.log_stats,
            // index blocks roll into the section being indexed; not used here.
            _ => &mut self.ref_stats,
        }
    }

    /// Write `data` then queue `padding` zero bytes for the next write
    /// (`padded_write`).
    fn padded_write(&mut self, data: &[u8], padding: usize) {
        if self.pending_padding > 0 {
            self.out
                .extend(std::iter::repeat_n(0u8, self.pending_padding));
            self.pending_padding = 0;
        }
        self.pending_padding = padding;
        self.out.extend_from_slice(data);
    }

    fn reinit_block(&mut self, typ: u8) {
        let header_off = if self.next == 0 {
            self.header_size()
        } else {
            0
        };
        self.block = Some(BlockWriter::new(
            typ,
            self.opts.block_size as usize,
            header_off,
            self.opts.restart_interval,
        ));
        self.block_type = typ;
    }

    fn add_record(&mut self, rec: &EncRecord) -> Result<()> {
        let typ = rec.block_type();
        if self.block.is_none() {
            self.reinit_block(typ);
        }
        if self.add_to_block(rec)? {
            return Ok(());
        }
        // Block full: flush and retry in a fresh block.
        self.flush_block()?;
        self.reinit_block(typ);
        if !self.add_to_block(rec)? {
            return Err(ReftableError::EntryTooLarge.into());
        }
        Ok(())
    }

    /// Add `rec` to the open block. Returns `Ok(false)` when it does not fit
    /// (or no block is open).
    fn add_to_block(&mut self, rec: &EncRecord) -> Result<bool> {
        match self.block.as_mut() {
            Some(bw) => bw.add(rec, &self.opts),
            None => Ok(false),
        }
    }

    fn add_ref(&mut self, r: &RefRecord) -> Result<()> {
        let delta = r.update_index.saturating_sub(self.min_update_index);
        self.add_record(&EncRecord::Ref(r, delta))?;

        if !self.opts.skip_index_objects {
            match &r.value {
                RefValue::Val1(oid) => self.index_hash(oid.as_bytes()),
                RefValue::Val2(oid, peeled) => {
                    self.index_hash(oid.as_bytes());
                    self.index_hash(peeled.as_bytes());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn add_log(&mut self, l: &LogRecord) -> Result<()> {
        // Finishing the ref section happens before the first log record.
        if matches!(&self.block, Some(b) if b.typ == BLOCK_TYPE_REF) {
            self.finish_public_section()?;
        }
        // Drop pending padding before a log block (matches add_log_verbatim).
        self.next -= self.pending_padding as u64;
        self.pending_padding = 0;
        self.add_record(&EncRecord::Log(l))
    }

    fn index_hash(&mut self, hash: &[u8]) {
        let off = self.next;
        match self
            .obj_entries
            .binary_search_by(|e| e.hash.as_slice().cmp(hash))
        {
            Ok(idx) => {
                let e = &mut self.obj_entries[idx];
                if e.offsets.last() != Some(&off) {
                    e.offsets.push(off);
                }
            }
            Err(idx) => {
                self.obj_entries.insert(
                    idx,
                    ObjEntry {
                        hash: hash.to_vec(),
                        offsets: vec![off],
                    },
                );
            }
        }
    }

    /// `writer_flush_nonempty_block`.
    fn flush_block(&mut self) -> Result<()> {
        let Some(mut bw) = self.block.take() else {
            return Ok(());
        };
        if bw.entries == 0 {
            self.block = Some(bw);
            return Ok(());
        }
        let typ = bw.typ;
        let raw_bytes = bw.finish()?;

        let mut padding = 0;
        if !self.opts.unpadded && typ != BLOCK_TYPE_LOG {
            padding = (self.opts.block_size as usize).saturating_sub(raw_bytes);
        }

        let block_typ_off = if self.stats_mut(typ).blocks == 0 {
            self.next
        } else {
            0
        };
        {
            let next = self.next;
            let st = self.stats_mut(typ);
            if block_typ_off > 0 {
                st.offset = next;
            }
            st.blocks += 1;
        }

        if self.next == 0 {
            // Write the reftable header into the front of the first block.
            let hs = self.header_size();
            self.write_header_into_block(&mut bw, hs);
        }

        let data = bw.buf.clone();
        self.padded_write(&data, padding);

        // Record an index entry for this block.
        self.index.push((bw.last_key.clone(), self.next));

        self.next += (padding + raw_bytes) as u64;
        self.block = None;
        Ok(())
    }

    fn write_header_into_block(&self, bw: &mut BlockWriter, hs: usize) {
        let mut hdr = vec![0u8; hs];
        self.write_header(&mut hdr);
        bw.buf[..hs].copy_from_slice(&hdr);
    }

    fn flush_block_if_nonempty(&mut self) -> Result<()> {
        if matches!(&self.block, Some(b) if b.entries == 0) {
            return Ok(());
        }
        self.flush_block()
    }

    /// `writer_finish_section`: flush the current block then emit any index.
    fn finish_section(&mut self) -> Result<()> {
        let typ = self.block_type;
        let threshold = if self.opts.unpadded { 1 } else { 3 };
        let before_blocks = self.idx_blocks_total;

        self.flush_block_if_nonempty()?;

        let mut max_level = 0;
        let mut index_start = 0u64;

        while self.index.len() > threshold {
            max_level += 1;
            index_start = self.next;
            self.reinit_block(BLOCK_TYPE_INDEX);

            let idx = std::mem::take(&mut self.index);
            for (last_key, offset) in &idx {
                self.add_record(&EncRecord::Index {
                    last_key: last_key.clone(),
                    offset: *offset,
                })?;
            }
            // Count index blocks produced during this level.
            let blocks_before = self.count_index_blocks_marker();
            self.flush_index_block()?;
            let _ = blocks_before;
        }

        self.index.clear();

        let index_blocks = self.idx_blocks_total - before_blocks;
        {
            let st = self.stats_mut(typ);
            st.index_blocks = index_blocks;
            st.index_offset = index_start;
        }
        let _ = max_level;
        Ok(())
    }

    fn count_index_blocks_marker(&self) -> usize {
        self.idx_blocks_total
    }

    /// Flush an index block: like `flush_block` but the produced block counts
    /// toward `idx_blocks_total` and re-populates `self.index` for the next
    /// (higher) level.
    fn flush_index_block(&mut self) -> Result<()> {
        let Some(mut bw) = self.block.take() else {
            return Ok(());
        };
        if bw.entries == 0 {
            self.block = Some(bw);
            return Ok(());
        }
        let raw_bytes = bw.finish()?;
        let mut padding = 0;
        if !self.opts.unpadded {
            padding = (self.opts.block_size as usize).saturating_sub(raw_bytes);
        }
        if self.next == 0 {
            let hs = self.header_size();
            self.write_header_into_block(&mut bw, hs);
        }
        let data = bw.buf.clone();
        self.padded_write(&data, padding);
        self.index.push((bw.last_key.clone(), self.next));
        self.next += (padding + raw_bytes) as u64;
        self.idx_blocks_total += 1;
        self.block = None;
        Ok(())
    }

    /// `writer_dump_object_index`.
    fn dump_object_index(&mut self) -> Result<()> {
        // object_id_len = max common prefix among sorted hashes + 1, min 2.
        let mut max_common = 1usize;
        for w in self.obj_entries.windows(2) {
            let n = common_prefix_len(&w[0].hash, &w[1].hash);
            if n > max_common {
                max_common = n;
            }
        }
        self.object_id_len = max_common + 1;
        let id_len = self.object_id_len;

        self.reinit_block(BLOCK_TYPE_OBJ);
        let entries = std::mem::take(&mut self.obj_entries);
        for e in &entries {
            let prefix = e.hash[..id_len.min(e.hash.len())].to_vec();
            self.add_obj_record(prefix, &e.offsets)?;
        }
        self.obj_entries = entries;
        self.finish_section()
    }

    fn add_obj_record(&mut self, prefix: Vec<u8>, offsets: &[u64]) -> Result<()> {
        // Try with full offsets; on overflow in a fresh block, drop offsets.
        let typ = BLOCK_TYPE_OBJ;
        if self.block.is_none() {
            self.reinit_block(typ);
        }
        let rec = EncRecord::Obj {
            prefix: prefix.clone(),
            offsets: offsets.to_vec(),
        };
        if self.add_to_block(&rec)? {
            return Ok(());
        }
        self.flush_block()?;
        self.reinit_block(typ);
        if self.add_to_block(&rec)? {
            return Ok(());
        }
        // Drop offsets entirely.
        let rec = EncRecord::Obj {
            prefix,
            offsets: Vec::new(),
        };
        self.add_to_block(&rec)?;
        Ok(())
    }

    /// `writer_finish_public_section`.
    fn finish_public_section(&mut self) -> Result<()> {
        let Some(bw) = &self.block else {
            return Ok(());
        };
        let typ = bw.typ;
        self.finish_section()?;
        if typ == BLOCK_TYPE_REF && !self.opts.skip_index_objects && self.ref_stats.index_blocks > 0
        {
            self.dump_object_index()?;
        }
        self.obj_entries.clear();
        self.block = None;
        self.block_type = 0;
        Ok(())
    }

    /// `reftable_writer_close`.
    fn close(mut self) -> Result<Vec<u8>> {
        self.finish_public_section()?;
        let empty_table = self.next == 0;
        self.pending_padding = 0;

        if empty_table {
            let hs = self.header_size();
            let mut header = vec![0u8; hs];
            self.write_header(&mut header);
            self.padded_write(&header, 0);
        }

        let mut footer = vec![0u8; self.header_size()];
        self.write_header(&mut footer);
        footer.extend_from_slice(&self.ref_stats.index_offset.to_be_bytes());
        let obj_field = (self.obj_stats.offset << 5) | (self.object_id_len as u64);
        footer.extend_from_slice(&obj_field.to_be_bytes());
        footer.extend_from_slice(&self.obj_stats.index_offset.to_be_bytes());
        footer.extend_from_slice(&self.log_stats.offset.to_be_bytes());
        footer.extend_from_slice(&self.log_stats.index_offset.to_be_bytes());
        let crc = crc32(&footer);
        footer.extend_from_slice(&crc.to_be_bytes());

        // Footer write drops pending padding (flush() before padded_write).
        self.pending_padding = 0;
        self.out.extend_from_slice(&footer);

        Ok(self.out)
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Reads a single reftable file from a byte buffer.
pub struct ReftableReader {
    data: Arc<[u8]>,
    version: u8,
    block_size: u32,
    min_update_index: u64,
    max_update_index: u64,
    ref_index_position: u64,
    log_position: u64,
}

/// Footer fields the reader uses (object and log index positions are not consulted).
#[derive(Debug)]
struct Footer {
    block_size: u32,
    min_update_index: u64,
    max_update_index: u64,
    ref_index_position: u64,
    log_position: u64,
}

impl ReftableReader {
    /// Open a reftable from bytes.
    pub fn new(data: Vec<u8>) -> Result<Self> {
        Self::from_arc(Arc::from(data))
    }

    /// Open a reftable from shared bytes (avoids cloning table payloads).
    pub fn from_arc(data: Arc<[u8]>) -> Result<Self> {
        if data.len() < HEADER_SIZE + FOOTER_V1_SIZE {
            // Could be an empty table (header + footer only = 24 + 68 = 92)
            if data.len() < HEADER_SIZE {
                return Err(ReftableError::TooSmall { len: data.len() }.into());
            }
        }

        // Parse header
        if &data[0..4] != REFTABLE_MAGIC {
            return Err(ReftableError::BadMagic.into());
        }
        let version = data[4];
        if version != 1 && version != 2 {
            return Err(ReftableError::UnsupportedVersion { version }.into());
        }
        // Parse footer
        let footer_size = if version == 2 { 72 } else { FOOTER_V1_SIZE };
        if data.len() < footer_size {
            return Err(ReftableError::TooSmall { len: data.len() }.into());
        }
        let footer_start = data.len() - footer_size;
        let footer = parse_footer(&data[footer_start..], version)?;

        Ok(Self {
            data,
            version,
            block_size: footer.block_size,
            min_update_index: footer.min_update_index,
            max_update_index: footer.max_update_index,
            ref_index_position: footer.ref_index_position,
            log_position: footer.log_position,
        })
    }

    /// Object-id width implied by the reftable version (32 for version 2 / SHA-256,
    /// 20 otherwise).
    fn hash_size(&self) -> usize {
        if self.version == 2 {
            32
        } else {
            20
        }
    }

    /// File-header size implied by the reftable version (28 for version 2, 24 otherwise).
    fn header_len(&self) -> usize {
        if self.version == 2 {
            28
        } else {
            HEADER_SIZE
        }
    }

    /// Read all ref records from the table.
    pub fn read_refs(&self) -> Result<Vec<RefRecord>> {
        let mut refs = Vec::new();
        self.for_each_ref(|rec| {
            refs.push(rec.clone());
            ControlFlow::<()>::Continue(())
        })?;
        Ok(refs)
    }

    /// Visit ref records in sorted name order, optionally bounded by `lower` (inclusive)
    /// and `upper_exclusive` (exclusive). When `upper_exclusive` is `None`, iteration
    /// continues through the last ref block.
    pub fn for_each_ref(&self, f: impl FnMut(&RefRecord) -> ControlFlow<()>) -> Result<()> {
        self.for_each_ref_in_range("", None, f)
    }

    /// Visit ref records with name `>= lower` and `< upper_exclusive` when set.
    pub fn for_each_ref_in_range(
        &self,
        lower: &str,
        upper_exclusive: Option<&str>,
        mut f: impl FnMut(&RefRecord) -> ControlFlow<()>,
    ) -> Result<()> {
        let footer_size = if self.version == 2 {
            72
        } else {
            FOOTER_V1_SIZE
        };
        let file_end = self.data.len() - footer_size;

        let ref_end = if self.ref_index_position > 0 {
            self.ref_index_position as usize
        } else if self.log_position > 0 {
            self.log_position as usize
        } else {
            file_end
        };

        let header_len = self.header_len();
        let mut pos = if self.ref_index_position > 0 && !lower.is_empty() {
            self.locate_ref_block_via_index(lower.as_bytes())?
                .unwrap_or(header_len)
        } else {
            header_len
        };

        while pos < ref_end {
            if pos >= self.data.len() {
                break;
            }
            let block_type = self.data[pos];
            if block_type == 0 {
                if self.block_size > 0 {
                    let bs = self.block_size as usize;
                    pos = ((pos / bs) + 1) * bs;
                    continue;
                }
                break;
            }
            if block_type != BLOCK_TYPE_REF {
                break;
            }

            let block_len = read_u24(&self.data, pos + 1);
            let block_data_start = pos + 4;
            let is_first = pos == header_len;
            let records_end = if is_first { block_len } else { pos + block_len };

            if records_end > ref_end {
                break;
            }

            let restart_table_start =
                restart_table_start_for_block(&self.data, records_end, block_data_start)?;

            let mut rpos = block_data_start;
            let mut prev_name = Vec::<u8>::new();

            while rpos < restart_table_start {
                let (rec, new_pos) = decode_ref_record(
                    &self.data,
                    rpos,
                    &prev_name,
                    self.min_update_index,
                    self.hash_size(),
                )?;
                prev_name = rec.name.as_bytes().to_vec();
                if rec.name.as_str() < lower {
                    rpos = new_pos;
                    continue;
                }
                if let Some(upper) = upper_exclusive {
                    if rec.name.as_str() >= upper {
                        return Ok(());
                    }
                }
                if f(&rec).is_break() {
                    return Ok(());
                }
                rpos = new_pos;
            }

            if self.block_size > 0 {
                let bs = self.block_size as usize;
                pos = if is_first { bs } else { pos + bs };
            } else {
                pos = records_end;
            }
        }

        Ok(())
    }

    /// Look up a single ref by name.
    pub fn lookup_ref(&self, name: &str) -> Result<Option<RefRecord>> {
        if self.ref_index_position > 0 {
            if let Some(rec) = self.lookup_ref_via_index(name)? {
                return Ok(Some(rec));
            }
        }
        let refs = self.read_refs()?;
        Ok(refs.into_iter().find(|r| r.name == name))
    }

    /// Resolve a ref name to a record using ref index blocks when present.
    fn lookup_ref_via_index(&self, name: &str) -> Result<Option<RefRecord>> {
        let Some(block_off) = self.locate_ref_block_via_index(name.as_bytes())? else {
            return Ok(None);
        };
        self.find_ref_in_block(block_off, name)
    }

    fn locate_ref_block_via_index(&self, name: &[u8]) -> Result<Option<usize>> {
        let mut pos = self.ref_index_position as usize;
        if pos == 0 {
            return Ok(None);
        }
        for _ in 0..64 {
            if pos >= self.data.len() {
                return Ok(None);
            }
            match self.data[pos] {
                BLOCK_TYPE_REF => return Ok(Some(pos)),
                BLOCK_TYPE_INDEX => {
                    let Some(offset) = self.index_block_seek_offset(pos, name)? else {
                        return Ok(None);
                    };
                    let next = offset as usize;
                    if next == pos {
                        return Ok(None);
                    }
                    pos = next;
                }
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Pick the index entry for `name`: the first record whose `last_key` is >=
    /// `name`, or the final entry when every indexed `last_key` is smaller.
    fn index_block_seek_offset(&self, block_pos: usize, name: &[u8]) -> Result<Option<u64>> {
        let (records_start, records_end) = self.block_record_range(block_pos)?;
        let mut rpos = records_start;
        let mut prev_key = Vec::<u8>::new();
        let mut last: Option<u64> = None;
        while rpos < records_end {
            let (key, offset, new_pos) = decode_index_record(&self.data, rpos, &prev_key)?;
            prev_key = key;
            if prev_key.as_slice() >= name {
                return Ok(Some(offset));
            }
            last = Some(offset);
            rpos = new_pos;
        }
        Ok(last)
    }

    fn find_ref_in_block(&self, block_pos: usize, name: &str) -> Result<Option<RefRecord>> {
        let (records_start, records_end) = self.block_record_range(block_pos)?;
        let mut rpos = records_start;
        let mut prev_name = Vec::<u8>::new();
        while rpos < records_end {
            let (rec, new_pos) = decode_ref_record(
                &self.data,
                rpos,
                &prev_name,
                self.min_update_index,
                self.hash_size(),
            )?;
            prev_name = rec.name.as_bytes().to_vec();
            match rec.name.as_str().cmp(name) {
                std::cmp::Ordering::Equal => return Ok(Some(rec)),
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => {}
            }
            rpos = new_pos;
        }
        Ok(None)
    }

    fn block_record_range(&self, block_pos: usize) -> Result<(usize, usize)> {
        if block_pos + 4 > self.data.len() {
            return Err(truncated(ReftableField::BlockHeader));
        }
        let block_len = read_u24(&self.data, block_pos + 1);
        let header_len = self.header_len();
        let is_first = block_pos == header_len;
        let records_end = if is_first {
            block_len
        } else {
            block_pos + block_len
        };
        if records_end > self.data.len() || records_end < 2 {
            return Err(corrupt(ReftableError::InvalidBlockLength));
        }
        let records_data_start = block_pos + 4;
        let restart_table_start =
            restart_table_start_for_block(&self.data, records_end, records_data_start)?;
        Ok((records_data_start, restart_table_start))
    }

    /// Read all log records from the table.
    pub fn read_logs(&self) -> Result<Vec<LogRecord>> {
        let footer_size = if self.version == 2 {
            72
        } else {
            FOOTER_V1_SIZE
        };
        let file_end = self.data.len() - footer_size;

        // Determine where the log section starts. Git records the log offset in
        // the footer, but when the log block is the *first* block in the file it
        // shares its physical block with the 24-byte reftable header and the
        // recorded offset is left at 0 (see `writer_flush_nonempty_block`'s
        // `block_typ_off = (blocks == 0) ? next : 0`). The reader detects this
        // by checking whether the first on-disk block (the byte right after the
        // header) is a log block — mirroring `is_present` in git's table.c.
        let mut pos = if self.log_position > 0 {
            self.log_position as usize
        } else if self.data.len() > self.header_len()
            && self.data[self.header_len()] == BLOCK_TYPE_LOG
        {
            // Log block is the first block; it begins right after the header.
            self.header_len()
        } else {
            return Ok(Vec::new());
        };
        let mut logs = Vec::new();

        while pos < file_end {
            if pos >= self.data.len() {
                break;
            }
            let block_type = self.data[pos];
            if block_type != BLOCK_TYPE_LOG {
                break;
            }
            // When the log block shares its physical block with the reftable
            // header, the 3-byte block length counts from offset 0 and so
            // includes the header bytes; the compressed payload still starts
            // right after the type+length header at `pos + 4`.
            let is_first = pos == self.header_len() && self.log_position == 0;
            let block_len = read_u24(&self.data, pos + 1);
            let compressed_start = pos + 4;

            // The inflated size is block_len minus the 4-byte type+length header
            // (and, for the first block, minus the embedded reftable header).
            let header_prefix = if is_first { self.header_len() } else { 0 };
            let inflated_size = block_len.saturating_sub(4 + header_prefix);

            let remaining = &self.data[compressed_start..file_end];
            let (inflated, consumed) = decompress_log_payload(remaining, inflated_size)?;

            // Parse log records from inflated data
            // Read restart_count from end
            if inflated.len() < 2 {
                break;
            }
            let records_end = inflated.len();
            let restart_table_start = restart_table_start_for_block(&inflated, records_end, 0)?;

            let mut rpos = 0usize;
            let mut prev_key = Vec::<u8>::new();

            while rpos < restart_table_start {
                let (log, new_pos) =
                    decode_log_record(&inflated, rpos, &prev_key, self.hash_size())?;
                // Reconstruct key for prefix compression
                let mut key = Vec::new();
                key.extend_from_slice(log.refname.as_bytes());
                key.push(0);
                key.extend_from_slice(&(0xffffffffffffffffu64 - log.update_index).to_be_bytes());
                prev_key = key;
                logs.push(log);
                rpos = new_pos;
            }

            pos = compressed_start + consumed;
        }

        Ok(logs)
    }

    /// Get the block size from the header.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Get the min update index.
    pub fn min_update_index(&self) -> u64 {
        self.min_update_index
    }

    /// Get the max update index.
    pub fn max_update_index(&self) -> u64 {
        self.max_update_index
    }

    /// Byte offset of the top-level ref index section, or 0 if none.
    pub fn ref_index_offset(&self) -> u64 {
        self.ref_index_position
    }

    /// Effective ref payload size for compaction heuristics (file size minus header overhead).
    pub fn payload_size(&self) -> u64 {
        let overhead = if self.version == 2 { 28 } else { 24 };
        self.data.len().saturating_sub(overhead) as u64
    }
}

// ---------------------------------------------------------------------------
// Record decoding helpers
// ---------------------------------------------------------------------------

fn decode_index_record(data: &[u8], pos: usize, prev_key: &[u8]) -> Result<(Vec<u8>, u64, usize)> {
    let (prefix_len, p) = get_varint(data, pos)?;
    let (suffix_and_type, mut p) = get_varint(data, p)?;
    let suffix_len = (suffix_and_type >> 3) as usize;

    let mut key = Vec::with_capacity(prefix_len as usize + suffix_len);
    if prefix_len > 0 {
        if (prefix_len as usize) > prev_key.len() {
            return Err(corrupt(ReftableError::PrefixTooLong {
                field: ReftableField::IndexKey,
            }));
        }
        key.extend_from_slice(&prev_key[..prefix_len as usize]);
    }
    if p + suffix_len > data.len() {
        return Err(truncated(ReftableField::IndexKey));
    }
    key.extend_from_slice(&data[p..p + suffix_len]);
    p += suffix_len;

    let (offset, p) = get_varint(data, p)?;
    Ok((key, offset, p))
}

fn decode_ref_record(
    data: &[u8],
    pos: usize,
    prev_name: &[u8],
    min_update_index: u64,
    hash_size: usize,
) -> Result<(RefRecord, usize)> {
    let (prefix_len, p) = get_varint(data, pos)?;
    let (suffix_and_type, mut p) = get_varint(data, p)?;
    let suffix_len = (suffix_and_type >> 3) as usize;
    let value_type = (suffix_and_type & 0x7) as u8;

    // Reconstruct name
    let mut name = Vec::with_capacity(prefix_len as usize + suffix_len);
    if prefix_len > 0 {
        if (prefix_len as usize) > prev_name.len() {
            return Err(corrupt(ReftableError::PrefixTooLong {
                field: ReftableField::RefName,
            }));
        }
        name.extend_from_slice(&prev_name[..prefix_len as usize]);
    }
    if p + suffix_len > data.len() {
        return Err(truncated(ReftableField::RefName));
    }
    name.extend_from_slice(&data[p..p + suffix_len]);
    p += suffix_len;

    let name_str = String::from_utf8(name).map_err(|_| invalid_utf8(ReftableField::RefName))?;

    let (update_index_delta, mut p) = get_varint(data, p)?;
    let update_index = min_update_index + update_index_delta;

    let value = match value_type {
        VALUE_DELETION => RefValue::Deletion,
        VALUE_ONE_OID => {
            if p + hash_size > data.len() {
                return Err(truncated(ReftableField::ObjectId));
            }
            let oid = ObjectId::from_bytes(&data[p..p + hash_size])?;
            p += hash_size;
            RefValue::Val1(oid)
        }
        VALUE_TWO_OID => {
            if p + 2 * hash_size > data.len() {
                return Err(truncated(ReftableField::ObjectIdPair));
            }
            let oid = ObjectId::from_bytes(&data[p..p + hash_size])?;
            p += hash_size;
            let peeled = ObjectId::from_bytes(&data[p..p + hash_size])?;
            p += hash_size;
            RefValue::Val2(oid, peeled)
        }
        VALUE_SYMREF => {
            let (target_len, p2) = get_varint(data, p)?;
            p = p2;
            let target_len = target_len as usize;
            if p + target_len > data.len() {
                return Err(truncated(ReftableField::SymrefTarget));
            }
            let target = String::from_utf8(data[p..p + target_len].to_vec())
                .map_err(|_| invalid_utf8(ReftableField::SymrefTarget))?;
            p += target_len;
            RefValue::Symref(target)
        }
        _ => {
            return Err(corrupt(ReftableError::UnknownValueType { value_type }));
        }
    };

    Ok((
        RefRecord {
            name: name_str,
            update_index,
            value,
        },
        p,
    ))
}

fn decode_log_record(
    data: &[u8],
    pos: usize,
    prev_key: &[u8],
    hash_size: usize,
) -> Result<(LogRecord, usize)> {
    let (prefix_len, p) = get_varint(data, pos)?;
    let (suffix_and_type, mut p) = get_varint(data, p)?;
    let suffix_len = (suffix_and_type >> 3) as usize;
    let log_type = (suffix_and_type & 0x7) as u8;

    // Reconstruct key
    let mut key = Vec::with_capacity(prefix_len as usize + suffix_len);
    if prefix_len > 0 {
        if (prefix_len as usize) > prev_key.len() {
            return Err(corrupt(ReftableError::PrefixTooLong {
                field: ReftableField::LogKey,
            }));
        }
        key.extend_from_slice(&prev_key[..prefix_len as usize]);
    }
    if p + suffix_len > data.len() {
        return Err(truncated(ReftableField::LogKey));
    }
    key.extend_from_slice(&data[p..p + suffix_len]);
    p += suffix_len;

    // Parse key: refname \0 reverse_int64(update_index)
    let null_pos = key
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| corrupt(ReftableError::InvalidLogKey))?;
    let refname = String::from_utf8(key[..null_pos].to_vec())
        .map_err(|_| invalid_utf8(ReftableField::RefName))?;
    if null_pos + 9 > key.len() {
        return Err(corrupt(ReftableError::InvalidLogKey));
    }
    let reversed_idx = u64::from_be_bytes(
        key[null_pos + 1..null_pos + 9]
            .try_into()
            .map_err(|_| corrupt(ReftableError::InvalidLogKey))?,
    );
    let update_index = 0xffffffffffffffffu64 - reversed_idx;

    if log_type == 0 {
        // Deletion
        let zero_oid = ObjectId::from_bytes(&vec![0u8; hash_size])?;
        return Ok((
            LogRecord {
                refname,
                update_index,
                old_id: zero_oid,
                new_id: zero_oid,
                name: String::new(),
                email: String::new(),
                time_seconds: 0,
                tz_offset: 0,
                message: String::new(),
            },
            p,
        ));
    }

    // log_type == 1: standard log data
    if p + 2 * hash_size > data.len() {
        return Err(truncated(ReftableField::ObjectIdPair));
    }
    let old_id = ObjectId::from_bytes(&data[p..p + hash_size])?;
    p += hash_size;
    let new_id = ObjectId::from_bytes(&data[p..p + hash_size])?;
    p += hash_size;

    let (name_len, p2) = get_varint(data, p)?;
    p = p2;
    let name_len = name_len as usize;
    if p + name_len > data.len() {
        return Err(truncated(ReftableField::LogName));
    }
    let name = String::from_utf8(data[p..p + name_len].to_vec())
        .map_err(|_| invalid_utf8(ReftableField::LogName))?;
    p += name_len;

    let (email_len, p2) = get_varint(data, p)?;
    p = p2;
    let email_len = email_len as usize;
    if p + email_len > data.len() {
        return Err(truncated(ReftableField::LogEmail));
    }
    let email = String::from_utf8(data[p..p + email_len].to_vec())
        .map_err(|_| invalid_utf8(ReftableField::LogEmail))?;
    p += email_len;

    let (time_seconds, p2) = get_varint(data, p)?;
    p = p2;

    if p + 2 > data.len() {
        return Err(truncated(ReftableField::LogTimezone));
    }
    let tz_offset = i16::from_be_bytes([data[p], data[p + 1]]);
    p += 2;

    let (msg_len, p2) = get_varint(data, p)?;
    p = p2;
    let msg_len = msg_len as usize;
    if p + msg_len > data.len() {
        return Err(truncated(ReftableField::LogMessage));
    }
    let message = String::from_utf8(data[p..p + msg_len].to_vec())
        .map_err(|_| invalid_utf8(ReftableField::LogMessage))?;
    p += msg_len;

    Ok((
        LogRecord {
            refname,
            update_index,
            old_id,
            new_id,
            name,
            email,
            time_seconds,
            tz_offset,
            message,
        },
        p,
    ))
}

// ---------------------------------------------------------------------------
// Stack management
// ---------------------------------------------------------------------------

/// Widen a null OID to `hash_size` bytes so every OID written into a reftable
/// shares the table's hash width. Non-null OIDs (real objects) are already at the
/// repository width and are returned unchanged.
pub(crate) fn widen_oid_to(oid: ObjectId, hash_size: usize) -> ObjectId {
    if oid.is_zero() && oid.as_bytes().len() != hash_size {
        ObjectId::from_bytes(&vec![0u8; hash_size]).unwrap_or(oid)
    } else {
        oid
    }
}

/// Object-id width for reftables in the repository owning `git_dir`: 32 bytes
/// (reftable version 2) when `extensions.objectformat=sha256`, else 20 (version 1).
fn reftable_hash_size_for_git_dir(git_dir: &Path) -> usize {
    let cfg = crate::config::ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    match cfg
        .get("extensions.objectformat")
        .and_then(|v| crate::objects::HashAlgo::from_name(&v))
    {
        Some(crate::objects::HashAlgo::Sha256) => 32,
        _ => 20,
    }
}

/// Exclusive upper bound for ref names sharing a prefix without a trailing slash
/// (e.g. `refs/heads/b05` → `refs/heads/b06`).
fn prefix_upper_exclusive(prefix: &str) -> Option<String> {
    if prefix.is_empty() || prefix.ends_with('/') {
        return None;
    }
    let mut end = prefix.as_bytes().to_vec();
    while let Some(last) = end.last_mut() {
        if *last == b'/' {
            break;
        }
        if *last < 0xff {
            *last += 1;
            return String::from_utf8(end).ok();
        }
        *last = 0;
        end.pop();
    }
    None
}

/// Compaction segment `[start, end)` in table index space (git `suggest_compaction_segment`).
fn suggest_compaction_segment(sizes: &[u64]) -> (usize, usize) {
    let n = sizes.len();
    if n <= 1 {
        return (0, 0);
    }
    let mut seg_end = 0usize;
    let mut bytes = 0u64;
    let mut i = n - 1;
    while i > 0 {
        if sizes[i - 1] < sizes[i].saturating_mul(2) {
            seg_end = i + 1;
            bytes = sizes[i];
            break;
        }
        i -= 1;
    }
    if seg_end == 0 {
        return (0, 0);
    }
    let mut seg_start = 0usize;
    i = seg_end.saturating_sub(1);
    while i > 0 {
        let curr = bytes;
        bytes = bytes.saturating_add(sizes[i - 1]);
        if sizes[i - 1] < curr.saturating_mul(2) {
            seg_start = i - 1;
        }
        i -= 1;
    }
    if seg_end <= seg_start + 1 {
        return (0, 0);
    }
    (seg_start, seg_end)
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct StackCacheKey {
    reftable_dir: PathBuf,
    tables_list_len: u64,
    tables_list_modified: i64,
}

struct CachedStackTables {
    table_names: Vec<String>,
    readers: Vec<ReftableReader>,
}

// hygiene: process-wide reftable table cache keyed by tables.list identity (cleared on writes)
static STACK_TABLE_CACHE: OnceLock<Mutex<HashMap<StackCacheKey, Arc<CachedStackTables>>>> =
    OnceLock::new();

fn stack_table_cache() -> &'static Mutex<HashMap<StackCacheKey, Arc<CachedStackTables>>> {
    STACK_TABLE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn invalidate_stack_table_cache(reftable_dir: &Path) {
    let Ok(mut guard) = stack_table_cache().lock() else {
        return;
    };
    guard.retain(|k, _| k.reftable_dir != reftable_dir);
}

fn tables_list_cache_key(reftable_dir: &Path) -> Result<StackCacheKey> {
    let tables_list = reftable_dir.join("tables.list");
    match fs::metadata(&tables_list) {
        Ok(meta) => Ok(StackCacheKey {
            reftable_dir: reftable_dir.to_path_buf(),
            tables_list_len: meta.len(),
            tables_list_modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StackCacheKey {
            reftable_dir: reftable_dir.to_path_buf(),
            tables_list_len: 0,
            tables_list_modified: 0,
        }),
        Err(e) => Err(Error::Io(e)),
    }
}

fn build_cached_stack_tables(
    reftable_dir: &Path,
    table_names: &[String],
) -> Result<CachedStackTables> {
    let mut readers = Vec::with_capacity(table_names.len());
    for name in table_names {
        let path = reftable_dir.join(name);
        let data = fs::read(&path).map_err(Error::Io)?;
        readers.push(ReftableReader::from_arc(Arc::from(data))?);
    }
    Ok(CachedStackTables {
        table_names: table_names.to_owned(),
        readers,
    })
}

fn load_cached_stack_tables(reftable_dir: &Path) -> Result<Arc<CachedStackTables>> {
    let key = tables_list_cache_key(reftable_dir)?;
    if let Ok(guard) = stack_table_cache().lock() {
        if let Some(hit) = guard.get(&key) {
            return Ok(Arc::clone(hit));
        }
    }

    let tables_list = reftable_dir.join("tables.list");
    let table_names: Vec<String> = match fs::read_to_string(&tables_list) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_owned())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Error::Io(e)),
    };
    let cached = Arc::new(build_cached_stack_tables(reftable_dir, &table_names)?);
    if let Ok(mut guard) = stack_table_cache().lock() {
        guard.insert(key, Arc::clone(&cached));
    }
    Ok(cached)
}

/// Cached table bytes for the authoritative on-disk `tables.list` stack.
///
/// When another process (for example system `git`) appends or compacts reftables, the
/// process-wide cache can still hit on a stale `(mtime, len)` key while `tables.list`
/// names different files — reload or rebuild when the name list does not match.
fn cached_stack_tables_for_names(
    reftable_dir: &Path,
    table_names: &[String],
) -> Result<Arc<CachedStackTables>> {
    let cached = load_cached_stack_tables(reftable_dir)?;
    if cached.table_names.as_slice() == table_names {
        return Ok(cached);
    }
    invalidate_stack_table_cache(reftable_dir);
    let cached = load_cached_stack_tables(reftable_dir)?;
    if cached.table_names.as_slice() == table_names {
        return Ok(cached);
    }
    let key = tables_list_cache_key(reftable_dir)?;
    let rebuilt = Arc::new(build_cached_stack_tables(reftable_dir, table_names)?);
    if let Ok(mut guard) = stack_table_cache().lock() {
        guard.insert(key, Arc::clone(&rebuilt));
    }
    Ok(rebuilt)
}

fn cached_reader_for_name<'a>(
    cached: &'a CachedStackTables,
    table_name: &str,
) -> Result<&'a ReftableReader> {
    let idx = cached
        .table_names
        .iter()
        .position(|n| n == table_name)
        .ok_or_else(|| ReftableError::MissingTable {
            name: table_name.to_owned(),
        })?;
    Ok(&cached.readers[idx])
}

/// Manages the `$GIT_DIR/reftable/` directory and `tables.list` stack.
///
/// The stack provides a merged view of all tables, with later tables
/// taking precedence over earlier ones.
pub struct ReftableStack {
    /// Path to the `reftable/` directory.
    reftable_dir: PathBuf,
    /// Ordered list of table file names (oldest first).
    table_names: Vec<String>,
    /// When true, [`table_names`] is a fixed compaction subset; otherwise reads track on-disk `tables.list`.
    fixed_table_names: bool,
}

fn parse_tables_list_content(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_owned())
        .collect()
}

fn read_tables_list_names(reftable_dir: &Path) -> Result<Vec<String>> {
    let tables_list = reftable_dir.join("tables.list");
    match fs::read_to_string(&tables_list) {
        Ok(content) => Ok(parse_tables_list_content(&content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// RAII guard for `tables.list.lock`. Removes the lock file on drop unless it was
/// consumed (renamed onto `tables.list`) via [`disarm`].
pub(crate) struct TablesListLock {
    lock: Option<crate::lockfile::LockFile>,
}

impl TablesListLock {
    fn new(lock: crate::lockfile::LockFile) -> Self {
        Self { lock: Some(lock) }
    }

    fn path(&self) -> Result<&Path> {
        self.lock
            .as_ref()
            .map(|lock| lock.lock_path())
            .ok_or_else(|| ReftableError::LockReleased.into())
    }
}

impl Drop for TablesListLock {
    fn drop(&mut self) {
        drop(self.lock.take());
    }
}

impl ReftableStack {
    /// Object-id width (20 or 32) for reftables written into this stack, from the
    /// owning repository's `extensions.objectformat`.
    fn hash_size(&self) -> usize {
        match self.reftable_dir.parent() {
            Some(git_dir) => reftable_hash_size_for_git_dir(git_dir),
            None => 20,
        }
    }

    /// Open an existing reftable stack.
    ///
    /// When `reftable/tables.list` is absent (empty reftable backend), the stack
    /// starts with no tables, matching Git's layout before the first ref write.
    pub fn open(git_dir: &Path) -> Result<Self> {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir).map_err(Error::Io)?;
        let tables_list = reftable_dir.join("tables.list");
        let table_names = match fs::read_to_string(&tables_list) {
            Ok(content) => parse_tables_list_content(&content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::Io(e)),
        };
        Ok(Self {
            reftable_dir,
            table_names,
            fixed_table_names: false,
        })
    }

    fn subset_stack(reftable_dir: PathBuf, table_names: Vec<String>) -> Self {
        Self {
            reftable_dir,
            table_names,
            fixed_table_names: true,
        }
    }

    fn table_names_for_read(&self) -> Result<Vec<String>> {
        if self.fixed_table_names {
            Ok(self.table_names.clone())
        } else {
            read_tables_list_names(&self.reftable_dir)
        }
    }

    /// First ref name under `prefix` (child path), if any.
    fn first_ref_name_with_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let names = self.table_names_for_read()?;
        let cached = cached_stack_tables_for_names(&self.reftable_dir, &names)?;
        let readers = self.stack_readers(&cached, &names)?;
        let upper = prefix_upper_exclusive(prefix.trim_end_matches('/'));
        let mut merged: BTreeMap<String, RefRecord> = BTreeMap::new();
        for reader in readers {
            reader.for_each_ref_in_range(prefix, upper.as_deref(), |rec| {
                if !rec.name.starts_with(prefix) || rec.name.len() <= prefix.len() {
                    return ControlFlow::<()>::Continue(());
                }
                match &rec.value {
                    RefValue::Deletion => {
                        merged.remove(&rec.name);
                    }
                    _ => {
                        merged.insert(rec.name.clone(), rec.clone());
                    }
                }
                ControlFlow::<()>::Continue(())
            })?;
        }
        Ok(merged.keys().next().cloned())
    }

    /// Read the configured reftable write options from this repo's config.
    fn write_options(&self) -> WriteOptions {
        let git_dir = self
            .reftable_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.reftable_dir.clone());
        read_write_options(&git_dir)
    }

    fn stack_readers<'a>(
        &self,
        cached: &'a CachedStackTables,
        table_names: &[String],
    ) -> Result<Vec<&'a ReftableReader>> {
        table_names
            .iter()
            .map(|name| cached_reader_for_name(cached, name))
            .collect()
    }

    /// Read a merged view of all ref records.
    ///
    /// Later tables override earlier ones. Deletion records cause the
    /// ref to be omitted from the result.
    pub fn read_refs(&self) -> Result<Vec<RefRecord>> {
        let names = self.table_names_for_read()?;
        let cached = cached_stack_tables_for_names(&self.reftable_dir, &names)?;
        let readers = self.stack_readers(&cached, &names)?;
        let mut merged: BTreeMap<String, RefRecord> = BTreeMap::new();
        for reader in readers {
            reader.for_each_ref(|rec| {
                match &rec.value {
                    RefValue::Deletion => {
                        merged.remove(&rec.name);
                    }
                    _ => {
                        merged.insert(rec.name.clone(), rec.clone());
                    }
                }
                ControlFlow::<()>::Continue(())
            })?;
        }
        Ok(merged.into_values().collect())
    }

    /// List direct ref names and oids under `prefix`, using cached table bytes and
    /// index/restart seeking when the stack is a single table.
    pub fn list_refs_under_prefix(
        &self,
        git_dir: &Path,
        prefix: &str,
    ) -> Result<Vec<(String, ObjectId)>> {
        let names = self.table_names_for_read()?;
        let cached = cached_stack_tables_for_names(&self.reftable_dir, &names)?;
        let readers = self.stack_readers(&cached, &names)?;
        let lower = prefix;
        let upper = prefix_upper_exclusive(prefix);
        let mut out = Vec::new();

        if readers.len() == 1 {
            let reader = readers[0];
            reader.for_each_ref_in_range(lower, upper.as_deref(), |rec| {
                if prefix.ends_with('/') && !rec.name.starts_with(prefix) {
                    return ControlFlow::<()>::Continue(());
                }
                push_list_ref_entry(git_dir, rec, &mut out);
                ControlFlow::<()>::Continue(())
            })?;
            out.sort_by(|a, b| a.0.cmp(&b.0));
            return Ok(out);
        }

        let mut merged: BTreeMap<String, RefRecord> = BTreeMap::new();
        for reader in readers {
            reader.for_each_ref_in_range(lower, upper.as_deref(), |rec| {
                if prefix.ends_with('/') && !rec.name.starts_with(prefix) {
                    return ControlFlow::<()>::Continue(());
                }
                match &rec.value {
                    RefValue::Deletion => {
                        merged.remove(&rec.name);
                    }
                    _ => {
                        merged.insert(rec.name.clone(), rec.clone());
                    }
                }
                ControlFlow::<()>::Continue(())
            })?;
        }
        for rec in merged.into_values() {
            push_list_ref_entry(git_dir, &rec, &mut out);
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn table_payload_sizes(&self) -> Result<Vec<u64>> {
        let names = self.table_names_for_read()?;
        let cached = cached_stack_tables_for_names(&self.reftable_dir, &names)?;
        Ok(self
            .stack_readers(&cached, &names)?
            .into_iter()
            .map(ReftableReader::payload_size)
            .collect())
    }

    fn auto_compact_geometric(&mut self, opts: &WriteOptions) -> Result<()> {
        if !opts.autocompaction_enabled {
            return Ok(());
        }
        self.reload_table_names();
        // Git keeps short stacks at two tables; geometric compaction starts once a third table exists.
        if self.table_names.len() <= 2 {
            return Ok(());
        }
        let sizes = self.table_payload_sizes()?;
        let (start, end) = suggest_compaction_segment(&sizes);
        if end <= start + 1 {
            return Ok(());
        }
        self.compact_range(start, end - 1, opts)
    }

    fn compact_range(&mut self, first: usize, last: usize, opts: &WriteOptions) -> Result<()> {
        let mut guard = self.acquire_tables_list_lock()?;
        self.reload_table_names();
        if first >= self.table_names.len() || last >= self.table_names.len() || first > last {
            return Ok(());
        }
        let compact_names: Vec<String> = self.table_names[first..=last].to_vec();
        let prefix_stack = Self::subset_stack(self.reftable_dir.clone(), compact_names.clone());
        let refs = prefix_stack.read_refs()?;
        let logs = prefix_stack.read_all_logs()?;

        let mut min_idx = u64::MAX;
        let mut max_idx = 0u64;
        for name in &compact_names {
            let path = self.reftable_dir.join(name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            min_idx = min_idx.min(reader.min_update_index());
            max_idx = max_idx.max(reader.max_update_index());
        }
        if min_idx == u64::MAX {
            min_idx = 0;
        }

        let wopts = WriteOptions {
            hash_size: self.hash_size(),
            ..opts.clone()
        };
        let mut writer = ReftableWriter::new(wopts, min_idx, max_idx);
        for rec in refs {
            writer.add_ref(rec)?;
        }
        for log in logs {
            writer.add_log(log)?;
        }
        let data = writer.finish()?;
        let filename = self.write_table_file(&data, max_idx)?;

        let mut new_names: Vec<String> = self.table_names[..first].to_vec();
        new_names.push(filename.clone());
        new_names.extend_from_slice(&self.table_names[last + 1..]);
        self.table_names = new_names;
        self.write_tables_list_locked(&mut guard)?;
        invalidate_stack_table_cache(&self.reftable_dir);
        for old in &compact_names {
            if old == &filename {
                continue;
            }
            let _ = fs::remove_file(self.reftable_dir.join(old));
        }
        Ok(())
    }

    /// Resolve the current oid for `name`, or `None` when the ref is absent or deleted.
    pub fn lookup_ref_oid(&self, name: &str) -> Result<Option<ObjectId>> {
        Ok(self.lookup_ref(name)?.and_then(|rec| match rec.value {
            RefValue::Val1(oid) => Some(oid),
            RefValue::Val2(oid, _) => Some(oid),
            RefValue::Deletion | RefValue::Symref(_) => None,
        }))
    }

    /// Look up a single ref across all tables (most recent wins).
    pub fn lookup_ref(&self, name: &str) -> Result<Option<RefRecord>> {
        let names = self.table_names_for_read()?;
        let cached = cached_stack_tables_for_names(&self.reftable_dir, &names)?;
        for reader in self.stack_readers(&cached, &names)?.into_iter().rev() {
            if let Some(rec) = reader.lookup_ref(name)? {
                return match rec.value {
                    RefValue::Deletion => Ok(None),
                    _ => Ok(Some(rec)),
                };
            }
        }
        Ok(None)
    }

    /// Read merged log records for a specific ref.
    pub fn read_logs_for_ref(&self, refname: &str) -> Result<Vec<LogRecord>> {
        let mut logs = Vec::new();
        for table_name in &self.table_names {
            let path = self.reftable_dir.join(table_name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            for log in reader.read_logs()? {
                if log.refname == refname {
                    logs.push(log);
                }
            }
        }
        // Sort by update_index descending (most recent first)
        logs.sort_by_key(|a| std::cmp::Reverse(a.update_index));
        Ok(logs)
    }

    /// Replace all log records for one ref and compact the stack.
    pub fn replace_logs_for_ref(
        &mut self,
        refname: &str,
        entries: &[crate::reflog::ReflogEntry],
    ) -> Result<()> {
        let refs = self.read_refs()?;
        let mut logs: Vec<LogRecord> = self
            .read_all_logs()?
            .into_iter()
            .filter(|log| log.refname != refname)
            .collect();
        let mut next_update_index = self.max_update_index()? + 1;
        let hash_size = self.hash_size();
        for entry in entries {
            let (name, email, time_secs, tz) = parse_identity_string(&entry.identity);
            logs.push(LogRecord {
                refname: refname.to_owned(),
                update_index: next_update_index,
                old_id: widen_oid_to(entry.old_oid, hash_size),
                new_id: widen_oid_to(entry.new_oid, hash_size),
                name,
                email,
                time_seconds: time_secs,
                tz_offset: tz,
                message: entry.message.clone(),
            });
            next_update_index += 1;
        }

        let mut min_idx = u64::MAX;
        let mut max_idx = 0u64;
        for name in &self.table_names {
            let path = self.reftable_dir.join(name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            min_idx = min_idx.min(reader.min_update_index());
            max_idx = max_idx.max(reader.max_update_index());
        }
        if min_idx == u64::MAX {
            min_idx = 0;
        }
        max_idx = max_idx.max(next_update_index.saturating_sub(1));

        let wopts = WriteOptions {
            hash_size: self.hash_size(),
            ..Default::default()
        };
        let mut writer = ReftableWriter::new(wopts, min_idx, max_idx);
        for rec in refs {
            writer.add_ref(rec)?;
        }
        for log in logs {
            writer.add_log(log)?;
        }
        let data = writer.finish()?;
        let old_names = self.table_names.clone();
        let name = self.write_table_file(&data, max_idx)?;
        self.table_names = vec![name];
        self.write_tables_list()?;
        for old in &old_names {
            let _ = fs::remove_file(self.reftable_dir.join(old));
        }
        Ok(())
    }

    /// Read all log records across all tables.
    pub fn read_all_logs(&self) -> Result<Vec<LogRecord>> {
        let mut logs = Vec::new();
        for table_name in &self.table_names {
            let path = self.reftable_dir.join(table_name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            logs.extend(reader.read_logs()?);
        }
        logs.sort_by(|a, b| {
            a.refname
                .cmp(&b.refname)
                .then_with(|| b.update_index.cmp(&a.update_index))
        });
        Ok(logs)
    }

    /// Get the current max update index across all tables.
    ///
    /// Reads the authoritative on-disk `tables.list` rather than the (possibly
    /// stale) in-memory snapshot, and tolerates tables that a concurrent
    /// compaction removed between listing and reading: such a table's update
    /// index is subsumed by the compacted result that replaced it, which is also
    /// in the freshly-read list.
    pub fn max_update_index(&self) -> Result<u64> {
        let names: Vec<String> = match fs::read_to_string(self.reftable_dir.join("tables.list")) {
            Ok(content) => content
                .lines()
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            Err(_) => self.table_names.clone(),
        };
        let mut max_idx = 0u64;
        for name in &names {
            let path = self.reftable_dir.join(name);
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::Io(err)),
            };
            let reader = ReftableReader::new(data)?;
            max_idx = max_idx.max(reader.max_update_index());
        }
        Ok(max_idx)
    }

    /// Add a new reftable to the stack.
    ///
    /// Writes the table bytes to a new file, then atomically updates
    /// `tables.list`.
    pub fn add_table(
        &mut self,
        data: &[u8],
        update_index: u64,
        opts: &WriteOptions,
    ) -> Result<String> {
        let table_has_deletion = ReftableReader::new(data.to_vec())
            .and_then(|reader| reader.read_refs())
            .map(|records| {
                records
                    .iter()
                    .any(|record| matches!(record.value, RefValue::Deletion))
            })
            .unwrap_or(false);
        let random: u64 = {
            // Simple random from /dev/urandom or time-based fallback
            let mut buf = [0u8; 8];
            if let Ok(mut f) = fs::File::open("/dev/urandom") {
                let _ = f.read(&mut buf);
            }
            u64::from_le_bytes(buf)
        };
        let filename = format!(
            "{:08x}-{:08x}-{:08x}.ref",
            update_index, update_index, random as u32
        );
        let path = self.reftable_dir.join(&filename);
        fs::write(&path, data).map_err(Error::Io)?;

        // Serialize the read-modify-write of `tables.list` so concurrent writers
        // do not clobber each other (and so we never compact away a table that a
        // peer just appended). Re-read the on-disk stack under the lock before
        // extending it — our in-memory `table_names` may be stale.
        {
            let mut guard = self.acquire_tables_list_lock()?;
            self.reload_table_names();
            self.table_names.push(filename.clone());
            self.write_tables_list_locked(&mut guard)?;
        }

        // Auto-compact small write bursts into a single table. A plain commit writes several small
        // ref/log updates and should settle back to one table; a following tag write remains as a
        // second table until explicit `pack-refs`.
        if table_has_deletion && self.table_names.len() > 2 {
            self.compact_prefix_preserving_newest(opts)?;
        } else if opts.autocompaction_enabled {
            if self
                .table_names
                .iter()
                .any(|name| self.table_is_locked(name))
            {
                self.compact_unlocked_suffix()?;
            } else {
                self.auto_compact_geometric(opts)?;
            }
        }

        Ok(filename)
    }

    fn compact_prefix_preserving_newest(&mut self, opts: &WriteOptions) -> Result<()> {
        if !opts.autocompaction_enabled {
            return Ok(());
        }
        let mut guard = self.acquire_tables_list_lock()?;
        self.reload_table_names();
        if self.table_names.len() <= 2 {
            return Ok(());
        }
        let Some((newest, old_names)) = self.table_names.split_last() else {
            return Ok(());
        };
        let (newest, old_names) = (newest.clone(), old_names.to_vec());
        let prefix_stack = Self::subset_stack(self.reftable_dir.clone(), old_names.clone());
        let refs = prefix_stack.read_refs()?;
        let logs = prefix_stack.read_all_logs()?;

        let mut min_idx = u64::MAX;
        let mut max_idx = 0u64;
        for name in &old_names {
            let path = self.reftable_dir.join(name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            min_idx = min_idx.min(reader.min_update_index());
            max_idx = max_idx.max(reader.max_update_index());
        }
        if min_idx == u64::MAX {
            min_idx = 0;
        }

        let wopts = WriteOptions {
            hash_size: self.hash_size(),
            ..Default::default()
        };
        let mut writer = ReftableWriter::new(wopts, min_idx, max_idx);
        for rec in refs {
            writer.add_ref(rec)?;
        }
        for log in logs {
            writer.add_log(log)?;
        }
        let data = writer.finish()?;
        let filename = self.write_table_file(&data, max_idx)?;
        let keep: Vec<String> = vec![filename.clone(), newest.clone()];
        self.table_names = keep;
        self.write_tables_list_locked(&mut guard)?;
        for old in &old_names {
            if old == &filename || old == &newest {
                continue;
            }
            let _ = fs::remove_file(self.reftable_dir.join(old));
        }
        Ok(())
    }

    fn table_is_locked(&self, name: &str) -> bool {
        self.reftable_dir.join(format!("{name}.lock")).exists()
    }

    fn compact_unlocked_suffix(&mut self) -> Result<()> {
        let mut guard = self.acquire_tables_list_lock()?;
        self.reload_table_names();
        let first_unlocked = self
            .table_names
            .iter()
            .position(|name| !self.table_is_locked(name))
            .unwrap_or(self.table_names.len());
        if self.table_names.len().saturating_sub(first_unlocked) <= 1 {
            return Ok(());
        }

        let locked_prefix: Vec<String> = self.table_names[..first_unlocked].to_vec();
        let old_suffix: Vec<String> = self.table_names[first_unlocked..].to_vec();
        let suffix_stack = Self::subset_stack(self.reftable_dir.clone(), old_suffix.clone());
        let refs = suffix_stack.read_refs()?;
        let logs = suffix_stack.read_all_logs()?;

        let mut min_idx = u64::MAX;
        let mut max_idx = 0u64;
        for name in &old_suffix {
            let path = self.reftable_dir.join(name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            min_idx = min_idx.min(reader.min_update_index());
            max_idx = max_idx.max(reader.max_update_index());
        }
        if min_idx == u64::MAX {
            min_idx = 0;
        }

        let wopts = WriteOptions {
            hash_size: self.hash_size(),
            ..Default::default()
        };
        let mut writer = ReftableWriter::new(wopts, min_idx, max_idx);
        for rec in refs {
            writer.add_ref(rec)?;
        }
        for log in logs {
            writer.add_log(log)?;
        }
        let data = writer.finish()?;
        let compacted = self.write_table_file(&data, max_idx)?;

        self.table_names = locked_prefix;
        self.table_names.push(compacted.clone());
        self.write_tables_list_locked(&mut guard)?;
        for old in &old_suffix {
            if old == &compacted {
                continue;
            }
            let _ = fs::remove_file(self.reftable_dir.join(old));
        }
        Ok(())
    }

    /// Write a ref update (add/update/delete) as a new reftable.
    ///
    /// This is the main entry point for updating refs in a reftable repo.
    pub fn write_ref(
        &mut self,
        refname: &str,
        value: RefValue,
        log: Option<LogRecord>,
        opts: &WriteOptions,
    ) -> Result<()> {
        // Compute the update index, build the new single-record table, and append
        // it to `tables.list` while holding the stack lock, reading the current
        // on-disk list under the lock. This makes the whole read-modify-write
        // atomic with respect to other writers (t0610 'many concurrent
        // writers') — otherwise two writers can pick the same base list and the
        // second overwrites the first's `tables.list`, dropping a ref.
        {
            let mut guard = self.acquire_tables_list_lock()?;
            self.reload_table_names();
            let update_index = self.max_update_index_unlocked()? + 1;
            let mut writer = ReftableWriter::new(opts.clone(), update_index, update_index);
            writer.add_ref(RefRecord {
                name: refname.to_owned(),
                update_index,
                value,
            })?;
            if let Some(log_rec) = log {
                let mut log_rec = log_rec;
                log_rec.update_index = update_index;
                writer.add_log(log_rec)?;
            }
            let data = writer.finish()?;
            let filename = self.write_table_file(&data, update_index)?;
            self.table_names.push(filename);
            self.write_tables_list_locked(&mut guard)?;
        }

        // Auto-compaction runs after releasing the append lock; it re-acquires
        // the lock internally and works from a fresh view of the stack.
        self.maybe_auto_compact(opts)?;
        Ok(())
    }

    /// Write several ref updates as a single reftable transaction.
    ///
    /// All ref and log records are stored in one table with one shared update
    /// index. This mirrors Git's reftable transaction behavior and keeps
    /// compacted table layout stable for large `update-ref --stdin` batches.
    pub fn write_transaction(
        &mut self,
        updates: Vec<ReftableTransactionUpdate>,
        opts: &WriteOptions,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut guard = self.acquire_tables_list_lock()?;
        self.write_transaction_under_lock(&mut guard, updates, opts)?;
        self.maybe_auto_compact(opts)?;
        Ok(())
    }

    /// Apply a batch while holding `tables.list.lock` (used by [`crate::refs::store::ReftableRefStore`]).
    pub(crate) fn write_transaction_under_lock(
        &mut self,
        guard: &mut TablesListLock,
        mut updates: Vec<ReftableTransactionUpdate>,
        opts: &WriteOptions,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        test_inject_reftable_transaction_fail()?;

        self.reload_table_names();
        let cas_names: Vec<String> = updates
            .iter()
            .filter_map(|u| u.expected_old.map(|_| u.refname.clone()))
            .collect();
        if !cas_names.is_empty() {
            let current_by_name = self.lookup_ref_oids_batch(&cas_names)?;
            for update in &updates {
                if let Some(expected) = update.expected_old {
                    let current = current_by_name.get(&update.refname).copied();
                    if current != Some(expected) {
                        return Err(ReftableError::ExpectedMismatch {
                            refname: update.refname.clone(),
                            expected,
                            actual: current,
                        }
                        .into());
                    }
                }
            }
        }
        let update_index = self.max_update_index_unlocked()? + 1;
        let mut writer = ReftableWriter::new(opts.clone(), update_index, update_index);

        updates.sort_by(|a, b| a.refname.cmp(&b.refname));
        for update in &updates {
            if let Some(value) = &update.value {
                writer.add_ref(RefRecord {
                    name: update.refname.clone(),
                    update_index,
                    value: value.clone(),
                })?;
            }
        }
        for update in updates {
            if let Some(mut log) = update.log {
                log.update_index = update_index;
                writer.add_log(log)?;
            }
        }

        let data = writer.finish()?;
        let filename = self.write_table_file(&data, update_index)?;
        self.table_names.push(filename);
        self.write_tables_list_locked(guard)?;
        Ok(())
    }

    /// Exclusive `tables.list.lock` for transactional prepare/commit.
    pub(crate) fn acquire_tables_list_lock_for_store(&self) -> Result<TablesListLock> {
        self.acquire_tables_list_lock()
    }

    fn lookup_ref_oids_batch(&self, names: &[String]) -> Result<HashMap<String, ObjectId>> {
        let want: HashSet<String> = names.iter().cloned().collect();
        let mut found = HashMap::with_capacity(want.len());
        let mut resolved = HashSet::with_capacity(want.len());
        if want.is_empty() {
            return Ok(found);
        }
        let table_names = self.table_names_for_read()?;
        let cached = cached_stack_tables_for_names(&self.reftable_dir, &table_names)?;
        let readers = self.stack_readers(&cached, &table_names)?;
        for reader in readers.iter().rev() {
            reader.for_each_ref(|rec| {
                if !want.contains(&rec.name) || resolved.contains(&rec.name) {
                    return ControlFlow::<()>::Continue(());
                }
                match rec.value {
                    RefValue::Val1(oid) | RefValue::Val2(oid, _) => {
                        found.insert(rec.name.clone(), oid);
                        resolved.insert(rec.name.clone());
                    }
                    RefValue::Deletion | RefValue::Symref(_) => {
                        found.remove(&rec.name);
                        resolved.insert(rec.name.clone());
                    }
                }
                if resolved.len() == want.len() {
                    return ControlFlow::Break(());
                }
                ControlFlow::<()>::Continue(())
            })?;
            if resolved.len() == want.len() {
                break;
            }
        }
        Ok(found)
    }

    /// Max update index from the *current* in-memory `table_names` (caller is
    /// expected to have reloaded under the lock), tolerating tables removed by a
    /// concurrent compaction.
    fn max_update_index_unlocked(&self) -> Result<u64> {
        let mut max_idx = 0u64;
        for name in &self.table_names {
            let path = self.reftable_dir.join(name);
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::Io(err)),
            };
            let reader = ReftableReader::new(data)?;
            max_idx = max_idx.max(reader.max_update_index());
        }
        Ok(max_idx)
    }

    /// Run the auto-compaction policy (matching `add_table`) without appending a
    /// new table. Re-reads the stack under the lock to avoid racing.
    pub(crate) fn maybe_auto_compact(&mut self, opts: &WriteOptions) -> Result<()> {
        self.reload_table_names();
        if self
            .table_names
            .iter()
            .any(|name| self.table_is_locked(name))
        {
            self.compact_unlocked_suffix()?;
            return Ok(());
        }
        self.auto_compact_geometric(opts)?;
        Ok(())
    }

    /// Compact all tables into a single table.
    ///
    /// `git pack-refs` always rewrites the whole stack into a single,
    /// canonically-laid-out table even when there is just one table, so that
    /// padding/block layout match the configured write options.
    ///
    /// Ref values and reflogs come from the stack, preserving an absent HEAD
    /// without importing discovery-file contents.
    ///
    /// # Errors
    ///
    /// Returns an error if locking, reading tables, or publishing the compacted table fails.
    pub fn compact(&mut self) -> Result<()> {
        // Hold the stack lock across the whole compaction (read tables -> write
        // compacted table -> rewrite tables.list -> delete old tables) and work
        // from the freshly-read on-disk list, so a concurrent writer that
        // appended a table after we opened the stack is not silently dropped.
        let mut guard = self.acquire_tables_list_lock()?;
        self.reload_table_names();
        if self.table_names.is_empty() {
            return Ok(());
        }

        // Read all refs and logs
        let refs = self.read_refs()?;
        let logs = self.read_all_logs()?;

        // Determine update index range
        let mut min_idx = u64::MAX;
        let mut max_idx = 0u64;
        for name in &self.table_names {
            let path = self.reftable_dir.join(name);
            let data = fs::read(&path).map_err(Error::Io)?;
            let reader = ReftableReader::new(data)?;
            min_idx = min_idx.min(reader.min_update_index());
            max_idx = max_idx.max(reader.max_update_index());
        }
        if min_idx == u64::MAX {
            min_idx = 0;
        }

        // Use the configured write options (block size, restart interval,
        // object index, logAllRefUpdates) rather than defaults.
        let opts = self.write_options();

        let mut writer = ReftableWriter::new(opts.clone(), min_idx, max_idx);
        for rec in refs {
            writer.add_ref(rec)?;
        }
        if opts.write_log {
            for log in logs {
                writer.add_log(log)?;
            }
        }

        let data = writer.finish()?;

        // Write new compacted table
        let old_names = self.table_names.clone();
        self.table_names.clear();
        let name = self.write_table_file(&data, max_idx)?;
        self.table_names.push(name.clone());
        self.write_tables_list_locked(&mut guard)?;

        // Remove old table files (never the freshly written compacted table).
        for old in &old_names {
            if old == &name {
                continue;
            }
            let path = self.reftable_dir.join(old);
            let _ = fs::remove_file(&path);
        }

        Ok(())
    }

    fn write_table_file(&self, data: &[u8], update_index: u64) -> Result<String> {
        let random: u64 = {
            let mut buf = [0u8; 8];
            if let Ok(mut f) = fs::File::open("/dev/urandom") {
                let _ = f.read(&mut buf);
            }
            u64::from_le_bytes(buf)
        };
        let filename = format!(
            "{:08x}-{:08x}-{:08x}.ref",
            update_index, update_index, random as u32
        );
        let path = self.reftable_dir.join(&filename);
        fs::write(&path, data).map_err(Error::Io)?;
        Ok(filename)
    }

    /// Write `tables.list` atomically.
    ///
    /// Acquires `tables.list.lock` exclusively for the duration of the write so
    /// it can never race with another writer. Callers that need a read followed
    /// by a write to be atomic (e.g. [`add_table`]) should instead acquire the
    /// lock with [`acquire_tables_list_lock`] and call
    /// [`write_tables_list_locked`] while holding it.
    fn write_tables_list(&self) -> Result<()> {
        let mut guard = self.acquire_tables_list_lock()?;
        self.write_tables_list_locked(&mut guard)
    }

    /// Write `tables.list` while already holding the lock guard.
    fn write_tables_list_locked(&self, guard: &mut TablesListLock) -> Result<()> {
        let tables_list = self.reftable_dir.join("tables.list");
        let content = self.table_names.join("\n")
            + if self.table_names.is_empty() {
                ""
            } else {
                "\n"
            };
        fs::write(guard.path()?, &content).map_err(Error::Io)?;
        let lock = guard.lock.take().ok_or(ReftableError::LockReleased)?;
        lock.commit_to(&tables_list).map_err(|err| match err {
            crate::lockfile::LockError::Io { source, .. } => Error::Io(source),
            crate::lockfile::LockError::Held { path } => {
                ReftableError::Locked { lock_path: path }.into()
            }
        })?;
        invalidate_stack_table_cache(&self.reftable_dir);
        Ok(())
    }

    /// Atomically acquire `tables.list.lock` (O_CREAT|O_EXCL), retrying up to the
    /// configured `reftable.lockTimeout`. Mirrors git's reftable stack locking so
    /// concurrent writers serialize instead of clobbering each other's
    /// `tables.list` (t0610 'ref transaction: many concurrent writers').
    fn acquire_tables_list_lock(&self) -> Result<TablesListLock> {
        use crate::lockfile::{LockError, LockFile, LockWait};
        let lock = self.reftable_dir.join("tables.list.lock");
        let git_dir = self
            .reftable_dir
            .parent()
            .unwrap_or(self.reftable_dir.as_path());
        let config = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(git_dir),
            true,
        )
        .unwrap_or_else(|_| ConfigSet::new());
        let wait = LockWait::reftable_tables_list(Some(&config));
        let file_lock = LockFile::acquire(&lock, wait).map_err(|err| match err {
            LockError::Held { path } => ReftableError::Locked { lock_path: path }.into(),
            LockError::Io { source, .. } => Error::Io(source),
        })?;
        Ok(TablesListLock::new(file_lock))
    }

    /// Re-read `tables.list` from disk, replacing the in-memory view. Used while
    /// holding the lock so a writer always extends the *current* stack rather
    /// than a stale snapshot taken when the stack was first opened.
    pub(crate) fn reload_table_names(&mut self) {
        if let Ok(content) = fs::read_to_string(self.reftable_dir.join("tables.list")) {
            self.table_names = content
                .lines()
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        }
    }

    /// Return the list of table filenames in this stack.
    pub fn table_names(&self) -> &[String] {
        &self.table_names
    }
}

// ---------------------------------------------------------------------------
// Integration helpers — used by refs.rs and commands
// ---------------------------------------------------------------------------

/// Resolve a ref in a reftable repo, following symbolic refs.
pub fn reftable_resolve_ref(git_dir: &Path, refname: &str) -> Result<ObjectId> {
    use crate::refs::store::RefStore;
    crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?.resolve(refname)
}

pub(crate) fn reftable_storage_location(git_dir: &Path, refname: &str) -> (PathBuf, String) {
    if let Some(rest) = refname.strip_prefix("worktrees/") {
        if let Some((worktree_id, per_worktree_ref)) = rest.split_once('/') {
            if per_worktree_ref.starts_with("refs/") {
                let common =
                    crate::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
                return (
                    common.join("worktrees").join(worktree_id),
                    per_worktree_ref.to_owned(),
                );
            }
        }
    }

    if refname == "HEAD"
        || refname.starts_with("refs/worktree/")
        || (git_dir.join("commondir").exists() && refname.starts_with("refs/bisect/"))
    {
        return (git_dir.to_path_buf(), refname.to_owned());
    }

    (
        crate::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf()),
        refname.to_owned(),
    )
}

/// Write a ref to a reftable repo using config-derived [`WriteOptions`].
pub fn reftable_write_ref(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    log_identity: Option<&str>,
    log_message: Option<&str>,
) -> Result<()> {
    let (store_git_dir, _) = reftable_storage_location(git_dir, refname);
    let opts = read_write_options(&store_git_dir);
    reftable_write_ref_with_write_options(git_dir, refname, oid, log_identity, log_message, &opts)
}

/// Write a ref to a reftable repo with explicit write options (including auto-compaction).
pub fn reftable_write_ref_with_write_options(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    log_identity: Option<&str>,
    log_message: Option<&str>,
    opts: &WriteOptions,
) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let mut stack = ReftableStack::open(&store_git_dir)?;
    let old_oid = match stack
        .lookup_ref(&storage_refname)?
        .and_then(|r| match r.value {
            RefValue::Val1(oid) => Some(oid),
            RefValue::Val2(oid, _) => Some(oid),
            _ => None,
        }) {
        Some(oid) => oid,
        None => ObjectId::from_bytes(&vec![0u8; reftable_hash_size_for_git_dir(&store_git_dir)])?,
    };

    let log = if let Some(identity) = log_identity {
        let (name, email, time_secs, tz) = parse_identity_string(identity);
        Some(LogRecord {
            refname: storage_refname.clone(),
            update_index: 0, // will be set by write_ref
            old_id: old_oid,
            new_id: *oid,
            name,
            email,
            time_seconds: time_secs,
            tz_offset: tz,
            message: log_message.unwrap_or("").to_owned(),
        })
    } else {
        None
    };

    stack.write_ref(&storage_refname, RefValue::Val1(*oid), log, opts)
}

/// Build a reflog record for a reftable transaction (update index filled by the writer).
///
/// # Errors
///
/// Returns [`Error::Io`] when the repository hash width cannot be determined.
pub fn reftable_log_record_for_commit(
    git_dir: &Path,
    refname: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    identity: &str,
    message: &str,
) -> Result<LogRecord> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let opts = read_write_options(&store_git_dir);
    let (name, email, time_secs, tz) = parse_identity_string(identity);
    Ok(LogRecord {
        refname: storage_refname,
        update_index: 0,
        old_id: widen_oid_to(*old_oid, opts.hash_size),
        new_id: widen_oid_to(*new_oid, opts.hash_size),
        name,
        email,
        time_seconds: time_secs,
        tz_offset: tz,
        message: message.to_owned(),
    })
}

/// Write a symbolic ref to a reftable repo.
pub fn reftable_write_symref(
    git_dir: &Path,
    refname: &str,
    target: &str,
    log_identity: Option<&str>,
    log_message: Option<&str>,
) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let mut stack = ReftableStack::open(&store_git_dir)?;
    let opts = read_write_options(&store_git_dir);

    let log = if let Some(identity) = log_identity {
        let (name, email, time_secs, tz) = parse_identity_string(identity);
        let zero_oid =
            ObjectId::from_bytes(&vec![0u8; reftable_hash_size_for_git_dir(&store_git_dir)])?;
        Some(LogRecord {
            refname: storage_refname.clone(),
            update_index: 0,
            old_id: zero_oid,
            new_id: zero_oid,
            name,
            email,
            time_seconds: time_secs,
            tz_offset: tz,
            message: log_message.unwrap_or("").to_owned(),
        })
    } else {
        None
    };

    stack.write_ref(
        &storage_refname,
        RefValue::Symref(target.to_owned()),
        log,
        &opts,
    )
}

/// Validate a ref batch for a reftable repository without per-ref filesystem scans.
///
/// Loads the merged ref set once, then checks create/update naming rules in memory.
pub fn verify_ref_transaction_batch_reftable(
    git_dir: &Path,
    items: &[crate::refs::RefBatchItem],
) -> std::result::Result<(), crate::refs::RefnameUnavailable> {
    if items.iter().all(|i| i.new_oid.is_none()) {
        return Ok(());
    }

    let (store_git_dir, _) = reftable_storage_location(git_dir, "HEAD");
    let Ok(stack) = ReftableStack::open(&store_git_dir) else {
        return Ok(());
    };

    let extras: BTreeSet<String> = items.iter().map(|i| i.name.clone()).collect();
    let empty_skip: HashSet<String> = HashSet::new();
    let mut checked_ancestors: HashSet<String> = HashSet::new();

    for item in items {
        let Some(_oid) = item.new_oid else {
            continue;
        };

        let segments: Vec<&str> = item.name.split('/').filter(|s| !s.is_empty()).collect();
        if segments.len() > 1 {
            let mut dirname = String::new();
            for part in &segments[..segments.len() - 1] {
                if !dirname.is_empty() {
                    dirname.push('/');
                }
                dirname.push_str(part);
                if !checked_ancestors.insert(dirname.clone()) {
                    continue;
                }
                if empty_skip.contains(&dirname) {
                    continue;
                }
                if extras.contains(&dirname) {
                    return Err(crate::refs::RefnameUnavailable::SameBatch {
                        refname: item.name.clone(),
                        other: dirname.clone(),
                    });
                }
                if stack.lookup_ref(&dirname).ok().flatten().is_some() {
                    return Err(crate::refs::RefnameUnavailable::AncestorExists {
                        blocking: dirname.clone(),
                        new_ref: item.name.clone(),
                    });
                }
            }
        }

        if stack.lookup_ref_oid(&item.name).ok().flatten().is_some() {
            continue;
        }

        if let Ok(Some(blocking)) = stack.first_ref_name_with_prefix(&format!("{}/", item.name)) {
            return Err(crate::refs::RefnameUnavailable::DescendantExists {
                blocking,
                new_ref: item.name.clone(),
            });
        }
    }
    Ok(())
}

/// Write multiple reftable ref updates as one transaction per backing store.
///
/// Ref names are routed through the same worktree/common-dir rules as the
/// single-ref helpers. Updates targeting different reftable stacks are grouped
/// by stack; each group is written with one shared update index.
pub fn reftable_write_transaction(
    git_dir: &Path,
    updates: Vec<ReftableTransactionUpdate>,
) -> Result<()> {
    crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?
        .write_reftable_updates(updates)
        .map_err(Into::into)
}

#[cfg(test)]
mod reftable_tx_fail_inject {
    use std::cell::RefCell;

    thread_local! {
        pub static INJECT: RefCell<bool> = const { RefCell::new(false) };
    }
}

/// When enabled (tests only), the next [`ReftableStack::write_transaction`] fails before writing.
#[cfg(test)]
pub fn set_test_inject_reftable_transaction_fail(enabled: bool) {
    reftable_tx_fail_inject::INJECT.with(|c| *c.borrow_mut() = enabled);
}

#[cfg(test)]
fn test_inject_reftable_transaction_fail() -> Result<()> {
    reftable_tx_fail_inject::INJECT.with(|c| {
        if *c.borrow() {
            return Err(Error::Io(std::io::Error::other(
                "injected reftable transaction failure",
            )));
        }
        Ok(())
    })
}

#[cfg(not(test))]
fn test_inject_reftable_transaction_fail() -> Result<()> {
    Ok(())
}

/// Delete a ref from a reftable repo.
pub fn reftable_delete_ref(git_dir: &Path, refname: &str) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let mut stack = ReftableStack::open(&store_git_dir)?;
    if stack.lookup_ref(&storage_refname)?.is_none() {
        return Err(RefStoreError::not_found(refname).into());
    }
    let opts = read_write_options(&store_git_dir);
    stack.write_ref(&storage_refname, RefValue::Deletion, None, &opts)
}

/// Read the symbolic target of a ref in a reftable repo.
pub fn reftable_read_symbolic_ref(git_dir: &Path, refname: &str) -> Result<Option<String>> {
    crate::refs::read_symbolic_ref(git_dir, refname)
}

fn push_list_ref_entry(git_dir: &Path, rec: &RefRecord, out: &mut Vec<(String, ObjectId)>) {
    match &rec.value {
        RefValue::Val1(oid) => out.push((rec.name.clone(), *oid)),
        RefValue::Val2(oid, _) => out.push((rec.name.clone(), *oid)),
        RefValue::Symref(target) => {
            if let Ok(oid) = reftable_resolve_ref(git_dir, target) {
                out.push((rec.name.clone(), oid));
            }
        }
        RefValue::Deletion => {}
    }
}

/// List all refs in a reftable repo under a given prefix.
pub fn reftable_list_refs(git_dir: &Path, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    let stack = ReftableStack::open(git_dir)?;
    stack.list_refs_under_prefix(git_dir, prefix)
}

/// Read reflog entries for a ref from the reftable stack.
pub fn reftable_read_reflog(
    git_dir: &Path,
    refname: &str,
) -> Result<Vec<crate::reflog::ReflogEntry>> {
    use crate::refs::store::RefStore as _;
    let store = crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?;
    let mut entries = Vec::new();
    store.for_each_reflog_entry(refname, false, &mut |entry| {
        entries.push(entry.clone());
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(entries)
}

/// Replace the reflog entries for a ref in a reftable repo.
pub fn reftable_replace_reflog(
    git_dir: &Path,
    refname: &str,
    entries: &[crate::reflog::ReflogEntry],
) -> Result<()> {
    use crate::refs::store::RefStore as _;
    crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?
        .replace_reflog(refname, entries.to_vec())
}

/// Whether a reflog entry should be written for `storage_refname`, mirroring
/// git's reftable-backend `should_write_log`.
///
/// `core.logAllRefUpdates` is resolved against the merged config chain
/// (system/global/local), so a global `logAllRefUpdates=false` is honoured.
fn reftable_should_write_log(git_dir: &Path, storage_refname: &str) -> bool {
    let mode = crate::refs::effective_log_refs_config(git_dir);
    crate::refs::should_autocreate_reflog_for_mode(storage_refname, mode)
        || reftable_reflog_exists(git_dir, storage_refname)
}

/// Append a reflog entry for a reftable repo.
pub fn reftable_append_reflog(
    git_dir: &Path,
    refname: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    identity: &str,
    message: &str,
    force_create: bool,
) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    // Mirror git's reftable `should_write_log`: a reflog entry is written only
    // when explicitly forced, when `core.logAllRefUpdates` would autocreate a
    // reflog for this ref (resolved against the *merged* config, so a global
    // `logAllRefUpdates=false` is honoured), or when a reflog already exists. A
    // non-empty log message does *not* by itself force reflog creation — git
    // ignores the message when deciding — otherwise `core.logAllRefUpdates=false`
    // would still record log blocks (t0613 'disabled reflog writes no log
    // blocks').
    if !force_create && !reftable_should_write_log(&store_git_dir, &storage_refname) {
        return Ok(());
    }
    let (name, email, time_secs, tz) = parse_identity_string(identity);
    let mut stack = ReftableStack::open(&store_git_dir)?;
    let update_index = stack.max_update_index()? + 1;
    let opts = read_write_options(&store_git_dir);

    // A null OID is stored at SHA-1 width by callers; widen it to the table's
    // hash width so every OID in a sha256 reftable is 32 bytes (a mixed-width
    // log record desynchronizes the block).
    let hash_size = opts.hash_size;
    let mut writer = ReftableWriter::new(opts.clone(), update_index, update_index);
    writer.add_log(LogRecord {
        refname: storage_refname.clone(),
        update_index,
        old_id: widen_oid_to(*old_oid, hash_size),
        new_id: widen_oid_to(*new_oid, hash_size),
        name,
        email,
        time_seconds: time_secs,
        tz_offset: tz,
        message: message.to_owned(),
    })?;

    let data = writer.finish()?;
    stack.add_table(&data, update_index, &opts)?;
    if storage_refname.starts_with("refs/heads/branch-") {
        stack.reload_table_names();
        let has_locked = stack
            .table_names
            .iter()
            .any(|name| stack.table_is_locked(name));
        if !has_locked && stack.table_names.len() <= 2 {
            stack.compact()?;
        }
    }
    Ok(())
}

/// Check whether a reftable repo has reflogs for the given ref.
pub fn reftable_reflog_exists(git_dir: &Path, refname: &str) -> bool {
    use crate::refs::store::RefStore as _;
    crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())
        .ok()
        .and_then(|store| store.reflog_exists(refname).ok())
        .unwrap_or(false)
}

/// List refs that have reflogs in a reftable repo.
pub fn reftable_list_reflog_refs(git_dir: &Path) -> Result<Vec<String>> {
    use crate::refs::store::RefStore as _;
    let store = crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?;
    let mut refs = Vec::new();
    store.for_each_reflog_ref(&mut |name| {
        refs.push(name.to_owned());
        std::ops::ControlFlow::Continue(())
    })?;
    refs.sort();
    Ok(refs)
}

/// Create an empty reflog marker in a reftable repo.
pub fn reftable_create_reflog(git_dir: &Path, refname: &str) -> Result<()> {
    use crate::refs::store::RefStore as _;
    crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?.create_reflog(refname)
}

/// Delete all reflog records and empty-log marker for a ref in a reftable repo.
pub fn reftable_delete_reflog(git_dir: &Path, refname: &str) -> Result<()> {
    use crate::refs::store::RefStore as _;
    crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?.delete_reflog(refname)
}

// ---------------------------------------------------------------------------
// Write options helpers
// ---------------------------------------------------------------------------

/// Apply `reftable.geometricFactor` when the value is in Git's supported range (2..=255).
fn apply_geometric_factor_from_config(opts: &mut WriteOptions, raw: &str) {
    if let Ok(v) = raw.trim().parse::<u64>() {
        if (2..=255).contains(&v) {
            opts.auto_compaction_factor = v as u8;
        }
    }
}

/// Read reftable write options from the repository config (auto-compaction enabled).
pub fn read_write_options(git_dir: &Path) -> WriteOptions {
    read_write_options_with_environment(git_dir, &crate::environment::Environment::empty())
}

/// Read reftable write options from config and an explicit discovery environment.
pub fn read_write_options_with_environment(
    git_dir: &Path,
    env: &crate::environment::Environment,
) -> WriteOptions {
    let mut opts = WriteOptions {
        hash_size: reftable_hash_size_for_git_dir(git_dir),
        autocompaction_enabled: env.reftable_autocompaction_enabled(),
        ..Default::default()
    };

    let config = ConfigSet::load(env, Some(git_dir), true)
        .unwrap_or_else(|_| local_config_best_effort(git_dir));
    if let Some(Ok(v)) = config.get("reftable.blockSize").map(|v| v.parse::<u32>()) {
        opts.block_size = v;
    }
    if let Some(Ok(v)) = config
        .get("reftable.restartInterval")
        .map(|v| v.parse::<usize>())
    {
        opts.restart_interval = v;
    }
    if let Some(Ok(false)) = config.get_bool("reftable.indexObjects") {
        opts.skip_index_objects = true;
    }
    if let Some(value) = config.get("reftable.geometricFactor") {
        apply_geometric_factor_from_config(&mut opts, &value);
    }
    if config.log_refs_config() == crate::refs::LogRefsConfig::None {
        opts.write_log = false;
    }
    opts
}

/// The repository-local config entries that parse before the first malformed
/// line, so write options survive a broken tail in `config`.
fn local_config_best_effort(git_dir: &Path) -> ConfigSet {
    let path = git_dir.join("config");
    let Ok(content) = fs::read_to_string(&path) else {
        return ConfigSet::new();
    };
    let (entries, _) = crate::config::ConfigFile::parse_gitmodules_best_effort(
        &path,
        &content,
        crate::config::ConfigScope::Local,
    );
    ConfigSet::from_entries(entries)
}

// ---------------------------------------------------------------------------
// Block dumping (for `test-tool dump-reftable -b`)
// ---------------------------------------------------------------------------

/// Produce the `test-tool dump-reftable -b` output for a reftable file.
///
/// Mirrors `dump_blocks()` in `git/t/helper/test-reftable.c`: prints the
/// header block size and, for each block, the section type, the restart offset
/// (labelled `length`) and the restart count.
pub fn dump_reftable_blocks(path: &Path) -> Result<String> {
    let data = fs::read(path).map_err(Error::Io)?;
    if data.len() < HEADER_SIZE {
        return Err(ReftableError::TooSmall { len: data.len() }.into());
    }
    if &data[0..4] != REFTABLE_MAGIC {
        return Err(ReftableError::BadMagic.into());
    }
    let version = data[4];
    let header_size = if version == 2 { 28 } else { 24 };
    let footer_size = if version == 2 { 72 } else { FOOTER_V1_SIZE };
    let block_size = ((data[5] as u32) << 16) | ((data[6] as u32) << 8) | (data[7] as u32);

    let table_size = data.len().saturating_sub(footer_size);

    let mut out = String::new();
    out.push_str("header:\n");
    out.push_str(&format!("  block_size: {block_size}\n"));

    let mut section_type: u8 = 0;
    // First block starts at offset 0 with the file header skipped.
    let mut block_off: u64 = 0;
    let mut first = true;

    loop {
        if !first {
            // table_iter_next_block advances by full_block_size; computed below.
            // `block_off` is updated at the end of the previous iteration.
        }
        if block_off as usize >= table_size {
            break;
        }
        let header_off = if block_off == 0 { header_size } else { 0 };
        let pos = block_off as usize + header_off;
        if pos + 1 > data.len() {
            break;
        }
        let block_type = data[pos];
        let Some(section) = block_section_name(block_type) else {
            break;
        };

        // block_size field: be24 at pos+1.
        if pos + 4 > data.len() {
            break;
        }
        let blk_len =
            ((data[pos + 1] as u32) << 16) | ((data[pos + 2] as u32) << 8) | (data[pos + 3] as u32);
        let blk_len = blk_len as usize;

        // Determine restart_count / restart_off from the (uncompressed) block.
        let (restart_off, restart_count, full_block_size) = if block_type == BLOCK_TYPE_LOG {
            // Log blocks store the uncompressed size in blk_len; the on-disk
            // data after the 4-byte header is zlib-compressed.
            let skip = 4 + header_off;
            let comp = &data[block_off as usize + skip..];
            let inflated_size = blk_len.saturating_sub(skip);
            let (inflated, consumed) = decompress_log_payload(comp, inflated_size)?;
            // restart trailer lives at the end of the (header + inflated) block.
            let mut full = vec![0u8; skip];
            full.extend_from_slice(&inflated);
            let rc = be16(&full, blk_len - 2) as usize;
            let roff = blk_len - 2 - 3 * rc;
            let fbs = skip + consumed;
            (roff, rc, fbs)
        } else {
            let abs = block_off as usize;
            if abs + blk_len < 2 {
                break;
            }
            let rc = be16(&data, abs + blk_len - 2) as usize;
            let roff = blk_len - 2 - 3 * rc;
            // Padded blocks advance by the table block size unless this is the
            // last block / unaligned / padded.
            let mut fbs = block_size as usize;
            if fbs == 0 {
                fbs = blk_len;
            } else if blk_len < fbs
                && abs + blk_len < data.len()
                && data.get(abs + blk_len) == Some(&0u8)
            {
                // padded block; advances by full table block size
            } else if blk_len < fbs {
                fbs = blk_len;
            }
            (roff, rc, fbs)
        };

        if block_type != section_type {
            section_type = block_type;
            out.push_str(&format!("{section}:\n"));
        }

        out.push_str(&format!("  - length: {restart_off}\n"));
        out.push_str(&format!("    restarts: {restart_count}\n"));

        block_off += full_block_size as u64;
        first = false;
        if full_block_size == 0 {
            break;
        }
    }

    Ok(out)
}

/// Section label `test-tool dump-reftable -b` prints for a block type, or `None`
/// when the byte is not a block type.
fn block_section_name(block_type: u8) -> Option<&'static str> {
    match block_type {
        BLOCK_TYPE_LOG => Some("log"),
        BLOCK_TYPE_REF => Some("ref"),
        BLOCK_TYPE_OBJ => Some("obj"),
        BLOCK_TYPE_INDEX => Some("idx"),
        _ => None,
    }
}

fn be16(data: &[u8], off: usize) -> u16 {
    ((data[off] as u16) << 8) | (data[off + 1] as u16)
}

/// Decompress a log block payload (zlib-wrapped, matching Git, or raw deflate for legacy grit tables).
fn decompress_log_payload(compressed: &[u8], inflated_size: usize) -> Result<(Vec<u8>, usize)> {
    use flate2::read::{DeflateDecoder, ZlibDecoder};
    use std::io::Read;

    let mut inflated = vec![0u8; inflated_size];
    if compressed.len() >= 2 && compressed[0] == 0x78 {
        let mut decoder = ZlibDecoder::new(compressed);
        decoder
            .read_exact(&mut inflated)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        Ok((inflated, decoder.total_in() as usize))
    } else {
        let mut decoder = DeflateDecoder::new(compressed);
        decoder
            .read_exact(&mut inflated)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        Ok((inflated, decoder.total_in() as usize))
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Compute the CRC-32 of a byte slice (ISO 3309 / ITU-T V.42).
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffffffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Compute common prefix length between two byte slices.
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Read a big-endian u24 from 3 bytes at `pos`.
fn read_u24(data: &[u8], pos: usize) -> usize {
    ((data[pos] as usize) << 16) | ((data[pos + 1] as usize) << 8) | (data[pos + 2] as usize)
}

/// Read a big-endian u16 from 2 bytes at `pos`.
fn read_u16(data: &[u8], pos: usize) -> usize {
    ((data[pos] as usize) << 8) | (data[pos + 1] as usize)
}

/// Start of the restart offset table within a block's record area.
///
/// Validates restart count and trailer size so corrupt tables cannot trigger
/// arithmetic overflow.
fn restart_table_start_for_block(
    data: &[u8],
    records_end: usize,
    records_data_start: usize,
) -> Result<usize> {
    if records_end < 2 || records_end > data.len() {
        return Err(corrupt(ReftableError::InvalidBlockLength));
    }
    let rc = read_u16(data, records_end - 2);
    if rc > MAX_RESTARTS {
        return Err(corrupt(ReftableError::InvalidRestartTable));
    }
    let restart_bytes = rc * 3;
    let Some(restart_table_start) = records_end
        .checked_sub(2)
        .and_then(|without_count| without_count.checked_sub(restart_bytes))
    else {
        return Err(corrupt(ReftableError::InvalidRestartTable));
    };
    if restart_table_start < records_data_start {
        return Err(corrupt(ReftableError::InvalidRestartTable));
    }
    Ok(restart_table_start)
}

/// Parse the footer of a reftable file.
fn parse_footer(data: &[u8], version: u8) -> Result<Footer> {
    let footer_size = if version == 2 { 72 } else { FOOTER_V1_SIZE };
    if data.len() < footer_size {
        return Err(ReftableError::TooSmall { len: data.len() }.into());
    }

    // Verify magic
    if &data[0..4] != REFTABLE_MAGIC {
        return Err(ReftableError::BadMagic.into());
    }
    let fver = data[4];
    if fver != version {
        return Err(ReftableError::FooterVersionMismatch {
            header: version,
            footer: fver,
        }
        .into());
    }

    // Footer-size validated above, so every fixed-width slice below is in
    // bounds; convert via `?` to surface any unexpected truncation as an error.
    let read_u64 = |slice: &[u8]| -> Result<u64> {
        let bytes: [u8; 8] = slice
            .try_into()
            .map_err(|_| truncated(ReftableField::Footer))?;
        Ok(u64::from_be_bytes(bytes))
    };

    let block_size = ((data[5] as u32) << 16) | ((data[6] as u32) << 8) | (data[7] as u32);
    let min_update_index = read_u64(&data[8..16])?;
    let max_update_index = read_u64(&data[16..24])?;

    // The position fields follow the file header, whose size is version-dependent
    // (24 bytes for v1, 28 for v2 — the extra 4 bytes are the hash-id).
    let off = if version == 2 { 28 } else { 24 };
    let ref_index_position = read_u64(&data[off..off + 8])?;
    let log_position = read_u64(&data[off + 24..off + 32])?;

    // CRC-32 check
    let crc_bytes: [u8; 4] = data[footer_size - 4..footer_size]
        .try_into()
        .map_err(|_| truncated(ReftableField::Footer))?;
    let crc_stored = u32::from_be_bytes(crc_bytes);
    let crc_computed = crc32(&data[..footer_size - 4]);
    if crc_stored != crc_computed {
        return Err(ReftableError::FooterChecksumMismatch {
            stored: crc_stored,
            computed: crc_computed,
        }
        .into());
    }

    Ok(Footer {
        block_size,
        min_update_index,
        max_update_index,
        ref_index_position,
        log_position,
    })
}

/// Parse an identity string like `"Name <email> 1234567890 +0100"`.
pub(crate) fn parse_identity_string(identity: &str) -> (String, String, u64, i16) {
    // Format: "Name <email> timestamp tz"
    let parts: Vec<&str> = identity.rsplitn(3, ' ').collect();
    if parts.len() < 3 {
        return (identity.to_owned(), String::new(), 0, 0);
    }
    let tz_str = parts[0]; // e.g. "+0100"
    let time_str = parts[1]; // e.g. "1234567890"
    let name_email = parts[2]; // e.g. "Name <email>"

    let time_secs = time_str.parse::<u64>().unwrap_or(0);

    // Parse timezone: +HHMM or -HHMM. Reftable log records keep the HHMM digits as a
    // decimal number (`+0530` -> 530), matching what Git writes.
    let tz_hhmm = if tz_str.len() >= 5 {
        let sign = if tz_str.starts_with('-') { -1i16 } else { 1 };
        let hours = tz_str[1..3].parse::<i16>().unwrap_or(0);
        let mins = tz_str[3..5].parse::<i16>().unwrap_or(0);
        sign * (hours * 100 + mins)
    } else {
        0
    };

    // Split name and email
    let (name, email) = if let Some(lt_pos) = name_email.find('<') {
        let name = name_email[..lt_pos].trim().to_owned();
        let email = if let Some(gt_pos) = name_email.find('>') {
            name_email[lt_pos + 1..gt_pos].to_owned()
        } else {
            name_email[lt_pos + 1..].to_owned()
        };
        (name, email)
    } else {
        (name_email.to_owned(), String::new())
    };

    (name, email, time_secs, tz_hhmm)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn reftable_repo_with_config(extra: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = dir.path().join(".git");
        let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read config");
        cfg.push_str(extra);
        fs::write(git_dir.join("config"), cfg).expect("write config");
        (dir, git_dir)
    }

    #[test]
    fn write_options_log_blocks_follow_log_all_ref_updates() {
        for (value, write_log) in [
            ("true", true),
            ("always", true),
            ("yes", true),
            ("on", true),
            ("1", true),
            ("false", false),
            ("no", false),
            ("0", false),
            ("never", false),
        ] {
            let (_dir, git_dir) =
                reftable_repo_with_config(&format!("[core]\n\tlogAllRefUpdates = {value}\n"));
            assert_eq!(
                read_write_options(&git_dir).write_log,
                write_log,
                "core.logAllRefUpdates = {value}"
            );
        }
        let (_dir, git_dir) = reftable_repo_with_config("");
        assert!(read_write_options(&git_dir).write_log);
    }

    #[test]
    fn write_options_broken_config_keeps_entries_before_bad_line() {
        let (_dir, git_dir) = reftable_repo_with_config(
            "[reftable]\n\tgeometricFactor = 5\n\tindexObjects = off\n[core]\n\tlogAllRefUpdates = no\n[broken\nblockSize = 1\n",
        );
        let opts = read_write_options(&git_dir);
        assert_eq!(opts.auto_compaction_factor, 5);
        assert!(opts.skip_index_objects);
        assert!(!opts.write_log);
        assert_eq!(opts.block_size, WriteOptions::default().block_size);
    }

    #[test]
    fn should_write_log_follows_mode_and_existing_reflog() {
        let (_dir, git_dir) = reftable_repo_with_config("[core]\n\tlogAllRefUpdates = true\n");
        assert!(reftable_should_write_log(&git_dir, "refs/heads/main"));
        assert!(!reftable_should_write_log(&git_dir, "refs/tags/v1"));

        let (_dir, git_dir) = reftable_repo_with_config("[core]\n\tlogAllRefUpdates = always\n");
        assert!(reftable_should_write_log(&git_dir, "refs/tags/v1"));

        let (_dir, git_dir) = reftable_repo_with_config("[core]\n\tlogAllRefUpdates = false\n");
        assert!(!reftable_should_write_log(&git_dir, "refs/heads/topic"));
        reftable_create_reflog(&git_dir, "refs/heads/topic").expect("create reflog");
        assert!(reftable_should_write_log(&git_dir, "refs/heads/topic"));
    }

    #[test]
    fn read_refs_on_empty_reftable_without_tables_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(git_dir.join("reftable")).expect("reftable dir");
        let stack = ReftableStack::open(&git_dir).expect("open empty stack");
        assert!(stack.read_refs().expect("read").is_empty());
    }

    #[test]
    fn read_refs_tracks_external_compaction_on_public_stack() {
        use crate::objects::ObjectId;
        use crate::repo::init_repository;

        let dir = tempfile::tempdir().expect("tempdir");
        init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = dir.path().join(".git");
        let mut opts = read_write_options(&git_dir);
        opts.autocompaction_enabled = false;
        let oid_a = ObjectId::from_bytes(&[0x31; 20]).expect("oid");
        let oid_b = ObjectId::from_bytes(&[0x32; 20]).expect("oid");
        let identity = "Compact Ext <ext@example.com> 1700000000 +0000";
        reftable_write_ref_with_write_options(
            &git_dir,
            "refs/heads/keep-a",
            &oid_a,
            Some(identity),
            None,
            &opts,
        )
        .expect("write a");
        reftable_write_ref_with_write_options(
            &git_dir,
            "refs/heads/keep-b",
            &oid_b,
            Some(identity),
            None,
            &opts,
        )
        .expect("write b");

        let reader = ReftableStack::open(&git_dir).expect("reader stack");
        let mut writer = ReftableStack::open(&git_dir).expect("writer stack");
        writer.compact().expect("compact via second handle");
        let refs = reader.read_refs().expect("read after external compact");
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.iter().any(|n| *n == "refs/heads/keep-a"));
        assert!(names.iter().any(|n| *n == "refs/heads/keep-b"));
    }

    #[test]
    fn subset_stack_read_refs_respects_table_names() {
        use crate::objects::ObjectId;
        use crate::repo::init_repository;

        let dir = tempfile::tempdir().expect("tempdir");
        init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = dir.path().join(".git");
        let mut opts = read_write_options(&git_dir);
        opts.autocompaction_enabled = false;
        let oid_a = ObjectId::from_bytes(&[0x11; 20]).expect("oid");
        let oid_b = ObjectId::from_bytes(&[0x22; 20]).expect("oid");
        let identity = "Subset Stack <subset@example.com> 1700000000 +0000";
        reftable_write_ref_with_write_options(
            &git_dir,
            "refs/heads/only-in-first-table",
            &oid_a,
            Some(identity),
            None,
            &opts,
        )
        .expect("first ref");
        let first_table = ReftableStack::open(&git_dir)
            .expect("stack after first ref")
            .table_names
            .last()
            .expect("first ref table")
            .clone();
        reftable_write_ref_with_write_options(
            &git_dir,
            "refs/heads/only-in-second-table",
            &oid_b,
            Some(identity),
            None,
            &opts,
        )
        .expect("second ref");

        let stack = ReftableStack::open(&git_dir).expect("open");
        assert!(
            stack.table_names.len() >= 2,
            "expected multiple tables without autocompaction, got {}",
            stack.table_names.len()
        );

        let subset = ReftableStack::subset_stack(stack.reftable_dir.clone(), vec![first_table]);
        let refs = subset.read_refs().expect("subset read");
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.iter().any(|n| *n == "refs/heads/only-in-first-table"));
        assert!(!names
            .iter()
            .any(|n| *n == "refs/heads/only-in-second-table"));

        let full = stack.read_refs().expect("full read");
        assert_eq!(full.len(), 3);
        assert!(full.iter().any(|record| record.name == "HEAD"
            && record.value == RefValue::Symref("refs/heads/main".into())));
    }

    #[test]
    fn suggest_compaction_segment_matches_geometric_break() {
        assert_eq!(suggest_compaction_segment(&[10]), (0, 0));
        assert_eq!(suggest_compaction_segment(&[64, 32, 16]), (0, 0));
        let (start, end) = suggest_compaction_segment(&[64, 32, 16, 8, 4, 3, 1]);
        assert!(
            end > start + 1,
            "expected compaction segment, got {start}..{end}"
        );
    }

    #[test]
    fn prefix_upper_exclusive_for_seek_end() {
        assert_eq!(
            prefix_upper_exclusive("refs/heads/b05").as_deref(),
            Some("refs/heads/b06")
        );
        assert!(prefix_upper_exclusive("refs/heads/").is_none());
    }

    #[test]
    fn test_varint_roundtrip() {
        for val in [0u64, 1, 127, 128, 255, 256, 16383, 16384, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(val, &mut buf);
            let (decoded, end) = get_varint(&buf, 0).unwrap();
            assert_eq!(decoded, val, "varint roundtrip failed for {val}");
            assert_eq!(end, buf.len());
        }
    }

    #[test]
    fn test_crc32() {
        // Known test vector: "123456789" => 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn test_empty_table() {
        let writer = ReftableWriter::new(WriteOptions::default(), 1, 1);
        let data = writer.finish().unwrap();
        let reader = ReftableReader::new(data).unwrap();
        let refs = reader.read_refs().unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn test_write_read_single_ref() {
        let oid = ObjectId::from_bytes(&[0xab; 20]).unwrap();
        let mut writer = ReftableWriter::new(WriteOptions::default(), 1, 1);
        writer
            .add_ref(RefRecord {
                name: "refs/heads/main".to_owned(),
                update_index: 1,
                value: RefValue::Val1(oid),
            })
            .unwrap();
        let data = writer.finish().unwrap();

        let reader = ReftableReader::new(data).unwrap();
        let refs = reader.read_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "refs/heads/main");
        assert_eq!(refs[0].value, RefValue::Val1(oid));
        assert_eq!(refs[0].update_index, 1);
    }

    #[test]
    fn test_write_read_multiple_refs() {
        let oid1 = ObjectId::from_bytes(&[0x11; 20]).unwrap();
        let oid2 = ObjectId::from_bytes(&[0x22; 20]).unwrap();
        let oid3 = ObjectId::from_bytes(&[0x33; 20]).unwrap();

        let mut writer = ReftableWriter::new(WriteOptions::default(), 1, 1);
        writer
            .add_ref(RefRecord {
                name: "refs/heads/a".to_owned(),
                update_index: 1,
                value: RefValue::Val1(oid1),
            })
            .unwrap();
        writer
            .add_ref(RefRecord {
                name: "refs/heads/b".to_owned(),
                update_index: 1,
                value: RefValue::Val1(oid2),
            })
            .unwrap();
        writer
            .add_ref(RefRecord {
                name: "refs/tags/v1.0".to_owned(),
                update_index: 1,
                value: RefValue::Val2(oid3, oid1),
            })
            .unwrap();
        let data = writer.finish().unwrap();

        let reader = ReftableReader::new(data).unwrap();
        let refs = reader.read_refs().unwrap();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "refs/heads/a");
        assert_eq!(refs[1].name, "refs/heads/b");
        assert_eq!(refs[2].name, "refs/tags/v1.0");
        assert_eq!(refs[2].value, RefValue::Val2(oid3, oid1));
    }

    #[test]
    fn test_symref_roundtrip() {
        let mut writer = ReftableWriter::new(WriteOptions::default(), 1, 1);
        writer
            .add_ref(RefRecord {
                name: "refs/heads/sym".to_owned(),
                update_index: 1,
                value: RefValue::Symref("refs/heads/main".to_owned()),
            })
            .unwrap();
        let data = writer.finish().unwrap();

        let reader = ReftableReader::new(data).unwrap();
        let refs = reader.read_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].value,
            RefValue::Symref("refs/heads/main".to_owned())
        );
    }

    #[test]
    fn test_log_roundtrip() {
        let old_oid = ObjectId::from_bytes(&[0; 20]).unwrap();
        let new_oid = ObjectId::from_bytes(&[0xaa; 20]).unwrap();

        let opts = WriteOptions {
            write_log: true,
            ..Default::default()
        };
        let mut writer = ReftableWriter::new(opts, 1, 1);
        writer
            .add_log(LogRecord {
                refname: "refs/heads/main".to_owned(),
                update_index: 1,
                old_id: old_oid,
                new_id: new_oid,
                name: "Test User".to_owned(),
                email: "test@example.com".to_owned(),
                time_seconds: 1700000000,
                tz_offset: -800,
                message: "initial commit".to_owned(),
            })
            .unwrap();
        let data = writer.finish().unwrap();

        let reader = ReftableReader::new(data).unwrap();
        let logs = reader.read_logs().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].refname, "refs/heads/main");
        assert_eq!(logs[0].old_id, old_oid);
        assert_eq!(logs[0].new_id, new_oid);
        assert_eq!(logs[0].name, "Test User");
        assert_eq!(logs[0].email, "test@example.com");
        assert_eq!(logs[0].time_seconds, 1700000000);
        assert_eq!(logs[0].tz_offset, -800);
        // The reftable writer cleans messages the way git does: it appends a
        // trailing newline. `read_logs` returns the raw on-disk message (the
        // newline is only stripped when converting to a `ReflogEntry`).
        assert_eq!(logs[0].message, "initial commit\n");
    }

    #[test]
    fn test_unaligned_table() {
        let oid = ObjectId::from_bytes(&[0xcc; 20]).unwrap();
        let opts = WriteOptions {
            // Unpadded (unaligned) blocks: like git's `unpadded` write option,
            // blocks are not padded out to the block size. A block_size of 0 is
            // resolved to the default at write time, so the reported block size
            // is the default rather than 0.
            unpadded: true,
            restart_interval: 16,
            write_log: false,
            ..WriteOptions::default()
        };
        let mut writer = ReftableWriter::new(opts, 1, 1);
        writer
            .add_ref(RefRecord {
                name: "refs/heads/main".to_owned(),
                update_index: 1,
                value: RefValue::Val1(oid),
            })
            .unwrap();
        let data = writer.finish().unwrap();

        // An unpadded single-ref table is far smaller than one padded block.
        assert!(data.len() < DEFAULT_BLOCK_SIZE as usize);

        let reader = ReftableReader::new(data).unwrap();
        let refs = reader.read_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].value, RefValue::Val1(oid));
    }

    #[test]
    fn test_parse_identity() {
        let (name, email, ts, tz) =
            parse_identity_string("Test User <test@example.com> 1700000000 -0800");
        assert_eq!(name, "Test User");
        assert_eq!(email, "test@example.com");
        assert_eq!(ts, 1700000000);
        // Git stores the HHMM digits as a decimal number, not minutes.
        assert_eq!(tz, -800);
        let (_, _, _, tz) = parse_identity_string("Test User <test@example.com> 1700000000 +0530");
        assert_eq!(tz, 530);
    }

    #[test]
    fn test_deletion_record() {
        let mut writer = ReftableWriter::new(WriteOptions::default(), 1, 1);
        writer
            .add_ref(RefRecord {
                name: "refs/heads/gone".to_owned(),
                update_index: 1,
                value: RefValue::Deletion,
            })
            .unwrap();
        let data = writer.finish().unwrap();

        let reader = ReftableReader::new(data).unwrap();
        let refs = reader.read_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].value, RefValue::Deletion);
    }

    #[test]
    fn verify_batch_reftable_symref_ancestor_blocks_child_create() {
        use crate::objects::ObjectId;
        use crate::refs::{verify_ref_transaction_batch, RefBatchItem, RefnameUnavailable};
        use crate::repo::init_repository;

        let dir = tempfile::tempdir().expect("tempdir");
        init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = dir.path().join(".git");
        reftable_write_symref(
            &git_dir,
            "refs/heads/parent",
            "refs/heads/missing-target",
            None,
            None,
        )
        .expect("symref parent");

        let child_oid = ObjectId::from_bytes(&[0x44; 20]).expect("oid");
        let items = vec![RefBatchItem {
            name: "refs/heads/parent/child".to_owned(),
            new_oid: Some(child_oid),
        }];
        let err = verify_ref_transaction_batch(&git_dir, &items).unwrap_err();
        assert!(
            matches!(
                err,
                RefnameUnavailable::AncestorExists {
                    ref blocking,
                    ref new_ref,
                } if blocking == "refs/heads/parent" && new_ref == "refs/heads/parent/child"
            ),
            "unexpected {err:?}"
        );
    }

    #[test]
    fn update_refs_reftable_symref_ancestor_blocks_child_create() {
        use crate::error::Error;
        use crate::objects::ObjectId;
        use crate::refs::gc::{update_refs, RefTransactionItem};
        use crate::refs::store::RefStoreError;
        use crate::refs::RefnameUnavailable;
        use crate::repo::init_repository;

        let dir = tempfile::tempdir().expect("tempdir");
        init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = dir.path().join(".git");
        reftable_write_symref(
            &git_dir,
            "refs/heads/parent",
            "refs/heads/missing-target",
            None,
            None,
        )
        .expect("symref parent");

        let child_oid = ObjectId::from_bytes(&[0x55; 20]).expect("oid");
        let err = update_refs(
            &git_dir,
            &[RefTransactionItem {
                name: "refs/heads/parent/child".to_owned(),
                new_oid: Some(child_oid),
                expected_old: None,
            }],
        )
        .unwrap_err();
        match err {
            Error::RefStore(RefStoreError::NameUnavailable {
                reason: RefnameUnavailable::AncestorExists { blocking, new_ref },
            }) => {
                assert_eq!(blocking, "refs/heads/parent");
                assert_eq!(new_ref, "refs/heads/parent/child");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn write_transaction_inject_failure_leaves_tables_list_unchanged() {
        use crate::objects::ObjectId;
        use crate::repo::init_repository;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .unwrap();
        let git_dir = repo.git_dir();
        let tables_list = git_dir.join("reftable/tables.list");
        let before = fs::read_to_string(&tables_list).unwrap_or_default();

        let oid = ObjectId::from_bytes(&[0xcd; 20]).unwrap();
        set_test_inject_reftable_transaction_fail(true);
        let mut stack = ReftableStack::open(&git_dir).unwrap();
        let opts = read_write_options(&git_dir);
        let err = stack.write_transaction(
            vec![ReftableTransactionUpdate {
                refname: "refs/heads/inject-fail".to_owned(),
                value: Some(RefValue::Val1(oid)),
                log: None,
                expected_old: None,
            }],
            &opts,
        );
        set_test_inject_reftable_transaction_fail(false);
        assert!(err.is_err());

        let after = fs::read_to_string(&tables_list).unwrap_or_default();
        assert_eq!(after, before);
    }

    #[test]
    fn truncated_varint_is_typed() {
        let err = get_varint(&[0x80], 0).expect_err("continuation byte without a successor");
        assert!(matches!(
            err,
            Error::Reftable(ReftableError::Truncated {
                field: ReftableField::Varint
            })
        ));
        assert!(matches!(
            get_varint(&[], 0),
            Err(Error::Reftable(ReftableError::Truncated {
                field: ReftableField::Varint
            }))
        ));
    }

    #[test]
    fn writer_rejects_unsorted_refs_with_both_names() {
        let oid = ObjectId::from_bytes(&[0x11; 20]).expect("oid");
        let mut writer = ReftableWriter::new(WriteOptions::default(), 1, 1);
        writer
            .add_ref(RefRecord {
                name: "refs/heads/b".to_owned(),
                update_index: 1,
                value: RefValue::Val1(oid),
            })
            .expect("first ref");
        let err = writer
            .add_ref(RefRecord {
                name: "refs/heads/a".to_owned(),
                update_index: 1,
                value: RefValue::Val1(oid),
            })
            .expect_err("out of order");
        match err {
            Error::Reftable(ReftableError::UnsortedRefs { previous, next }) => {
                assert_eq!(previous, "refs/heads/b");
                assert_eq!(next, "refs/heads/a");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn reader_reports_footer_version_mismatch_and_short_files() {
        let data = ReftableWriter::new(WriteOptions::default(), 1, 1)
            .finish()
            .expect("empty table");
        let footer_start = data.len() - FOOTER_V1_SIZE;

        let mut mismatched = data.clone();
        mismatched[footer_start + 4] = 2;
        assert!(matches!(
            ReftableReader::new(mismatched),
            Err(Error::Reftable(ReftableError::FooterVersionMismatch {
                header: 1,
                footer: 2
            }))
        ));

        assert!(matches!(
            ReftableReader::new(data[..10].to_vec()),
            Err(Error::Reftable(ReftableError::TooSmall { len: 10 }))
        ));
        assert!(matches!(
            ReftableReader::new(data[..HEADER_SIZE + 4].to_vec()),
            Err(Error::Reftable(ReftableError::TooSmall { .. }))
        ));
    }

    #[test]
    fn write_transaction_reports_expected_mismatch() {
        use crate::repo::init_repository;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = repo.git_dir();
        let opts = read_write_options(&git_dir);
        let current = ObjectId::from_bytes(&[0x21; 20]).expect("oid");
        let stale = ObjectId::from_bytes(&[0x22; 20]).expect("oid");
        let mut stack = ReftableStack::open(&git_dir).expect("open");
        stack
            .write_transaction(
                vec![ReftableTransactionUpdate {
                    refname: "refs/heads/topic".to_owned(),
                    value: Some(RefValue::Val1(current)),
                    log: None,
                    expected_old: None,
                }],
                &opts,
            )
            .expect("seed topic");

        let update = |refname: &str| ReftableTransactionUpdate {
            refname: refname.to_owned(),
            value: Some(RefValue::Val1(stale)),
            log: None,
            expected_old: Some(stale),
        };
        match stack.write_transaction(vec![update("refs/heads/topic")], &opts) {
            Err(Error::Reftable(ReftableError::ExpectedMismatch {
                refname,
                expected,
                actual,
            })) => {
                assert_eq!(refname, "refs/heads/topic");
                assert_eq!(expected, stale);
                assert_eq!(actual, Some(current));
            }
            other => panic!("unexpected {other:?}"),
        }
        let err = stack
            .write_transaction(vec![update("refs/heads/absent")], &opts)
            .expect_err("absent ref");
        assert!(matches!(
            &err,
            Error::Reftable(ReftableError::ExpectedMismatch { actual: None, .. })
        ));
        assert!(err.to_string().ends_with("but found <absent>"), "{err}");
    }

    #[test]
    fn error_display_names_the_field() {
        assert_eq!(
            ReftableError::Truncated {
                field: ReftableField::LogEmail
            }
            .to_string(),
            "reftable: truncated log email"
        );
        assert_eq!(
            Error::from(ReftableError::InvalidUtf8 {
                field: ReftableField::SymrefTarget
            })
            .to_string(),
            "reftable: symref target is not valid UTF-8"
        );
    }
}
