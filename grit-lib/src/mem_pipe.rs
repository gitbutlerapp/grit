//! A bounded byte pipe between two threads of one process, kept in memory.
//!
//! Used where a wire-protocol peer runs on another thread of this process (the
//! in-process upload-pack behind local fetches). Unlike an OS pipe, writing after the
//! reader is gone returns [`std::io::ErrorKind::BrokenPipe`] instead of raising
//! `SIGPIPE`, so a failing fetch can never kill an embedding process whose signal setup
//! the library does not control.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Bytes a [`MemPipeWriter`] may have written ahead of its reader before it blocks.
const DEFAULT_CAPACITY: usize = 1024 * 1024;

#[derive(Default)]
struct State {
    buf: VecDeque<u8>,
    writer_open: bool,
    reader_open: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed.wait(guard).unwrap_or_else(|e| e.into_inner())
    }
}

/// The read end of a [`mem_pipe`]. Reads return `Ok(0)` once the writer is dropped and
/// everything it wrote has been read.
pub(crate) struct MemPipeReader {
    shared: Arc<Shared>,
}

/// The write end of a [`mem_pipe`]. Writes block while the pipe is full and fail with
/// [`io::ErrorKind::BrokenPipe`] once the reader is dropped.
pub(crate) struct MemPipeWriter {
    shared: Arc<Shared>,
}

/// A connected reader and writer holding at most 1 MiB in flight.
pub(crate) fn mem_pipe() -> (MemPipeReader, MemPipeWriter) {
    mem_pipe_with_capacity(DEFAULT_CAPACITY)
}

/// [`mem_pipe`] with a chosen capacity in bytes (at least 1).
pub(crate) fn mem_pipe_with_capacity(capacity: usize) -> (MemPipeReader, MemPipeWriter) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            writer_open: true,
            reader_open: true,
            ..State::default()
        }),
        changed: Condvar::new(),
        capacity: capacity.max(1),
    });
    (
        MemPipeReader {
            shared: Arc::clone(&shared),
        },
        MemPipeWriter { shared },
    )
}

impl Read for MemPipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.lock();
        loop {
            if !state.buf.is_empty() {
                let n = out.len().min(state.buf.len());
                for (dst, src) in out.iter_mut().zip(state.buf.drain(..n)) {
                    *dst = src;
                }
                self.shared.changed.notify_all();
                return Ok(n);
            }
            if !state.writer_open {
                return Ok(0);
            }
            state = self.shared.wait(state);
        }
    }
}

impl Drop for MemPipeReader {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.reader_open = false;
        state.buf.clear();
        self.shared.changed.notify_all();
    }
}

impl Write for MemPipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.lock();
        loop {
            if !state.reader_open {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            let room = self.shared.capacity.saturating_sub(state.buf.len());
            if room > 0 {
                let n = room.min(data.len());
                state.buf.extend(&data[..n]);
                self.shared.changed.notify_all();
                return Ok(n);
            }
            state = self.shared.wait(state);
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MemPipeWriter {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.writer_open = false;
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_flow_across_threads_with_backpressure() {
        let (mut r, mut w) = mem_pipe_with_capacity(7);
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let sent = data.clone();
        let writer = std::thread::spawn(move || w.write_all(&sent));
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn writing_after_the_reader_is_gone_is_a_broken_pipe_not_a_signal() {
        let (r, mut w) = mem_pipe_with_capacity(4);
        let writer = std::thread::spawn(move || w.write_all(&[0; 64]));
        drop(r);
        let err = writer.join().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn reader_sees_eof_after_the_writer_is_dropped() {
        let (mut r, mut w) = mem_pipe();
        w.write_all(b"last words").unwrap();
        drop(w);
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "last words");
        assert_eq!(r.read(&mut [0; 4]).unwrap(), 0);
    }
}
