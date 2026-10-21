//! Split raw side-band channel-2 chunks into typed remote progress lines.
//!
//! Git upload-pack multiplexes progress on side-band channel 2. Payload bytes
//! use `\r` for in-place counter updates and `\n` for final lines; either
//! delimiter may straddle pkt-line boundaries.

/// One decoded fragment of remote progress text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteLine {
    /// In-place update (payload terminated with `\r`).
    Update(String),
    /// Final line (payload terminated with `\n`).
    Line(String),
}

/// Incrementally decode channel-2 progress bytes into [`RemoteLine`] items.
#[derive(Debug, Default)]
pub struct SidebandLines {
    pending: Vec<u8>,
}

impl SidebandLines {
    /// Create an empty splitter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Append `chunk` and invoke `emit` for each complete line decoded so far.
    pub fn feed<F>(&mut self, chunk: &[u8], mut emit: F)
    where
        F: FnMut(RemoteLine),
    {
        if chunk.is_empty() {
            return;
        }
        self.pending.extend_from_slice(chunk);
        while let Some(i) = self.pending.iter().position(|&b| b == b'\r' || b == b'\n') {
            let delim = self.pending[i];
            let line = self.pending.drain(..i).collect::<Vec<_>>();
            self.pending.drain(..1);
            let text = String::from_utf8_lossy(&line).into_owned();
            let item = if delim == b'\r' {
                RemoteLine::Update(text)
            } else {
                RemoteLine::Line(text)
            };
            emit(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<RemoteLine> {
        let mut splitter = SidebandLines::new();
        let mut out = Vec::new();
        for chunk in chunks {
            splitter.feed(*chunk, |line| out.push(line));
        }
        out
    }

    #[test]
    fn cr_updates() {
        assert_eq!(
            collect(&[b"one\r two\r"]),
            vec![
                RemoteLine::Update("one".into()),
                RemoteLine::Update(" two".into()),
            ]
        );
    }

    #[test]
    fn lf_lines() {
        assert_eq!(
            collect(&[b"first\nsecond\n"]),
            vec![
                RemoteLine::Line("first".into()),
                RemoteLine::Line("second".into()),
            ]
        );
    }

    #[test]
    fn split_across_chunks() {
        assert_eq!(
            collect(&[b"hel", b"lo\rwor", b"ld\n"]),
            vec![
                RemoteLine::Update("hello".into()),
                RemoteLine::Line("world".into()),
            ]
        );
    }

    #[test]
    fn invalid_utf8_lossy() {
        let mut splitter = SidebandLines::new();
        let mut out = Vec::new();
        splitter.feed(b"\xFF\xFEok\r", |line| out.push(line));
        assert_eq!(out, vec![RemoteLine::Update("\u{FFFD}\u{FFFD}ok".into())]);
    }
}
