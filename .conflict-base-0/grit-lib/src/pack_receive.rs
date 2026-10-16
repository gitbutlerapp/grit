//! Stream side-band pack data from fetch/clone into memory or a pack temp file.
//!
//! HTTP and stateless-RPC responses carry the packfile inside side-band-64k
//! (channel 1). These helpers demux that stream without holding the full pack in
//! a `Vec` when the caller writes to disk instead.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::progress::{ProgressPhase, ProgressUnit};

/// Where demuxed pack bytes are stored.
pub enum PackReceiveTarget<'a> {
    /// Accumulate in a `Vec` (local transports and tests).
    Memory(&'a mut Vec<u8>),
    /// Append to an open temp pack under `objects/pack/`.
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
    if *seen_pack {
        let n = data.len() as u64;
        target.append(data)?;
        return Ok(n);
    }
    pending.extend_from_slice(data);
    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
        *seen_pack = true;
        let chunk = &pending[pos..];
        let n = chunk.len() as u64;
        target.append(chunk)?;
        pending.clear();
        Ok(n)
    } else {
        if pending.len() > 3 {
            let keep_from = pending.len() - 3;
            pending.drain(..keep_from);
        }
        Ok(0)
    }
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
) -> Result<()> {
    let mut pending = Vec::new();
    let mut payload = Vec::with_capacity(65520);
    read_sideband_pack_tail(r, target, progress, &mut pending, &mut false, &mut payload)?;
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

/// Copy `reader` to `writer` with a large stack buffer (faster than [`io::copy`]'s 8 KiB default).
pub fn copy_reader_to_writer(reader: &mut dyn Read, writer: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(Error::Io)?;
        total += n as u64;
    }
    Ok(total)
}

/// `read_sideband_pack_tail` function.
pub fn read_sideband_pack_tail(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
    payload_buf: &mut Vec<u8>,
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

/// A temp pack file under `objects/pack/` for streaming clone/fetch receive.
pub struct TempPackReceive {
    file: BufWriter<File>,
    path: PathBuf,
    seen_pack: bool,
    sideband_payload: Vec<u8>,
}

impl TempPackReceive {
    /// Create `tmp_pack_*` under `objects_dir/pack/`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the directory or file cannot be created.
    pub fn create(objects_dir: &Path) -> Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let pack_dir = objects_dir.join("pack");
        std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
        for _ in 0..64 {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = pack_dir.join(format!("tmp_pack_{}_{seq}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file: BufWriter::with_capacity(4 * 1024 * 1024, file),
                        path,
                        seen_pack: false,
                        sideband_payload: Vec::with_capacity(65520),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Err(Error::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate tmp_pack file name under objects/pack",
        )))
    }

    /// Whether any pack byte (including the `PACK` header) has been written.
    #[must_use]
    pub fn has_pack(&self) -> bool {
        self.seen_pack
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
    ) -> Result<()> {
        let mut target = PackReceiveTarget::File(&mut self.file);
        let mut pending = Vec::new();
        let mut seen = self.seen_pack;
        read_sideband_pack_tail(
            reader,
            &mut target,
            progress,
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
        let len = self.file.get_ref().metadata().map_err(Error::Io)?.len();
        if !self.seen_pack || len == 0 {
            drop(self.file);
            let _ = std::fs::remove_file(&self.path);
            return Ok(None);
        }
        drop(self.file);
        Ok(Some(self.path))
    }

    /// Path to the temp file (for debugging and install).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Completed HTTP pack receive: temp file on disk or in-memory bytes.
pub enum PackReceiveOutcome {
    /// Pack bytes held in memory (fresh clone fast path).
    Memory(Vec<u8>),
    /// Pack written to `objects/pack/tmp_pack_*`.
    File(PathBuf),
}

enum PackReceiveInner {
    File(TempPackReceive),
    Memory(MemoryPackReceive),
}

/// In-memory side-band pack accumulator for small/medium fresh clones.
struct MemoryPackReceive {
    buf: Vec<u8>,
    seen_pack: bool,
    sideband_payload: Vec<u8>,
}

impl MemoryPackReceive {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(8 * 1024 * 1024),
            seen_pack: false,
            sideband_payload: Vec::with_capacity(65520),
        }
    }

    fn finish(self) -> Result<Option<Vec<u8>>> {
        if !self.seen_pack || self.buf.len() < 12 {
            return Ok(None);
        }
        Ok(Some(self.buf))
    }
}

/// Temp pack file receive sink for HTTP stateless-RPC fetch/clone.
pub struct PackReceiveSink(PackReceiveInner);

impl PackReceiveSink {
    /// Create `tmp_pack_*` under `objects_dir/pack/`.
    pub fn create(objects_dir: &Path) -> Result<Self> {
        Ok(Self(PackReceiveInner::File(TempPackReceive::create(
            objects_dir,
        )?)))
    }

    /// Accumulate the pack in memory (fresh clone over HTTP).
    pub fn create_in_memory() -> Self {
        Self(PackReceiveInner::Memory(MemoryPackReceive::new()))
    }

    /// Demux a side-band-64k stream from `reader` into this sink.
    pub fn read_sideband(
        &mut self,
        reader: &mut dyn Read,
        progress: &mut dyn Progress,
    ) -> Result<()> {
        match &mut self.0 {
            PackReceiveInner::File(f) => f.read_sideband(reader, progress),
            PackReceiveInner::Memory(m) => {
                let mut pending = Vec::new();
                let mut seen = m.seen_pack;
                let mut target = PackReceiveTarget::Memory(&mut m.buf);
                read_sideband_pack_tail(
                    reader,
                    &mut target,
                    progress,
                    &mut pending,
                    &mut seen,
                    &mut m.sideband_payload,
                )?;
                m.seen_pack = seen;
                Ok(())
            }
        }
    }

    /// Finish receiving and return pack bytes or a temp path when non-empty.
    pub fn finish(self) -> Result<Option<PackReceiveOutcome>> {
        match self.0 {
            PackReceiveInner::File(f) => Ok(f.finish()?.map(PackReceiveOutcome::File)),
            PackReceiveInner::Memory(m) => Ok(m.finish()?.map(PackReceiveOutcome::Memory)),
        }
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
        read_sideband_pack_to(&mut Cursor::new(stream), &mut target, &mut rec).expect("read");

        assert!(rec.started);
        assert_eq!(rec.total, None);
        assert_eq!(rec.increments, vec![pack1.len() as u64, pack2.len() as u64]);
        assert!(rec.finished);
        assert_eq!(target_buf, b"PACKaaaabbbbbbbb");
    }

    #[test]
    fn temp_pack_finish_leaves_durable_path_with_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let mut recv = TempPackReceive::create(&objects).expect("create");
        let header = b"PACK";
        use std::io::Write;
        recv.write_all(header).expect("write");
        assert!(recv.path().is_file(), "path must exist before finish");
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
}
