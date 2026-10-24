//! Stream side-band pack data from fetch/clone into memory or a pack temp file.
//!
//! HTTP and stateless-RPC responses carry the packfile inside side-band-64k
//! (channel 1). These helpers demux that stream without holding the full pack in
//! a `Vec` when the caller writes to disk instead.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::objects::HashAlgo;
use crate::pack_stream_scan::PackStreamScanner;
use crate::progress::{poll_cancel, Cancel, ProgressPhase, ProgressUnit, ReceivedObjects};

/// Where demuxed pack bytes are stored.
pub enum PackReceiveTarget<'a> {
    /// Accumulate in a `Vec` (local transports and tests).
    Memory(&'a mut Vec<u8>),
    /// Append to an open temp pack under `objects/incoming/`.
    File(&'a mut dyn Write),
}

impl PackReceiveTarget<'_> {
    fn append(&mut self, data: &[u8]) -> Result<()> {
        match self {
            Self::Memory(v) => {
                v.extend_from_slice(data);
                Ok(())
            }
            Self::File(f) => f.write_all(data).map_err(Error::Io),
        }
    }
}

/// Append channel-1 (or raw) data, scanning for the `PACK` magic that may straddle
/// chunk boundaries.
pub fn append_pack_data(
    data: &[u8],
    target: &mut PackReceiveTarget<'_>,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
) -> Result<u64> {
    append_pack_data_counted(data, target, pending, seen_pack, None)
}

/// [`append_pack_data`] that also hands every pack byte to `counter`.
fn append_pack_data_counted(
    data: &[u8],
    target: &mut PackReceiveTarget<'_>,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
    counter: Option<&mut ObjectCounter>,
) -> Result<u64> {
    let chunk = if *seen_pack {
        data
    } else {
        pending.extend_from_slice(data);
        let Some(pos) = find_pack_magic(pending) else {
            if pending.len() > 3 {
                let keep_from = pending.len() - 3;
                pending.drain(..keep_from);
            }
            return Ok(0);
        };
        *seen_pack = true;
        &pending[pos..]
    };
    let n = chunk.len() as u64;
    target.append(chunk)?;
    if let Some(counter) = counter {
        counter.push(chunk);
    }
    if !pending.is_empty() {
        pending.clear();
    }
    Ok(n)
}

/// Pack bytes handed to the counting thread at a time.
const COUNTER_BATCH: usize = 256 * 1024;
/// Batches the counting thread may fall behind by before counting is
/// abandoned, so a receive faster than inflate never waits or buffers
/// without bound.
const COUNTER_BACKLOG: usize = 64;

/// Counts held by the background thread, read by the receiving thread.
#[derive(Default)]
struct CounterState {
    header_seen: AtomicBool,
    total: AtomicU64,
    received: AtomicU64,
    deltas: AtomicU64,
    stop: AtomicBool,
}

/// Counts a pack's objects on a background thread while the receive loop
/// writes its bytes (see [`crate::pack_stream_scan`]).
struct ObjectCounter {
    sender: Option<SyncSender<Vec<u8>>>,
    batch: Vec<u8>,
    state: Arc<CounterState>,
    worker: Option<JoinHandle<()>>,
    last_reported: Option<ReceivedObjects>,
}

impl ObjectCounter {
    fn spawn(hash: HashAlgo) -> Option<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(COUNTER_BACKLOG);
        let state = Arc::new(CounterState::default());
        let shared = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("grit-pack-count".to_owned())
            .spawn(move || {
                let mut scanner = PackStreamScanner::new(hash);
                for chunk in receiver {
                    if shared.stop.load(Ordering::Relaxed) || scanner.feed(&chunk).is_err() {
                        return;
                    }
                    if let Some(counts) = scanner.counts() {
                        shared.total.store(counts.total, Ordering::Relaxed);
                        shared.deltas.store(counts.deltas, Ordering::Relaxed);
                        shared.received.store(counts.received, Ordering::Relaxed);
                        shared.header_seen.store(true, Ordering::Release);
                    }
                    if scanner.is_complete() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Self {
            sender: Some(sender),
            batch: Vec::with_capacity(COUNTER_BATCH),
            state,
            worker: Some(worker),
            last_reported: None,
        })
    }

    fn push(&mut self, data: &[u8]) {
        let Some(sender) = &self.sender else {
            return;
        };
        self.batch.extend_from_slice(data);
        if self.batch.len() < COUNTER_BATCH {
            return;
        }
        let batch = std::mem::replace(&mut self.batch, Vec::with_capacity(COUNTER_BATCH));
        match sender.try_send(batch) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Inflate cannot keep up with this transfer; stop counting
                // rather than slow the receive down.
                self.state.stop.store(true, Ordering::Relaxed);
                self.sender = None;
            }
            Err(TrySendError::Disconnected(_)) => self.sender = None,
        }
    }

    /// Hand `progress` the latest counts if they moved since the last report.
    fn report(&mut self, progress: &mut dyn Progress) {
        if !self.state.header_seen.load(Ordering::Acquire) {
            return;
        }
        let counts = ReceivedObjects {
            received: self.state.received.load(Ordering::Relaxed),
            total: self.state.total.load(Ordering::Relaxed),
            deltas: self.state.deltas.load(Ordering::Relaxed),
        };
        if self.last_reported != Some(counts) {
            self.last_reported = Some(counts);
            progress.received_objects(counts);
        }
    }
}

