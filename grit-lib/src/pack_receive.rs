//! Stream side-band pack data from fetch/clone into memory or a pack temp file.
//!
//! HTTP and stateless-RPC responses carry the packfile inside side-band-64k
//! (channel 1). These helpers demux that stream without holding the full pack in
//! a `Vec` when the caller writes to disk instead.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fetch::Progress;

/// Where demuxed pack bytes are stored.
pub enum PackReceiveTarget<'a> {
    /// Accumulate in a `Vec` (local transports and tests).
    Memory(&'a mut Vec<u8>),
    /// Append to an open temp pack under `objects/pack/`.
    File(&'a mut File),
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
) -> Result<()> {
    if *seen_pack {
        return target.append(data);
    }
    pending.extend_from_slice(data);
    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
        *seen_pack = true;
        target.append(&pending[pos..])?;
        pending.clear();
    } else if pending.len() > 3 {
        let keep_from = pending.len() - 3;
        pending.drain(..keep_from);
    }
    Ok(())
}

/// Demultiplex a side-band-64k stream: pack bytes go to `target`, channel 2 to
/// `progress`, channel 3 is a fatal error.
pub fn read_sideband_pack_to(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
) -> Result<()> {
    read_sideband_pack_tail(r, target, progress, &mut Vec::new(), &mut false)
}

/// Continue demux after the first side-band payload (or mid-stream) with existing
/// `pending` / `seen_pack` state.
pub fn read_sideband_pack_tail(
    r: &mut dyn Read,
    target: &mut PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    pending: &mut Vec<u8>,
    seen_pack: &mut bool,
) -> Result<()> {
    loop {
        let mut len_buf = [0u8; 4];
        match r.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let len_str = std::str::from_utf8(&len_buf)
            .map_err(|_| Error::Message("bad pkt length".to_owned()))?;
        let len = usize::from_str_radix(len_str, 16)
            .map_err(|_| Error::Message("bad pkt length".to_owned()))?;
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
        let mut payload = vec![0u8; len - 4];
        r.read_exact(&mut payload)?;
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => append_pack_data(&payload[1..], target, pending, seen_pack)?,
            2 => progress.message(&payload[1..]),
            3 => {
                return Err(Error::Message(format!(
                    "remote error: {}",
                    String::from_utf8_lossy(&payload[1..]).trim_end()
                )))
            }
            _ => append_pack_data(&payload, target, pending, seen_pack)?,
        }
    }
    Ok(())
}

/// A temp pack file under `objects/pack/` for streaming clone/fetch receive.
pub struct TempPackReceive {
    file: File,
    path: PathBuf,
    seen_pack: bool,
}

impl TempPackReceive {
    /// Create `tmp_pack_*` under `objects_dir/pack/`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the directory or file cannot be created.
    pub fn create(objects_dir: &Path) -> Result<Self> {
        let pack_dir = objects_dir.join("pack");
        std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
        let tmp = tempfile::Builder::new()
            .prefix("tmp_pack_")
            .tempfile_in(&pack_dir)
            .map_err(Error::Io)?;
        let path = tmp.path().to_path_buf();
        let file = tmp.into_file();
        Ok(Self {
            file,
            path,
            seen_pack: false,
        })
    }

    /// Whether any pack byte (including the `PACK` header) has been written.
    #[must_use]
    pub fn has_pack(&self) -> bool {
        self.seen_pack
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn mark_seen_pack(&mut self, seen: bool) {
        if seen {
            self.seen_pack = true;
        }
    }

    pub(crate) fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        if !data.is_empty() {
            self.file.write_all(data).map_err(Error::Io)?;
            if data.windows(4).any(|w| w == b"PACK") {
                self.seen_pack = true;
            }
        }
        Ok(())
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
        let mut seen = false;
        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let len_str = std::str::from_utf8(&len_buf)
                .map_err(|_| Error::Message("bad pkt length".to_owned()))?;
            let len = usize::from_str_radix(len_str, 16)
                .map_err(|_| Error::Message("bad pkt length".to_owned()))?;
            match len {
                0 => {
                    if seen {
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
            let mut payload = vec![0u8; len - 4];
            reader.read_exact(&mut payload)?;
            if payload.is_empty() {
                continue;
            }
            match payload[0] {
                1 => {
                    append_pack_data(&payload[1..], &mut target, &mut pending, &mut seen)?;
                }
                2 => progress.message(&payload[1..]),
                3 => {
                    return Err(Error::Message(format!(
                        "remote error: {}",
                        String::from_utf8_lossy(&payload[1..]).trim_end()
                    )))
                }
                _ => append_pack_data(&payload, &mut target, &mut pending, &mut seen)?,
            }
        }
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
        if !self.seen_pack || self.file.metadata().map_err(Error::Io)?.len() == 0 {
            drop(self.file);
            let _ = std::fs::remove_file(&self.path);
            return Ok(None);
        }
        self.file.flush().map_err(Error::Io)?;
        drop(self.file);
        Ok(Some(self.path))
    }

    /// Path to the temp file (for debugging and install).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
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
