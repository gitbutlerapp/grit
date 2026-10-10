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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::objects::ObjectId;

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
        return Err(Error::InvalidRef("varint: unexpected end of data".into()));
    }
    let mut val = (data[pos] & 0x7f) as u64;
    while data[pos] & 0x80 != 0 {
        pos += 1;
        if pos >= data.len() {
            return Err(Error::InvalidRef("varint: unexpected end of data".into()));
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
    /// Timezone offset in minutes (signed).
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
                return Err(Error::InvalidRef(format!(
                    "reftable: refs must be sorted, got '{}' after '{}'",
                    rec.name, last.name
                )));
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
            return Err(Error::InvalidRef("reftable: empty record key".into()));
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
        // Attempt to add.
        let opts = self.opts.clone();
        let fit = {
            let bw = self
                .block
                .as_mut()
                .ok_or_else(|| Error::InvalidRef("reftable: no active block writer".into()))?;
            bw.add(rec, &opts)?
        };
        if fit {
            return Ok(());
        }
        // Block full: flush and retry in a fresh block.
        self.flush_block()?;
        self.reinit_block(typ);
        let opts = self.opts.clone();
        let bw = self
            .block
            .as_mut()
            .ok_or_else(|| Error::InvalidRef("reftable: no active block writer".into()))?;
        if !bw.add(rec, &opts)? {
            return Err(Error::InvalidRef(
                "reftable: transaction failure: entry too large".into(),
            ));
        }
        Ok(())
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
        let opts = self.opts.clone();
        let rec = EncRecord::Obj {
            prefix: prefix.clone(),
            offsets: offsets.to_vec(),
        };
        let fit = {
            let bw = self
                .block
                .as_mut()
                .ok_or_else(|| Error::InvalidRef("reftable: no active block writer".into()))?;
            bw.add(&rec, &opts)?
        };
        if fit {
            return Ok(());
        }
        self.flush_block()?;
        self.reinit_block(typ);
        let opts = self.opts.clone();
        let fit = {
            let bw = self
                .block
                .as_mut()
                .ok_or_else(|| Error::InvalidRef("reftable: no active block writer".into()))?;
            bw.add(&rec, &opts)?
        };
        if fit {
            return Ok(());
        }
        // Drop offsets entirely.
        let rec = EncRecord::Obj {
            prefix,
            offsets: Vec::new(),
        };
        let opts = self.opts.clone();
        let bw = self
            .block
            .as_mut()
            .ok_or_else(|| Error::InvalidRef("reftable: no active block writer".into()))?;
        bw.add(&rec, &opts)?;
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
    data: Vec<u8>,
    version: u8,
    block_size: u32,
    min_update_index: u64,
    max_update_index: u64,
    ref_index_position: u64,
    log_position: u64,
}

/// Parsed footer fields.
#[derive(Debug)]
#[allow(dead_code)]
struct Footer {
    version: u8,
    block_size: u32,
    min_update_index: u64,
    max_update_index: u64,
    ref_index_position: u64,
    obj_position_and_id_len: u64,
    obj_index_position: u64,
    log_position: u64,
    log_index_position: u64,
}

impl ReftableReader {
    /// Open a reftable from bytes.
    pub fn new(data: Vec<u8>) -> Result<Self> {
        if data.len() < HEADER_SIZE + FOOTER_V1_SIZE {
            // Could be an empty table (header + footer only = 24 + 68 = 92)
            if data.len() < HEADER_SIZE {
                return Err(Error::InvalidRef("reftable: file too small".into()));
            }
        }

        // Parse header
        if &data[0..4] != REFTABLE_MAGIC {
            return Err(Error::InvalidRef("reftable: bad magic".into()));
        }
        let version = data[4];
        if version != 1 && version != 2 {
            return Err(Error::InvalidRef(format!(
                "reftable: unsupported version {version}"
            )));
        }
        let _block_size = ((data[5] as u32) << 16) | ((data[6] as u32) << 8) | (data[7] as u32);
        let _min_update_index = u64::from_be_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| Error::InvalidRef("reftable: truncated header".into()))?,
        );
        let _max_update_index = u64::from_be_bytes(
            data[16..24]
                .try_into()
                .map_err(|_| Error::InvalidRef("reftable: truncated header".into()))?,
        );

        // Parse footer
        let footer_size = if version == 2 { 72 } else { FOOTER_V1_SIZE };
        if data.len() < footer_size {
            return Err(Error::InvalidRef(
                "reftable: file too small for footer".into(),
            ));
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
        let footer_size = if self.version == 2 {
            72
        } else {
            FOOTER_V1_SIZE
        };
        let file_end = self.data.len() - footer_size;

        // Determine where ref blocks end
        let ref_end = if self.ref_index_position > 0 {
            self.ref_index_position as usize
        } else if self.log_position > 0 {
            self.log_position as usize
        } else {
            file_end
        };

        let mut pos = 0usize;
        // Skip the file header — the first ref block shares the header's physical
        // block, starting at the header size (24 for v1, 28 for v2).
        let header_len = self.header_len();
        if pos < header_len {
            pos = header_len;
        }

        while pos < ref_end {
            if pos >= self.data.len() {
                break;
            }
            let block_type = self.data[pos];
            if block_type == 0 {
                // Padding — skip to next block boundary
                if self.block_size > 0 {
                    let bs = self.block_size as usize;
                    pos = ((pos / bs) + 1) * bs;
                    continue;
                } else {
                    break;
                }
            }
            if block_type != BLOCK_TYPE_REF {
                break;
            }

            let block_len = read_u24(&self.data, pos + 1);
            // Determine the data range for this block
            let block_data_start = pos + 4; // after type(1) + len(3)

            // The first block's block_len includes the file header.
            let is_first = pos == header_len;
            let records_end = if is_first {
                // block_len is from file start
                block_len
            } else {
                pos + block_len
            };

            if records_end > ref_end {
                break;
            }

            let restart_table_start =
                restart_table_start_for_block(&self.data, records_end, block_data_start)?;

            // Read records from block_data_start to restart_table_start
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
                refs.push(rec);
                rpos = new_pos;
            }

            // Advance to next block
            if self.block_size > 0 {
                let bs = self.block_size as usize;
                if is_first {
                    pos = bs;
                } else {
                    pos += bs;
                }
            } else {
                pos = records_end;
            }
        }

        Ok(refs)
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
            return Err(Error::InvalidRef("reftable: truncated block header".into()));
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
            return Err(Error::InvalidRef("reftable: invalid block length".into()));
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
            return Err(Error::InvalidRef(
                "reftable: index prefix_len exceeds prev key".into(),
            ));
        }
        key.extend_from_slice(&prev_key[..prefix_len as usize]);
    }
    if p + suffix_len > data.len() {
        return Err(Error::InvalidRef("reftable: index suffix overflows".into()));
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
            return Err(Error::InvalidRef(
                "reftable: prefix_len exceeds prev name".into(),
            ));
        }
        name.extend_from_slice(&prev_name[..prefix_len as usize]);
    }
    if p + suffix_len > data.len() {
        return Err(Error::InvalidRef("reftable: suffix overflows block".into()));
    }
    name.extend_from_slice(&data[p..p + suffix_len]);
    p += suffix_len;

    let name_str = String::from_utf8(name)
        .map_err(|_| Error::InvalidRef("reftable: invalid UTF-8 in ref name".into()))?;

    let (update_index_delta, mut p) = get_varint(data, p)?;
    let update_index = min_update_index + update_index_delta;

    let value = match value_type {
        VALUE_DELETION => RefValue::Deletion,
        VALUE_ONE_OID => {
            if p + hash_size > data.len() {
                return Err(Error::InvalidRef("reftable: truncated OID".into()));
            }
            let oid = ObjectId::from_bytes(&data[p..p + hash_size])?;
            p += hash_size;
            RefValue::Val1(oid)
        }
        VALUE_TWO_OID => {
            if p + 2 * hash_size > data.len() {
                return Err(Error::InvalidRef("reftable: truncated OID pair".into()));
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
                return Err(Error::InvalidRef(
                    "reftable: truncated symref target".into(),
                ));
            }
            let target = String::from_utf8(data[p..p + target_len].to_vec())
                .map_err(|_| Error::InvalidRef("reftable: invalid UTF-8 in symref".into()))?;
            p += target_len;
            RefValue::Symref(target)
        }
        _ => {
            return Err(Error::InvalidRef(format!(
                "reftable: unknown value_type {value_type}"
            )));
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
            return Err(Error::InvalidRef(
                "reftable: log prefix_len exceeds prev key".into(),
            ));
        }
        key.extend_from_slice(&prev_key[..prefix_len as usize]);
    }
    if p + suffix_len > data.len() {
        return Err(Error::InvalidRef("reftable: log suffix overflows".into()));
    }
    key.extend_from_slice(&data[p..p + suffix_len]);
    p += suffix_len;

    // Parse key: refname \0 reverse_int64(update_index)
    let null_pos = key
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::InvalidRef("reftable: log key missing null separator".into()))?;
    let refname = String::from_utf8(key[..null_pos].to_vec())
        .map_err(|_| Error::InvalidRef("reftable: invalid UTF-8 in log refname".into()))?;
    if null_pos + 9 > key.len() {
        return Err(Error::InvalidRef("reftable: log key too short".into()));
    }
    let reversed_idx = u64::from_be_bytes(
        key[null_pos + 1..null_pos + 9]
            .try_into()
            .map_err(|_| Error::InvalidRef("reftable: log key too short".into()))?,
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
        return Err(Error::InvalidRef("reftable: truncated log OIDs".into()));
    }
    let old_id = ObjectId::from_bytes(&data[p..p + hash_size])?;
    p += hash_size;
    let new_id = ObjectId::from_bytes(&data[p..p + hash_size])?;
    p += hash_size;

    let (name_len, p2) = get_varint(data, p)?;
    p = p2;
    let name_len = name_len as usize;
    if p + name_len > data.len() {
        return Err(Error::InvalidRef("reftable: truncated log name".into()));
    }
    let name = String::from_utf8(data[p..p + name_len].to_vec())
        .map_err(|_| Error::InvalidRef("reftable: invalid UTF-8 in log name".into()))?;
    p += name_len;

    let (email_len, p2) = get_varint(data, p)?;
    p = p2;
    let email_len = email_len as usize;
    if p + email_len > data.len() {
        return Err(Error::InvalidRef("reftable: truncated log email".into()));
    }
    let email = String::from_utf8(data[p..p + email_len].to_vec())
        .map_err(|_| Error::InvalidRef("reftable: invalid UTF-8 in log email".into()))?;
    p += email_len;

    let (time_seconds, p2) = get_varint(data, p)?;
    p = p2;

    if p + 2 > data.len() {
        return Err(Error::InvalidRef("reftable: truncated tz_offset".into()));
    }
    let tz_offset = i16::from_be_bytes([data[p], data[p + 1]]);
    p += 2;

    let (msg_len, p2) = get_varint(data, p)?;
    p = p2;
    let msg_len = msg_len as usize;
    if p + msg_len > data.len() {
        return Err(Error::InvalidRef("reftable: truncated log message".into()));
    }
    let message = String::from_utf8(data[p..p + msg_len].to_vec())
        .map_err(|_| Error::InvalidRef("reftable: invalid UTF-8 in log message".into()))?;
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
fn widen_oid_to(oid: ObjectId, hash_size: usize) -> ObjectId {
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

/// Manages the `$GIT_DIR/reftable/` directory and `tables.list` stack.
///
/// The stack provides a merged view of all tables, with later tables
/// taking precedence over earlier ones.
pub struct ReftableStack {
    /// Path to the `reftable/` directory.
    reftable_dir: PathBuf,
    /// Ordered list of table file names (oldest first).
    table_names: Vec<String>,
}

/// RAII guard for `tables.list.lock`. Removes the lock file on drop unless it was
/// consumed (renamed onto `tables.list`) via [`disarm`].
struct TablesListLock {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl TablesListLock {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            armed: std::cell::Cell::new(true),
        }
    }

    /// Mark the lock as consumed so its `Drop` does not remove the path (it has
    /// been renamed onto `tables.list`).
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for TablesListLock {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_file(&self.path);
        }
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
    pub fn open(git_dir: &Path) -> Result<Self> {
        let reftable_dir = git_dir.join("reftable");
        let tables_list = reftable_dir.join("tables.list");
        let content = fs::read_to_string(&tables_list).map_err(Error::Io)?;
        let table_names: Vec<String> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_owned())
            .collect();
        Ok(Self {
            reftable_dir,
            table_names,
        })
    }

    /// Inject the HEAD symbolic ref into the ref set being compacted, mirroring
    /// git's reftable layout where HEAD lives inside the table.
    ///
    /// Returns a HEAD reflog record to add to the log section if the target
    /// branch has a most-recent reflog entry (so HEAD@{0} mirrors it).
    fn inject_head_ref(&self, refs: &mut Vec<RefRecord>, min_idx: u64) -> Option<LogRecord> {
        let git_dir = self.reftable_dir.parent()?;
        let head_path = git_dir.join("HEAD");
        let content = fs::read_to_string(&head_path).ok()?;
        let target = content.strip_prefix("ref: ")?.trim();
        if target.is_empty() || target == "refs/heads/.invalid" {
            return None;
        }
        // Only inject HEAD if it is not already present.
        if refs.iter().any(|r| r.name == "HEAD") {
            return None;
        }
        // HEAD takes the smallest update index (git assigns it the first one).
        refs.push(RefRecord {
            name: "HEAD".to_owned(),
            update_index: min_idx,
            value: RefValue::Symref(target.to_owned()),
        });
        refs.sort_by(|a, b| a.name.cmp(&b.name));

        // HEAD reflog entries are already written separately by the commit /
        // update-ref paths (`append_reflog("HEAD", …)`). Only synthesize a
        // mirror of the branch's newest entry when HEAD has no reflog of its
        // own — otherwise compaction would duplicate HEAD@{0} (yielding an
        // extra log record and an oversized log block, t0613 'default write
        // options').
        if self
            .read_logs_for_ref("HEAD")
            .map(|logs| !logs.is_empty())
            .unwrap_or(false)
        {
            return None;
        }

        // Mirror the target branch's newest reflog entry as HEAD@{0}.
        let target_logs = self.read_logs_for_ref(target).ok()?;
        let newest = target_logs.into_iter().next()?;
        Some(LogRecord {
            refname: "HEAD".to_owned(),
            update_index: newest.update_index,
            old_id: newest.old_id,
            new_id: newest.new_id,
            name: newest.name,
            email: newest.email,
            time_seconds: newest.time_seconds,
            tz_offset: newest.tz_offset,
            message: newest.message,
        })
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

    /// Read a merged view of all ref records.
    ///
    /// Later tables override earlier ones. Deletion records cause the
    /// ref to be omitted from the result.
    pub fn read_refs(&self) -> Result<Vec<RefRecord>> {
        let mut merged: BTreeMap<String, RefRecord> = BTreeMap::new();

        for name in &self.table_names {
            let path = self.reftable_dir.join(name);
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::Io(err)),
            };
            let reader = ReftableReader::new(data)?;
            for rec in reader.read_refs()? {
                match &rec.value {
                    RefValue::Deletion => {
                        merged.remove(&rec.name);
                    }
                    _ => {
                        merged.insert(rec.name.clone(), rec);
                    }
                }
            }
        }

        Ok(merged.into_values().collect())
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
        // Search tables in reverse (newest first)
        for table_name in self.table_names.iter().rev() {
            let path = self.reftable_dir.join(table_name);
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::Io(err)),
            };
            let reader = ReftableReader::new(data)?;
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
            let guard = self.acquire_tables_list_lock()?;
            self.reload_table_names();
            self.table_names.push(filename.clone());
            self.write_tables_list_locked(&guard)?;
        }

        // Auto-compact small write bursts into a single table. A plain commit writes several small
        // ref/log updates and should settle back to one table; a following tag write remains as a
        // second table until explicit `pack-refs`.
        if table_has_deletion && self.table_names.len() > 2 {
            self.compact_prefix_preserving_newest(opts)?;
        } else if self.table_names.len() > 3 && opts.autocompaction_enabled {
            if self
                .table_names
                .iter()
                .any(|name| self.table_is_locked(name))
            {
                self.compact_unlocked_suffix()?;
            } else {
                self.compact()?;
            }
        }

        Ok(filename)
    }

    fn compact_prefix_preserving_newest(&mut self, opts: &WriteOptions) -> Result<()> {
        if !opts.autocompaction_enabled {
            return Ok(());
        }
        let guard = self.acquire_tables_list_lock()?;
        self.reload_table_names();
        if self.table_names.len() <= 2 {
            return Ok(());
        }
        let newest =
            self.table_names.last().cloned().ok_or_else(|| {
                Error::InvalidRef("reftable: table stack unexpectedly empty".into())
            })?;
        let old_names: Vec<String> = self.table_names[..self.table_names.len() - 1].to_vec();
        let prefix_stack = Self {
            reftable_dir: self.reftable_dir.clone(),
            table_names: old_names.clone(),
        };
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
        self.write_tables_list_locked(&guard)?;
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
        let guard = self.acquire_tables_list_lock()?;
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
        let suffix_stack = Self {
            reftable_dir: self.reftable_dir.clone(),
            table_names: old_suffix.clone(),
        };
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
        self.write_tables_list_locked(&guard)?;
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
            let guard = self.acquire_tables_list_lock()?;
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
            self.write_tables_list_locked(&guard)?;
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

        test_inject_reftable_transaction_fail()?;

        {
            let guard = self.acquire_tables_list_lock()?;
            self.reload_table_names();
            for update in &updates {
                if let Some(expected) = update.expected_old {
                    let current = self.lookup_ref_oid(&update.refname)?;
                    if current != Some(expected) {
                        return Err(Error::Message(format!(
                            "ref transaction rejected: '{}' expected {} but found {}",
                            update.refname,
                            expected.to_hex(),
                            current
                                .map(|o| o.to_hex())
                                .unwrap_or_else(|| "<absent>".to_owned()),
                        )));
                    }
                }
            }
            let update_index = self.max_update_index_unlocked()? + 1;
            let mut writer = ReftableWriter::new(opts.clone(), update_index, update_index);

            let mut updates = updates;
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
            self.write_tables_list_locked(&guard)?;
        }

        self.maybe_auto_compact(opts)?;
        Ok(())
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
    fn maybe_auto_compact(&mut self, opts: &WriteOptions) -> Result<()> {
        self.reload_table_names();
        let has_locked = self
            .table_names
            .iter()
            .any(|name| self.table_is_locked(name));
        if self.table_names.len() > 3 && opts.autocompaction_enabled {
            if has_locked {
                self.compact_unlocked_suffix()?;
            } else {
                self.compact()?;
            }
        }
        Ok(())
    }

    /// Compact all tables into a single table.
    ///
    /// `git pack-refs` always rewrites the whole stack into a single,
    /// canonically-laid-out table even when there is just one table, so that
    /// padding/block layout match the configured write options.
    pub fn compact(&mut self) -> Result<()> {
        // Hold the stack lock across the whole compaction (read tables -> write
        // compacted table -> rewrite tables.list -> delete old tables) and work
        // from the freshly-read on-disk list, so a concurrent writer that
        // appended a table after we opened the stack is not silently dropped.
        let guard = self.acquire_tables_list_lock()?;
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

        // Git stores HEAD as a symbolic ref inside the reftable (the on-disk
        // `.git/HEAD` is only a `.invalid` stub). grit keeps the real HEAD in
        // `.git/HEAD`, so inject it into the compacted table to match git's
        // on-disk layout.
        let mut refs = refs;
        let head_log = self.inject_head_ref(&mut refs, min_idx);

        let mut writer = ReftableWriter::new(opts.clone(), min_idx, max_idx);
        for rec in refs {
            writer.add_ref(rec)?;
        }
        if opts.write_log {
            let mut logs = logs;
            if let Some(hl) = head_log {
                logs.push(hl);
            }
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
        self.write_tables_list_locked(&guard)?;

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
        let guard = self.acquire_tables_list_lock()?;
        self.write_tables_list_locked(&guard)
    }

    /// Write `tables.list` while already holding the lock guard.
    fn write_tables_list_locked(&self, guard: &TablesListLock) -> Result<()> {
        let tables_list = self.reftable_dir.join("tables.list");
        let content = self.table_names.join("\n")
            + if self.table_names.is_empty() {
                ""
            } else {
                "\n"
            };
        fs::write(&guard.path, &content).map_err(Error::Io)?;
        // `fs::rename` consumes the lock file; mark the guard disarmed so its
        // Drop does not try to remove the (now-renamed) path.
        fs::rename(&guard.path, &tables_list).map_err(Error::Io)?;
        guard.disarm();
        Ok(())
    }

    fn lock_timeout_ms(&self) -> u64 {
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
        config
            .get("reftable.lockTimeout")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1000)
    }

    /// Atomically acquire `tables.list.lock` (O_CREAT|O_EXCL), retrying up to the
    /// configured `reftable.lockTimeout`. Mirrors git's reftable stack locking so
    /// concurrent writers serialize instead of clobbering each other's
    /// `tables.list` (t0610 'ref transaction: many concurrent writers').
    fn acquire_tables_list_lock(&self) -> Result<TablesListLock> {
        let lock = self.reftable_dir.join("tables.list.lock");
        let timeout_ms = self.lock_timeout_ms();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Ok(TablesListLock::new(lock)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if timeout_ms == 0 || Instant::now() >= deadline {
                        return Err(Error::InvalidRef(
                            "cannot lock references: data is locked".to_owned(),
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(Error::Io(err)),
            }
        }
    }

    /// Re-read `tables.list` from disk, replacing the in-memory view. Used while
    /// holding the lock so a writer always extends the *current* stack rather
    /// than a stale snapshot taken when the stack was first opened.
    fn reload_table_names(&mut self) {
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

/// Path to the repository-local `config` file (linked worktree common dir when needed).
fn repository_config_file(git_dir: &Path) -> Option<PathBuf> {
    let local = git_dir.join("config");
    if local.is_file() {
        return Some(local);
    }
    let common = crate::refs::common_dir(git_dir)?;
    let shared = common.join("config");
    if shared.is_file() {
        Some(shared)
    } else {
        None
    }
}

/// Whether `extensions.refstorage = reftable` is declared in repository-local config only.
pub(crate) fn reftable_declared_in_repository_config(git_dir: &Path) -> bool {
    let Some(path) = repository_config_file(git_dir) else {
        return false;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let mut in_extensions = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_extensions = trimmed.eq_ignore_ascii_case("[extensions]");
            continue;
        }
        if in_extensions {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim().eq_ignore_ascii_case("refstorage")
                    && value.trim().eq_ignore_ascii_case("reftable")
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Detect whether a git directory uses the reftable backend (uncached).
pub fn is_reftable_repo(git_dir: &Path) -> bool {
    reftable_declared_in_repository_config(git_dir)
}

/// Resolve a ref in a reftable repo, following symbolic refs.
pub fn reftable_resolve_ref(git_dir: &Path, refname: &str) -> Result<ObjectId> {
    reftable_resolve_ref_depth(git_dir, refname, 0)
}

fn reftable_storage_location(git_dir: &Path, refname: &str) -> (PathBuf, String) {
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

fn reftable_resolve_ref_depth(git_dir: &Path, refname: &str, depth: usize) -> Result<ObjectId> {
    if depth >= crate::refs::SYMREF_MAXDEPTH {
        return Err(Error::InvalidRef(format!(
            "reftable: symlink too deep: {refname}"
        )));
    }

    // HEAD is special — stored as a file even in reftable repos
    if refname == "HEAD" {
        let head_path = git_dir.join("HEAD");
        if head_path.exists() {
            let content = fs::read_to_string(&head_path).map_err(Error::Io)?;
            let content = content.trim();
            if let Some(target) = content.strip_prefix("ref: ") {
                if target.trim() == "refs/heads/.invalid" {
                    return reftable_resolve_ref_depth(git_dir, "refs/worktree/HEAD", depth + 1);
                }
                return reftable_resolve_ref_depth(git_dir, target.trim(), depth + 1);
            }
            // Detached HEAD
            if content.len() == 40 && content.chars().all(|c| c.is_ascii_hexdigit()) {
                return content.parse();
            }
        }
    }

    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let stack = ReftableStack::open(&store_git_dir)?;
    match stack.lookup_ref(&storage_refname)? {
        Some(rec) => match rec.value {
            RefValue::Val1(oid) => Ok(oid),
            RefValue::Val2(oid, _) => Ok(oid),
            RefValue::Symref(target) => {
                reftable_resolve_ref_depth(&store_git_dir, &target, depth + 1)
            }
            RefValue::Deletion => Err(Error::InvalidRef(format!("ref not found: {refname}"))),
        },
        None => Err(Error::InvalidRef(format!("ref not found: {refname}"))),
    }
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

    // Check config for logAllRefUpdates
    let write_log = log.is_some() || should_log_ref_updates(&store_git_dir);
    let log = if write_log { log } else { None };

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

/// Write multiple reftable ref updates as one transaction per backing store.
///
/// Ref names are routed through the same worktree/common-dir rules as the
/// single-ref helpers. Updates targeting different reftable stacks are grouped
/// by stack; each group is written with one shared update index.
pub fn reftable_write_transaction(
    git_dir: &Path,
    updates: Vec<ReftableTransactionUpdate>,
) -> Result<()> {
    let mut grouped: BTreeMap<PathBuf, Vec<ReftableTransactionUpdate>> = BTreeMap::new();
    for mut update in updates {
        let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, &update.refname);
        update.refname = storage_refname.clone();
        if let Some(log) = update.log.as_mut() {
            log.refname = storage_refname;
        }
        grouped.entry(store_git_dir).or_default().push(update);
    }

    for (store_git_dir, updates) in grouped {
        let mut stack = ReftableStack::open(&store_git_dir)?;
        let opts = read_write_options(&store_git_dir);
        stack.write_transaction(updates, &opts)?;
    }
    Ok(())
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
        return Err(Error::InvalidRef(format!("ref not found: {refname}")));
    }
    let opts = read_write_options(&store_git_dir);
    stack.write_ref(&storage_refname, RefValue::Deletion, None, &opts)
}

/// Read the symbolic target of a ref in a reftable repo.
pub fn reftable_read_symbolic_ref(git_dir: &Path, refname: &str) -> Result<Option<String>> {
    if refname == "HEAD" {
        let head_path = git_dir.join("HEAD");
        let content = match fs::read_to_string(&head_path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        return Ok(content
            .trim()
            .strip_prefix("ref: ")
            .map(|target| target.trim().to_owned()));
    }
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let stack = ReftableStack::open(&store_git_dir)?;
    match stack.lookup_ref(&storage_refname)? {
        Some(rec) => match rec.value {
            RefValue::Symref(target) => Ok(Some(target)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

/// List all refs in a reftable repo under a given prefix.
pub fn reftable_list_refs(git_dir: &Path, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    let stack = ReftableStack::open(git_dir)?;
    let refs = stack.read_refs()?;
    let mut result = Vec::new();
    for rec in refs {
        let matches_prefix = rec.name.starts_with(prefix)
            || (prefix.ends_with('/') && rec.name == prefix.trim_end_matches('/'));
        if matches_prefix {
            match rec.value {
                RefValue::Val1(oid) => result.push((rec.name, oid)),
                RefValue::Val2(oid, _) => result.push((rec.name, oid)),
                RefValue::Symref(target) => {
                    // Try to resolve the symref
                    if let Ok(oid) = reftable_resolve_ref(git_dir, &target) {
                        result.push((rec.name, oid));
                    }
                }
                RefValue::Deletion => {}
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

/// Read reflog entries for a ref from the reftable stack.
pub fn reftable_read_reflog(
    git_dir: &Path,
    refname: &str,
) -> Result<Vec<crate::reflog::ReflogEntry>> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let stack = ReftableStack::open(&store_git_dir)?;
    let logs = stack.read_logs_for_ref(&storage_refname)?;
    let mut entries = Vec::new();
    for log in logs {
        // Reconstruct the identity string
        let tz_sign = if log.tz_offset >= 0 { '+' } else { '-' };
        let tz_abs = log.tz_offset.unsigned_abs();
        let tz_hours = tz_abs / 60;
        let tz_mins = tz_abs % 60;
        let identity = format!(
            "{} <{}> {} {}{:02}{:02}",
            log.name, log.email, log.time_seconds, tz_sign, tz_hours, tz_mins
        );
        // Reftable stores reflog messages with a trailing newline (git's
        // `reftable_writer_add_log` appends one), whereas the files-backend
        // reflog line convention — and thus grit's `ReflogEntry` — keeps the
        // message without its line terminator. Strip a single trailing newline
        // so reflog display is identical regardless of backend.
        let message = log
            .message
            .strip_suffix('\n')
            .map(ToOwned::to_owned)
            .unwrap_or(log.message);
        entries.push(crate::reflog::ReflogEntry {
            old_oid: log.old_id,
            new_oid: log.new_id,
            identity,
            message,
        });
    }
    entries.reverse();
    Ok(entries)
}

/// Replace the reflog entries for a ref in a reftable repo.
pub fn reftable_replace_reflog(
    git_dir: &Path,
    refname: &str,
    entries: &[crate::reflog::ReflogEntry],
) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let mut markers = read_empty_reflog_markers(&store_git_dir);
    if entries.is_empty() {
        markers.insert(storage_refname.clone());
    } else {
        markers.remove(&storage_refname);
    }
    write_empty_reflog_markers(&store_git_dir, &markers)?;
    let mut stack = ReftableStack::open(&store_git_dir)?;
    stack.replace_logs_for_ref(&storage_refname, entries)
}

/// Effective `core.logAllRefUpdates` mode for a reftable store, reading the
/// full config chain (system/global/local) via [`ConfigSet`].
///
/// `should_autocreate_reflog` in `refs.rs` only consults the repo-local
/// `config` file, so a `core.logAllRefUpdates=false` set in the *global* config
/// (as `test_config_global` does) is invisible to it. Reftable stores must see
/// the merged value, so we resolve it here instead.
enum LogRefsMode {
    Always,
    Normal,
    None,
}

fn reftable_log_refs_mode(git_dir: &Path) -> LogRefsMode {
    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .ok();
    let value = config
        .as_ref()
        .and_then(|cfg| cfg.get("core.logAllRefUpdates"));
    match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("always") => LogRefsMode::Always,
        Some("true") | Some("yes") | Some("on") | Some("1") => LogRefsMode::Normal,
        Some("false") | Some("no") | Some("off") | Some("0") | Some("never") => LogRefsMode::None,
        // Unset: git resolves to NONE for bare repos, NORMAL otherwise.
        _ => {
            let bare = config
                .as_ref()
                .and_then(|cfg| cfg.get_bool("core.bare"))
                .and_then(std::result::Result::ok)
                .unwrap_or(false);
            if bare {
                LogRefsMode::None
            } else {
                LogRefsMode::Normal
            }
        }
    }
}

/// Whether a reflog entry should be written for `storage_refname`, mirroring
/// git's reftable-backend `should_write_log`.
fn reftable_should_write_log(git_dir: &Path, storage_refname: &str) -> bool {
    use crate::refs::should_autocreate_reflog_for_mode;
    match reftable_log_refs_mode(git_dir) {
        LogRefsMode::Always => true,
        LogRefsMode::Normal => {
            if should_autocreate_reflog_for_mode(
                storage_refname,
                crate::refs::LogRefsConfig::Normal,
            ) {
                true
            } else {
                reftable_reflog_exists(git_dir, storage_refname)
            }
        }
        LogRefsMode::None => reftable_reflog_exists(git_dir, storage_refname),
    }
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
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    if read_empty_reflog_markers(&store_git_dir).contains(&storage_refname) {
        return true;
    }
    if let Ok(stack) = ReftableStack::open(&store_git_dir) {
        if let Ok(logs) = stack.read_logs_for_ref(&storage_refname) {
            return !logs.is_empty();
        }
    }
    false
}

/// List refs that have reflogs in a reftable repo.
pub fn reftable_list_reflog_refs(git_dir: &Path) -> Result<Vec<String>> {
    let stack = ReftableStack::open(git_dir)?;
    let mut refs: BTreeSet<String> = read_empty_reflog_markers(git_dir);
    for log in stack.read_all_logs()? {
        refs.insert(log.refname);
    }
    Ok(refs.into_iter().collect())
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
    )?;
    Ok(())
}

/// Create an empty reflog marker in a reftable repo.
pub fn reftable_create_reflog(git_dir: &Path, refname: &str) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let mut markers = read_empty_reflog_markers(&store_git_dir);
    markers.insert(storage_refname);
    write_empty_reflog_markers(&store_git_dir, &markers)
}

/// Delete all reflog records and empty-log marker for a ref in a reftable repo.
pub fn reftable_delete_reflog(git_dir: &Path, refname: &str) -> Result<()> {
    let (store_git_dir, storage_refname) = reftable_storage_location(git_dir, refname);
    let mut markers = read_empty_reflog_markers(&store_git_dir);
    markers.remove(&storage_refname);
    write_empty_reflog_markers(&store_git_dir, &markers)?;
    let mut stack = ReftableStack::open(&store_git_dir)?;
    stack.replace_logs_for_ref(&storage_refname, &[])
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

    if let Ok(config) = ConfigSet::load(env, Some(git_dir), true) {
        if let Some(value) = config.get("reftable.blockSize") {
            if let Ok(v) = value.parse::<u32>() {
                opts.block_size = v;
            }
        }
        if let Some(value) = config.get("reftable.restartInterval") {
            if let Ok(v) = value.parse::<usize>() {
                opts.restart_interval = v;
            }
        }
        if let Some(value) = config.get("reftable.indexObjects") {
            let value = value.to_lowercase();
            if value == "false" || value == "0" || value == "no" || value == "off" {
                opts.skip_index_objects = true;
            }
        }
        if let Some(value) = config.get("reftable.geometricFactor") {
            apply_geometric_factor_from_config(&mut opts, &value);
        }
        if let Some(value) = config.get("core.logAllRefUpdates") {
            let value = value.to_lowercase();
            if !(value == "true" || value == "always") {
                opts.write_log = false;
            }
        }
        return opts;
    }

    let config_path = git_dir.join("config");
    if let Ok(content) = fs::read_to_string(&config_path) {
        let mut in_reftable = false;
        let mut in_core = false;
        let mut log_all_ref_updates: Option<bool> = None;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                let section_lower = trimmed.to_lowercase();
                in_reftable = section_lower.starts_with("[reftable]");
                in_core = section_lower.starts_with("[core]");
                continue;
            }
            if in_reftable {
                if let Some((key, value)) = trimmed.split_once('=') {
                    let key = key.trim().to_lowercase();
                    let value = value.trim();
                    match key.as_str() {
                        "blocksize" => {
                            if let Ok(v) = value.parse::<u32>() {
                                opts.block_size = v;
                            }
                        }
                        "restartinterval" => {
                            if let Ok(v) = value.parse::<usize>() {
                                opts.restart_interval = v;
                            }
                        }
                        "indexobjects" => {
                            let value = value.to_lowercase();
                            if value == "false" || value == "0" || value == "no" || value == "off" {
                                opts.skip_index_objects = true;
                            }
                        }
                        "geometricfactor" => {
                            apply_geometric_factor_from_config(&mut opts, value);
                        }
                        _ => {}
                    }
                }
            }
            if in_core {
                if let Some((key, value)) = trimmed.split_once('=') {
                    let key = key.trim().to_lowercase();
                    let value = value.trim().to_lowercase();
                    if key == "logallrefupdates" {
                        log_all_ref_updates = Some(value == "true" || value == "always");
                    }
                }
            }
        }

        if let Some(false) = log_all_ref_updates {
            opts.write_log = false;
        }
    }

    opts
}

/// Check if logAllRefUpdates is enabled.
fn should_log_ref_updates(git_dir: &Path) -> bool {
    let config_path = git_dir.join("config");
    if let Ok(content) = fs::read_to_string(&config_path) {
        let mut in_core = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_core = trimmed.to_lowercase().starts_with("[core]");
                continue;
            }
            if in_core {
                if let Some((key, value)) = trimmed.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("logallrefupdates") {
                        let v = value.trim().to_lowercase();
                        return v == "true" || v == "always";
                    }
                }
            }
        }
    }
    false
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
        return Err(Error::InvalidRef("reftable: file too small".into()));
    }
    if &data[0..4] != REFTABLE_MAGIC {
        return Err(Error::InvalidRef("reftable: bad magic".into()));
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
        if !is_block_type(block_type) {
            break;
        }

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
            let section = match block_type {
                BLOCK_TYPE_LOG => "log",
                BLOCK_TYPE_REF => "ref",
                BLOCK_TYPE_OBJ => "obj",
                BLOCK_TYPE_INDEX => "idx",
                _ => return Err(Error::InvalidRef("reftable: bad block type".into())),
            };
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

fn is_block_type(t: u8) -> bool {
    t == BLOCK_TYPE_REF || t == BLOCK_TYPE_LOG || t == BLOCK_TYPE_OBJ || t == BLOCK_TYPE_INDEX
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
        return Err(Error::InvalidRef(
            "reftable: block trailer out of bounds".into(),
        ));
    }
    let rc = read_u16(data, records_end - 2);
    if rc > MAX_RESTARTS {
        return Err(Error::InvalidRef(format!(
            "reftable: restart count out of range: {rc}"
        )));
    }
    let restart_bytes = rc * 3;
    let Some(restart_table_start) = records_end
        .checked_sub(2)
        .and_then(|without_count| without_count.checked_sub(restart_bytes))
    else {
        return Err(Error::InvalidRef(
            "reftable: restart table larger than block".into(),
        ));
    };
    if restart_table_start < records_data_start {
        return Err(Error::InvalidRef(
            "reftable: restart table overlaps block records".into(),
        ));
    }
    Ok(restart_table_start)
}

/// Parse the footer of a reftable file.
fn parse_footer(data: &[u8], version: u8) -> Result<Footer> {
    let footer_size = if version == 2 { 72 } else { FOOTER_V1_SIZE };
    if data.len() < footer_size {
        return Err(Error::InvalidRef("reftable: footer too small".into()));
    }

    // Verify magic
    if &data[0..4] != REFTABLE_MAGIC {
        return Err(Error::InvalidRef("reftable: bad footer magic".into()));
    }
    let fver = data[4];
    if fver != version {
        return Err(Error::InvalidRef(format!(
            "reftable: footer version mismatch: header={version}, footer={fver}"
        )));
    }

    // Footer-size validated above, so every fixed-width slice below is in
    // bounds; convert via `?` to surface any unexpected truncation as an error.
    let read_u64 = |slice: &[u8]| -> Result<u64> {
        let bytes: [u8; 8] = slice
            .try_into()
            .map_err(|_| Error::InvalidRef("reftable: truncated footer field".into()))?;
        Ok(u64::from_be_bytes(bytes))
    };

    let block_size = ((data[5] as u32) << 16) | ((data[6] as u32) << 8) | (data[7] as u32);
    let min_update_index = read_u64(&data[8..16])?;
    let max_update_index = read_u64(&data[16..24])?;

    // The position fields follow the file header, whose size is version-dependent
    // (24 bytes for v1, 28 for v2 — the extra 4 bytes are the hash-id).
    let off = if version == 2 { 28 } else { 24 };
    let ref_index_position = read_u64(&data[off..off + 8])?;
    let obj_position_and_id_len = read_u64(&data[off + 8..off + 16])?;
    let obj_index_position = read_u64(&data[off + 16..off + 24])?;
    let log_position = read_u64(&data[off + 24..off + 32])?;
    let log_index_position = read_u64(&data[off + 32..off + 40])?;

    // CRC-32 check
    let crc_bytes: [u8; 4] = data[footer_size - 4..footer_size]
        .try_into()
        .map_err(|_| Error::InvalidRef("reftable: truncated footer CRC".into()))?;
    let crc_stored = u32::from_be_bytes(crc_bytes);
    let crc_computed = crc32(&data[..footer_size - 4]);
    if crc_stored != crc_computed {
        return Err(Error::InvalidRef(format!(
            "reftable: footer CRC mismatch: stored={crc_stored:08x}, computed={crc_computed:08x}"
        )));
    }

    Ok(Footer {
        version: fver,
        block_size,
        min_update_index,
        max_update_index,
        ref_index_position,
        obj_position_and_id_len,
        obj_index_position,
        log_position,
        log_index_position,
    })
}

/// Parse an identity string like `"Name <email> 1234567890 +0100"`.
fn parse_identity_string(identity: &str) -> (String, String, u64, i16) {
    // Format: "Name <email> timestamp tz"
    let parts: Vec<&str> = identity.rsplitn(3, ' ').collect();
    if parts.len() < 3 {
        return (identity.to_owned(), String::new(), 0, 0);
    }
    let tz_str = parts[0]; // e.g. "+0100"
    let time_str = parts[1]; // e.g. "1234567890"
    let name_email = parts[2]; // e.g. "Name <email>"

    let time_secs = time_str.parse::<u64>().unwrap_or(0);

    // Parse timezone: +HHMM or -HHMM
    let tz_minutes = if tz_str.len() >= 5 {
        let sign = if tz_str.starts_with('-') { -1i16 } else { 1 };
        let hours = tz_str[1..3].parse::<i16>().unwrap_or(0);
        let mins = tz_str[3..5].parse::<i16>().unwrap_or(0);
        sign * (hours * 60 + mins)
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

    (name, email, time_secs, tz_minutes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
                tz_offset: -480,
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
        assert_eq!(logs[0].tz_offset, -480);
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
        assert_eq!(tz, -480);
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
    fn write_transaction_inject_failure_leaves_tables_list_unchanged() {
        use crate::objects::ObjectId;
        use crate::repo::init_repository;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let repo = init_repository(tmp.path(), false, "main", None, "reftable").unwrap();
        let git_dir = repo.git_dir;
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
}