impl Drop for ObjectCounter {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        self.sender = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn find_pack_magic(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"PACK")
}

fn note_received_pack_bytes(progress: &mut dyn Progress, active: &mut bool, nbytes: u64) {
    if nbytes == 0 {
        return;
    }
    if !*active {
        progress.start_with_unit(ProgressPhase::ReceivingObjects, ProgressUnit::Bytes, None);
        *active = true;
    }
    progress.inc(nbytes);
}

fn note_received_pack_bytes_coalesced(
    progress: &mut dyn Progress,
    active: &mut bool,
    pending: &mut u64,
    nbytes: u64,
) {
    if nbytes == 0 {
        return;
    }
    *pending += nbytes;
    const COALESCE: u64 = 256 * 1024;
    if *pending >= COALESCE {
        note_received_pack_bytes(progress, active, *pending);
        *pending = 0;
    }
}

fn flush_coalesced_progress(progress: &mut dyn Progress, active: &mut bool, pending: &mut u64) {
    if *pending > 0 {
        note_received_pack_bytes(progress, active, *pending);
        *pending = 0;
    }
}

fn finish_receiving(progress: &mut dyn Progress, active: bool) {
    if active {
        progress.finish();
    }
}

/// Demultiplex a side-band-64k stream: pack bytes go to `target`, channel 2 to
/// `progress`, channel 3 is a fatal error.
pub fn read_sideband_pack_to(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<()> {
    crate::trace::trace_span!("grit.pack.receive");
    let mut pending = Vec::new();
    let mut payload = Vec::with_capacity(65520);
    read_sideband_pack_tail(
        r,
        target,
        progress,
        cancel,
        &mut pending,
        &mut false,
        &mut payload,
    )?;
    Ok(())
}

/// Continue demux after the first side-band payload (or mid-stream) with existing
/// `pending` / `seen_pack` state.
fn pkt_line_payload_len(len_buf: &[u8; 4]) -> Result<usize> {
    let mut len = 0usize;
    for &b in len_buf {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as usize,
            b'a'..=b'f' => (b - b'a' + 10) as usize,
            b'A'..=b'F' => (b - b'A' + 10) as usize,
            _ => return Err(Error::Message("bad pkt length".to_owned())),
        };
        len = (len << 4) | digit;
    }
    Ok(len)
}

fn read_interrupted(reader: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    const ATTEMPTS: usize = 3;
    let mut attempt = 0usize;
    loop {
        match reader.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted && attempt + 1 < ATTEMPTS => {
                attempt += 1;
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
}

/// Copy `reader` to `writer` with a large buffer (faster than [`io::copy`]'s 8 KiB default).
pub fn copy_reader_to_writer(reader: &mut dyn Read, writer: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = read_interrupted(reader, &mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(Error::Io)?;
        total += n as u64;
    }
    Ok(total)
}

fn sideband_unexpected_eof() -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "unexpected end of side-band stream",
    ))
}

const SIDEBAND_IO_CHUNK: usize = 1024 * 1024;

struct SidebandStreamBuf {
    storage: Vec<u8>,
    pos: usize,
    end: usize,
}

impl SidebandStreamBuf {
    fn new() -> Self {
        Self {
            storage: vec![0; SIDEBAND_IO_CHUNK],
            pos: 0,
            end: 0,
        }
    }

