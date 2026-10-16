//! File corruption helpers for adversarial pack and index tests.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Flip one byte at `offset` in `path`.
///
/// Returns an I/O error when the file is shorter than `offset + 1`.
pub fn flip_byte_at(path: &Path, offset: u64) -> std::io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut b = [0u8; 1];
    file.read_exact(&mut b)?;
    b[0] ^= 0xff;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&b)?;
    Ok(())
}

/// Truncate `path` to `new_len` bytes.
pub fn truncate_file(path: &Path, new_len: u64) -> std::io::Result<()> {
    File::open(path)?.set_len(new_len)
}

/// Overwrite `data` at `offset` in `path`.
///
/// Returns an error when the file cannot be opened for write or the range extends
/// past the end of the file.
pub fn overwrite_range(path: &Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    Ok(())
}