    fn compact(&mut self) {
        if self.pos == 0 {
            return;
        }
        self.storage.copy_within(self.pos..self.end, 0);
        self.end -= self.pos;
        self.pos = 0;
    }

    fn refill(&mut self, r: &mut dyn Read) -> Result<bool> {
        self.compact();
        if self.end >= self.storage.len() {
            return Ok(false);
        }
        let n = read_interrupted(r, &mut self.storage[self.end..])?;
        self.end += n;
        Ok(n > 0)
    }

    fn available(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }
}

/// `read_sideband_pack_tail` function.
pub fn read_sideband_pack_tail(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
    payload_buf: &mut Vec<u8>,
) -> Result<()> {
    read_sideband_pack_tail_buffered(
        r,
        target,
        progress,
        cancel,
        pending,
        seen_pack,
        payload_buf,
        &mut SidebandStreamBuf::new(),
    )
}

fn read_sideband_pack_tail_buffered(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
    _payload_buf: &mut Vec<u8>,
    stream: &mut SidebandStreamBuf,
) -> Result<()> {
    let mut receiving = false;
    let mut progress_pending = 0u64;
    // Only a pack that starts in this call can be counted from its header.
    let mut counter = (!*seen_pack && progress.wants_received_objects())
        .then(|| ObjectCounter::spawn(HashAlgo::Sha1))
        .flatten();
    loop {
        poll_cancel(progress, cancel)?;
        while stream.available() < 4 {
            if !stream.refill(r)? {
                if stream.available() == 0 {
                    flush_coalesced_progress(progress, &mut receiving, &mut progress_pending);
                    finish_receiving(progress, receiving);
                    return Ok(());
                }
                break;
            }
        }
        if stream.available() < 4 {
            flush_coalesced_progress(progress, &mut receiving, &mut progress_pending);
            finish_receiving(progress, receiving);
            if stream.available() == 0 {
                return Ok(());
            }
            return Err(sideband_unexpected_eof());
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&stream.storage[stream.pos..stream.pos + 4]);
        let len = pkt_line_payload_len(&len_buf)?;
        match len {
            0 => {
                stream.pos += 4;
                if *seen_pack {
                    flush_coalesced_progress(progress, &mut receiving, &mut progress_pending);
                    finish_receiving(progress, receiving);
                    return Ok(());
                }
                continue;
            }
            1 | 2 => {
                stream.pos += 4;
                continue;
            }
            n if n <= 4 => {
                return Err(Error::Message(format!(
                    "invalid pkt-line length in side-band stream: {n}"
                )))
            }
            _ => {}
        }
        let payload_len = len - 4;
        while stream.available() < 4 + payload_len {
            if !stream.refill(r)? {
                return Err(sideband_unexpected_eof());
            }
        }
        let payload = &stream.storage[stream.pos + 4..stream.pos + 4 + payload_len];
        stream.pos += 4 + payload_len;
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => {
                let data = &payload[1..];
                let nbytes = if *seen_pack && counter.is_none() {
                    target.append(data)?;
                    data.len() as u64
                } else {
                    append_pack_data_counted(data, target, pending, seen_pack, counter.as_mut())?
                };
                note_received_pack_bytes_coalesced(
                    progress,
                    &mut receiving,
                    &mut progress_pending,
                    nbytes,
                );
                if let (Some(counter), true) = (&mut counter, receiving) {
                    counter.report(progress);
                }
            }
            2 => progress.message(&payload[1..]),
            3 => {
                return Err(Error::Message(format!(
                    "remote error: {}",
                    String::from_utf8_lossy(&payload[1..]).trim_end()
                )))
            }
            _ => {
                let nbytes = append_pack_data_counted(
                    payload,
                    target,
                    pending,
                    seen_pack,
                    counter.as_mut(),
                )?;
                note_received_pack_bytes_coalesced(
                    progress,
                    &mut receiving,
                    &mut progress_pending,
                    nbytes,
                );
                if let (Some(counter), true) = (&mut counter, receiving) {
                    counter.report(progress);
                }
            }
        }
    }
}

/// Per-packet `read_exact` side-band demux (pre-#1188 path). Used only by regression tests.
#[cfg(test)]
fn read_sideband_pack_tail_read_exact_loop(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
    payload_buf: &mut Vec<u8>,
    staging_bytes: &mut u64,
) -> Result<()> {
    let mut receiving = false;
    loop {
        let mut len_buf = [0u8; 4];
        match r.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let len = pkt_line_payload_len(&len_buf)?;
        match len {
            0 => {
                if *seen_pack {
                    break;
                }
                continue;
            }
            1 | 2 => continue,
            n if n <= 4 => {
                return Err(Error::Message(format!(
                    "invalid pkt-line length in side-band stream: {n}"
                )))
            }
            _ => {}
        }
        let payload_len = len - 4;
        if payload_buf.len() < payload_len {
            payload_buf.resize(payload_len, 0);
        }
        payload_buf.truncate(payload_len);
        *staging_bytes += payload_len as u64;
        r.read_exact(payload_buf)?;
        if payload_buf.is_empty() {
            continue;
        }
        match payload_buf[0] {
            1 => {
                let nbytes = if *seen_pack {
                    let data = &payload_buf[1..];
                    let n = data.len() as u64;
                    target.append(data)?;
                    n
                } else {
                    append_pack_data(&payload_buf[1..], target, pending, seen_pack)?
                };
                note_received_pack_bytes(progress, &mut receiving, nbytes);
            }
            2 => progress.message(&payload_buf[1..]),
            3 => {
                return Err(Error::Message(format!(
                    "remote error: {}",
                    String::from_utf8_lossy(&payload_buf[1..]).trim_end()
                )))
            }
            _ => {
                let nbytes = append_pack_data(payload_buf, target, pending, seen_pack)?;
                note_received_pack_bytes(progress, &mut receiving, nbytes);
            }
        }
    }
    finish_receiving(progress, receiving);
    Ok(())
}

/// Removes the spool path on drop unless [`Self::disarm`] ran (ownership handed off).
struct SpoolFileGuard {
    path: PathBuf,
    delete_on_drop: bool,
}

impl SpoolFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            delete_on_drop: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.delete_on_drop = false;
    }
}

impl Drop for SpoolFileGuard {
    fn drop(&mut self) {
        if self.delete_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A temp pack file under `objects/incoming/` for streaming clone/fetch receive.
pub struct TempPackReceive {
    file: BufWriter<File>,
    spool: SpoolFileGuard,
    seen_pack: bool,
    sideband_payload: Vec<u8>,
}

fn pack_receive_incoming_dir(objects_dir: &Path) -> PathBuf {
    objects_dir.join("incoming")
}

fn ensure_pack_receive_incoming_dir(objects_dir: &Path) -> Result<PathBuf> {
    let dir = pack_receive_incoming_dir(objects_dir);
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(Error::Io)?;
    }
    Ok(dir)
}

fn create_private_spool_file(path: &Path) -> Result<File> {
    // Receive-pack spool segment (not a lock file); exclusive create avoids clobber.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(Error::Io)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(Error::Io)
    }
}

/// Ensure pack bytes are visible to a follow-up open in this process.
///
/// Git does not fsync in-progress receive spools; a single `fdatasync` on Unix at
/// handoff is enough before [`assert_received_pack_durable`] and index-pack mmap/read.
fn sync_spool_for_handoff(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        file.sync_data().map_err(Error::Io)
    }
    #[cfg(not(unix))]
    {
        file.sync_all().map_err(Error::Io)
    }
}

fn assert_received_pack_durable(path: &Path, expected_len: u64) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 8;
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(25_u64.saturating_mul(attempt as u64)));
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::Io(e)),
        };
        if !meta.is_file() {
            return Err(Error::ReceivedPackTempMissing {
                path: path.to_path_buf(),
            });
        }
        if meta.len() != expected_len {
            if attempt + 1 < MAX_ATTEMPTS {
                continue;
            }
            return Err(Error::CorruptObject(format!(
                "received pack size mismatch: expected {expected_len}, got {}",
                meta.len()
            )));
        }
        let mut header = [0u8; 4];
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::Io(e)),
        };
        match f.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) if attempt + 1 < MAX_ATTEMPTS => continue,
            Err(e) => return Err(Error::Io(e)),
        }
        if &header != b"PACK" {
            return Err(Error::CorruptObject(
                "received data is not a pack stream".to_owned(),
            ));
        }
        return Ok(());
    }
    Err(Error::ReceivedPackTempMissing {
        path: path.to_path_buf(),
    })
}

impl TempPackReceive {
    /// Create a `pack-receive-*.tmp` file under `objects_dir/incoming/`.
    ///
    /// Spool files stay inside the repository object store (same filesystem as
    /// `objects/pack/`) but outside published pack names until index-pack runs.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the directory or file cannot be created.
    pub fn create(objects_dir: &Path) -> Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let spool_dir = ensure_pack_receive_incoming_dir(objects_dir)?;
        for _ in 0..64 {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = spool_dir.join(format!("pack-receive-{}_{seq}.tmp", std::process::id()));
            match create_private_spool_file(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file: BufWriter::with_capacity(4 * 1024 * 1024, file),
                        spool: SpoolFileGuard::new(path),
                        seen_pack: false,
                        sideband_payload: Vec::with_capacity(65520),
                    });
                }
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate pack-receive temp file name under objects/incoming",
        )))
    }

    /// Demux side-band from `reader` into this temp file.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, or I/O errors from the read or write path.
    pub fn read_sideband(
        &mut self,
        reader: &mut dyn Read,
        progress: &mut dyn Progress,
        cancel: &dyn Cancel,
    ) -> Result<()> {
        let mut target = PackReceiveTarget::File(&mut self.file);
        let mut pending = Vec::new();
        let mut seen = self.seen_pack;
        read_sideband_pack_tail(
            reader,
            &mut target,
            progress,
            cancel,
            &mut pending,
            &mut seen,
            &mut self.sideband_payload,
        )?;
        self.seen_pack = seen;
        self.file.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Finish receiving: return the path when a non-empty pack was written, else
    /// remove the empty temp file and return `None`.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from metadata or removal.
    pub fn finish(mut self) -> Result<Option<PathBuf>> {
        self.file.flush().map_err(Error::Io)?;
        sync_spool_for_handoff(self.file.get_ref())?;
        let len = self.file.get_ref().metadata().map_err(Error::Io)?.len();
        if !self.seen_pack || len == 0 {
            drop(self.file);
            return Ok(None);
        }
        let path = self.spool.path().to_path_buf();
        drop(self.file);
        assert_received_pack_durable(&path, len)?;
        self.spool.disarm();
        Ok(Some(path))
    }

    /// Path to the temp file (for debugging and install).
    #[must_use]
    pub fn path(&self) -> &Path {
        self.spool.path()
    }
}

impl Write for TempPackReceive {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            self.seen_pack = true;
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::Progress;
    use crate::progress::ProgressPhase;
    use std::io::Cursor;

    #[derive(Default)]
    struct ReceiveRecorder {
        started: bool,
        total: Option<u64>,
        increments: Vec<u64>,
        finished: bool,
    }

    impl Progress for ReceiveRecorder {}

    impl crate::progress::ProgressSink for ReceiveRecorder {
        fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
            assert_eq!(phase, ProgressPhase::ReceivingObjects);
            self.started = true;
            self.total = total;
        }
        fn inc(&mut self, units: u64) {
            self.increments.push(units);
        }
        fn finish(&mut self) {
            self.finished = true;
        }
    }

    fn sideband_pkt(channel: u8, data: &[u8]) -> Vec<u8> {
        let payload_len = 1 + data.len();
        let total_len = payload_len + 4;
        let mut out = format!("{total_len:04x}").into_bytes();
        out.push(channel);
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn reports_received_bytes_per_packet() {
        let pack1 = b"PACKaaaa";
        let pack2 = b"bbbbbbbb";
        let mut stream = Vec::new();
        stream.extend_from_slice(&sideband_pkt(1, pack1));
        stream.extend_from_slice(&sideband_pkt(1, pack2));
        stream.extend_from_slice(b"0000");

        let mut target_buf = Vec::new();
        let mut target = PackReceiveTarget::Memory(&mut target_buf);
        let mut rec = ReceiveRecorder::default();
        read_sideband_pack_to(
            &mut Cursor::new(stream),
            &mut target,
            &mut rec,
            &crate::progress::NeverCancel,
        )
        .expect("read");

        assert!(rec.started);
        assert_eq!(rec.total, None);
        assert_eq!(
            rec.increments.iter().sum::<u64>(),
            (pack1.len() + pack2.len()) as u64
        );
        assert!(rec.finished);
        assert_eq!(target_buf, b"PACKaaaabbbbbbbb");
    }

    fn synthetic_band1_sideband_stream(packets: usize, payload_bytes: usize) -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(&sideband_pkt(1, b"PACK"));
        let payload = vec![0xcd_u8; payload_bytes];
        for _ in 0..packets {
            stream.extend_from_slice(&sideband_pkt(1, &payload));
        }
        stream.extend_from_slice(b"0000");
        stream
    }

    /// Regression for #1188: the legacy per-packet `read_exact` loop copies every
    /// side-band payload through a staging `Vec` (~2× memory traffic before PACK
    /// is found). The buffered demux writes channel-1 bytes directly from its
    /// read window and never touches the staging buffer on this stream.
    #[test]
    fn buffered_sideband_demux_avoids_per_packet_staging_buffer() {
        const PACKETS: usize = 64;
        const PAYLOAD: usize = 65_515;
        let stream = synthetic_band1_sideband_stream(PACKETS, PAYLOAD);

        let mut legacy_out = Vec::new();
        let mut legacy_target = PackReceiveTarget::Memory(&mut legacy_out);
        let mut legacy_pending = Vec::new();
        let mut legacy_seen = false;
        let mut legacy_scratch = Vec::with_capacity(65520);
        let mut legacy_staging = 0u64;
        let mut legacy_rec = ReceiveRecorder::default();
        read_sideband_pack_tail_read_exact_loop(
            &mut Cursor::new(stream.clone()),
            &mut legacy_target,
            &mut legacy_rec,
            &mut legacy_pending,
            &mut legacy_seen,
            &mut legacy_scratch,
            &mut legacy_staging,
        )
        .expect("legacy demux");

        let mut opt_out = Vec::new();
        let mut opt_target = PackReceiveTarget::Memory(&mut opt_out);
        let mut opt_pending = Vec::new();
        let mut opt_seen = false;
        let mut opt_scratch = Vec::with_capacity(65520);
        let mut opt_rec = ReceiveRecorder::default();
        read_sideband_pack_tail(
            &mut Cursor::new(stream),
            &mut opt_target,
            &mut opt_rec,
            &crate::progress::NeverCancel,
            &mut opt_pending,
            &mut opt_seen,
            &mut opt_scratch,
        )
        .expect("buffered demux");

        assert_eq!(legacy_out, opt_out);
        let min_legacy_staging = PACKETS as u64 * (PAYLOAD as u64 + 1);
        assert!(
            legacy_staging >= min_legacy_staging,
            "legacy path should stage every pkt payload (>= {min_legacy_staging}), got {legacy_staging}"
        );
        assert!(
            opt_scratch.is_empty(),
            "buffered demux should not use the staging Vec on band-1-only streams"
        );
    }

    /// Asks for object counts and records what it is told.
    #[derive(Default)]
    struct CountRecorder {
        counts: Vec<ReceivedObjects>,
    }

    impl crate::progress::ProgressSink for CountRecorder {}

    impl Progress for CountRecorder {
        fn wants_received_objects(&self) -> bool {
            true
        }
        fn received_objects(&mut self, counts: ReceivedObjects) {
            self.counts.push(counts);
        }
    }

    #[test]
    fn counts_objects_while_receiving_when_asked() {
        use grit_test_support::objects::{HashAlgo as FixtureAlgo, ObjectKind as PK, PackBuilder};
        let mut pb = PackBuilder::new(FixtureAlgo::Sha1);
        // Incompressible blobs, so the pack spans several counter batches.
        let mut seed = 0x9e37_79b9_u32;
        for _ in 0..12 {
            let blob: Vec<u8> = (0..64 * 1024)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            pb.add_full(PK::Blob, &blob);
        }
        let pack = pb.build().bytes;
        let mut wire = Vec::new();
        for piece in pack.chunks(65_000) {
            wire.extend(sideband_pkt(1, piece));
        }
        wire.extend_from_slice(b"0000");

        let mut out = Vec::new();
        let mut target = PackReceiveTarget::Memory(&mut out);
        let mut rec = CountRecorder::default();
        read_sideband_pack_to(
            &mut Cursor::new(wire),
            &mut target,
            &mut rec,
            &crate::progress::NeverCancel,
        )
        .expect("receive");
        assert_eq!(out, pack);
        // The counting thread runs behind the receive, so how far it got by
        // the last report varies; every report is consistent and ordered.
        assert!(rec.counts.iter().all(|c| c.total == 12 && c.received <= 12));
        assert!(rec
            .counts
            .windows(2)
            .all(|w| w[0].received <= w[1].received));
    }

    #[test]
    fn does_not_count_objects_unless_asked() {
        let mut wire = sideband_pkt(1, b"PACK\0\0\0\x02\0\0\0\0");
        wire.extend_from_slice(b"0000");
        let mut out = Vec::new();
        let mut target = PackReceiveTarget::Memory(&mut out);
        let mut rec = ReceiveRecorder::default();
        read_sideband_pack_to(
            &mut Cursor::new(wire),
            &mut target,
            &mut rec,
            &crate::progress::NeverCancel,
        )
        .expect("receive");
        assert_eq!(out.len(), 12);
    }

    #[test]
    fn truncated_sideband_payload_is_unexpected_eof() {
        // First pkt: band-1 "PACK" (0009 = 4 + 5 payload bytes). Second pkt declares 12
        // payload bytes but the stream ends after "truncated" (11 bytes) — must not Ok().
        let stream = b"0009\x01PACK0010\x01truncated";
        let mut target_buf = Vec::new();
        let mut target = PackReceiveTarget::Memory(&mut target_buf);
        let mut pending = Vec::new();
        let mut seen = false;
        let mut scratch = Vec::new();
        let err = read_sideband_pack_tail(
            &mut Cursor::new(stream),
            &mut target,
            &mut ReceiveRecorder::default(),
            &crate::progress::NeverCancel,
            &mut pending,
            &mut seen,
            &mut scratch,
        )
        .expect_err("truncated side-band payload");
        match err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn temp_pack_finish_leaves_durable_path_with_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let mut recv = TempPackReceive::create(&objects).expect("create");
        let header = b"PACK";
        use std::io::Write;
        recv.write_all(header).expect("write");
        let path = recv.finish().expect("finish").expect("some pack");
        assert!(path.is_file(), "finish must return an on-disk path");
        assert_eq!(std::fs::read(&path).expect("read"), header);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn temp_pack_finish_removes_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let recv = TempPackReceive::create(&objects).expect("create");
        let path = recv.path().to_path_buf();
        assert!(recv.finish().expect("finish").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn temp_pack_spools_under_objects_incoming_not_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        std::fs::create_dir_all(objects.join("pack")).expect("pack dir");
        let recv = TempPackReceive::create(&objects).expect("create");
        let path = recv.path().to_path_buf();
        assert!(
            path.starts_with(objects.join("incoming")),
            "temp pack must live under objects/incoming during receive"
        );
        assert!(
            !path.starts_with(objects.join("pack")),
            "temp pack must not live under objects/pack during receive"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn temp_pack_drop_after_partial_receive_removes_spool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let mut recv = TempPackReceive::create(&objects).expect("create");
        let path = recv.path().to_path_buf();
        use std::io::Write;
        recv.write_all(b"PACKpartial").expect("write");
        recv.flush().expect("flush");
        assert!(path.is_file());
        drop(recv);
        assert!(
            !path.exists(),
            "partial spool must be removed when TempPackReceive drops without finish"
        );
    }

    #[test]
    fn temp_pack_drop_after_sideband_error_removes_spool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let mut recv = TempPackReceive::create(&objects).expect("create");
        let path = recv.path().to_path_buf();
        let mut stream = sideband_pkt(1, b"PACKaaaa");
        stream.extend_from_slice(&sideband_pkt(3, b"network interrupted"));
        let mut rec = ReceiveRecorder::default();
        let err = recv
            .read_sideband(
                &mut Cursor::new(stream),
                &mut rec,
                &crate::progress::NeverCancel,
            )
            .expect_err("remote side-band error");
        assert!(matches!(err, Error::Message(_)));
        drop(recv);
        assert!(
            !path.exists(),
            "spool must be removed after failed read_sideband"
        );
    }

    #[cfg(unix)]
    #[test]
    fn temp_pack_spool_file_is_private_to_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let recv = TempPackReceive::create(&objects).expect("create");
        let mode = std::fs::metadata(recv.path())
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "spool file must not be world-readable");
        let path = recv.path().to_path_buf();
        drop(recv);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn received_pack_durable_check_missing_file_is_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("removed.pack");
        std::fs::write(&path, b"PACK").expect("write");
        let len = std::fs::metadata(&path).expect("meta").len();
        std::fs::remove_file(&path).expect("remove");
        let err = assert_received_pack_durable(&path, len).expect_err("missing pack");
        assert!(matches!(err, Error::ReceivedPackTempMissing { .. }));
    }
}
