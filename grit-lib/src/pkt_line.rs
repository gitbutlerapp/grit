//! Git pkt-line format helpers.
//!
//! Implements reading and writing of the pkt-line framing used in the
//! Git wire protocol (both v0 and v2).

use std::io::{self, Read, Write};

use crate::error::{Error, Result};
use crate::remote::RemoteError;

/// Special packet type: flush packet (`0000`).
pub const FLUSH: &str = "0000";
/// Special packet type: delimiter packet (`0001`).
pub const DELIM: &str = "0001";
/// Special packet type: response-end packet (`0002`).
#[allow(dead_code)]
pub(crate) const RESPONSE_END: &str = "0002";

/// Write a single pkt-line to `w`. The data should not include a trailing
/// newline; one is appended automatically.
pub(crate) fn write_line(w: &mut impl Write, data: &str) -> io::Result<()> {
    let len = 4 + data.len() + 1;
    writeln!(w, "{len:04x}{data}")
}

/// Append a pkt-line encoding of `data` (with trailing newline) to `buf`.
pub fn write_line_to_vec(buf: &mut Vec<u8>, data: &str) -> io::Result<()> {
    let len = 4 + data.len() + 1;
    let line = format!("{len:04x}{data}\n");
    buf.extend_from_slice(line.as_bytes());
    Ok(())
}

/// Write a flush packet (`0000`).
pub fn write_flush(w: &mut impl Write) -> io::Result<()> {
    write!(w, "0000")
}

/// Write one pkt-line with arbitrary bytes (no trailing newline added).
pub(crate) fn write_packet_raw(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let total = payload
        .len()
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pkt-line payload too large"))?;
    if total > 65520 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pkt-line exceeds maximum size",
        ));
    }
    write!(w, "{total:04x}")?;
    w.write_all(payload)?;
    Ok(())
}

/// Write a delimiter packet (`0001`).
pub(crate) fn write_delim(w: &mut impl Write) -> io::Result<()> {
    write!(w, "0001")
}

/// A single packet read from the wire.
#[derive(Debug, PartialEq, Eq)]
pub enum Packet {
    /// A data line (content without trailing newline).
    Data(String),
    /// Flush packet (`0000`).
    Flush,
    /// Delimiter packet (`0001`).
    Delim,
    /// Response-end packet (`0002`).
    ResponseEnd,
}

/// Read one pkt-line from `r`. Returns `None` at EOF.
pub fn read_packet(r: &mut impl Read) -> io::Result<Option<Packet>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len_str =
        std::str::from_utf8(&len_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    match len {
        0 => Ok(Some(Packet::Flush)),
        1 => Ok(Some(Packet::Delim)),
        2 => Ok(Some(Packet::ResponseEnd)),
        n if n <= 4 => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid pkt-line length: {n}"),
        )),
        n => {
            let payload_len = n - 4;
            let mut buf = vec![0u8; payload_len];
            r.read_exact(&mut buf)?;
            // Smart HTTP / CGI paths may emit non-UTF-8 in diagnostic pkt-lines; Git tolerates
            // lossy decoding. NUL remains valid and is preserved.
            let s = String::from_utf8_lossy(&buf).into_owned();
            Ok(Some(Packet::Data(
                s.strip_suffix('\n').unwrap_or(&s).to_owned(),
            )))
        }
    }
}

/// Read packets from `r` until a flush or delimiter packet, or EOF.
///
/// Returns the collected data lines and the terminator (`Flush`, `Delim`, or `None`).
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn read_until_flush_or_delim(
    r: &mut impl Read,
) -> io::Result<(Vec<String>, Option<Packet>)> {
    let mut lines = Vec::new();
    loop {
        match read_packet(r)? {
            None => return Ok((lines, None)),
            Some(Packet::Flush) => return Ok((lines, Some(Packet::Flush))),
            Some(Packet::Delim) => return Ok((lines, Some(Packet::Delim))),
            Some(Packet::ResponseEnd) => return Ok((lines, Some(Packet::ResponseEnd))),
            Some(Packet::Data(s)) => lines.push(s),
        }
    }
}

/// Write one sideband pkt-line on `band`.
pub(crate) fn write_sideband_packet(
    w: &mut (impl Write + ?Sized),
    band: u8,
    payload: &[u8],
) -> io::Result<()> {
    let len = 4 + 1 + payload.len();
    write!(w, "{len:04x}")?;
    w.write_all(&[band])?;
    w.write_all(payload)?;
    Ok(())
}

/// Write payload on sideband channel 1 using the same 64k chunking as `git upload-pack`.
pub(crate) fn write_sideband_channel1_64k(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    const MAX_PAYLOAD: usize = 65515;
    for chunk in payload.chunks(MAX_PAYLOAD) {
        let len = 4 + 1 + chunk.len();
        write!(w, "{len:04x}")?;
        w.write_all(&[1u8])?;
        w.write_all(chunk)?;
    }
    Ok(())
}

/// Incrementally frame pack bytes on sideband channel 1 (64k pkt-lines).
pub struct SidebandChannel1Writer<'a, W: Write + ?Sized + 'a> {
    inner: &'a mut W,
    pending: Vec<u8>,
}

impl<'a, W: Write + ?Sized + 'a> SidebandChannel1Writer<'a, W> {
    /// Wrap `inner` as a [`Write`] target that emits sideband-64k frames.
    pub fn new(inner: &'a mut W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }

    /// Flush any buffered band-1 bytes.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            write_sideband_packet(self.inner, 1, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

impl<'a, W: Write + ?Sized + 'a> Write for SidebandChannel1Writer<'a, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        const MAX_PAYLOAD: usize = 65515;
        let mut offset = 0usize;
        // Drain any carry-over first without duplicating the new slice.
        if !self.pending.is_empty() {
            let need = MAX_PAYLOAD.saturating_sub(self.pending.len());
            if need > 0 && offset < buf.len() {
                let take = need.min(buf.len());
                self.pending.extend_from_slice(&buf[..take]);
                offset += take;
            }
            while self.pending.len() >= MAX_PAYLOAD {
                let chunk: Vec<u8> = self.pending.drain(..MAX_PAYLOAD).collect();
                write_sideband_packet(self.inner, 1, &chunk)?;
            }
        }
        while offset + MAX_PAYLOAD <= buf.len() {
            write_sideband_packet(self.inner, 1, &buf[offset..offset + MAX_PAYLOAD])?;
            offset += MAX_PAYLOAD;
        }
        if offset < buf.len() {
            self.pending.extend_from_slice(&buf[offset..]);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Parse a 4-byte hexadecimal pkt-line length prefix.
pub(crate) fn parse_hex_len(prefix: &[u8]) -> io::Result<usize> {
    let s = std::str::from_utf8(prefix)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
    usize::from_str_radix(s, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid length: {e}")))
}

/// Fail with [`RemoteError::Fatal`] when `line` is an `ERR <message>` data line.
///
/// A server may send `ERR` in place of any line it would otherwise send; call this on
/// each data line before interpreting it.
///
/// # Errors
///
/// [`RemoteError::Fatal`] carrying the message, trailing whitespace trimmed.
pub(crate) fn reject_err_line(line: &str) -> Result<()> {
    match line.strip_prefix("ERR ") {
        Some(message) => Err(RemoteError::Fatal {
            message: message.trim_end().to_owned(),
        }
        .into()),
        None => Ok(()),
    }
}

/// [`RemoteError::Fatal`] for the payload of a side-band channel-3 packet (band byte removed).
pub(crate) fn sideband_fatal(payload: &[u8]) -> Error {
    RemoteError::Fatal {
        message: String::from_utf8_lossy(payload).trim_end().to_owned(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fatal_message(err: Error) -> String {
        match err {
            Error::Remote(RemoteError::Fatal { message }) => message,
            other => panic!("expected RemoteFatal, got {other:?}"),
        }
    }

    #[test]
    fn err_line_is_a_remote_fatal_error() {
        let err = reject_err_line("ERR upload-pack: not our ref deadbeef\n").unwrap_err();
        assert_eq!(err.kind_name(), "remote_fatal");
        assert_eq!(
            err.to_string(),
            "remote error: upload-pack: not our ref deadbeef"
        );
        assert_eq!(fatal_message(err), "upload-pack: not our ref deadbeef");
    }

    #[test]
    fn other_lines_pass_through() {
        for line in [
            "NAK",
            "ACK 0123",
            "ERRATA",
            "ERR",
            " ERR x",
            "err lower",
            "",
        ] {
            assert!(reject_err_line(line).is_ok(), "{line:?}");
        }
    }

    #[test]
    fn sideband_fatal_trims_and_decodes_lossily() {
        let err = sideband_fatal(b"pack-objects died \xff\n");
        assert_eq!(fatal_message(err), "pack-objects died \u{fffd}");
    }

    #[test]
    fn sideband_packet_and_flush_roundtrip() {
        use std::io::Cursor;

        let mut encoded = Vec::new();
        write_sideband_packet(&mut encoded, 1, b"payload").expect("write sb");
        assert_eq!(encoded, b"000c\x01payload");

        let mut pkt = Vec::new();
        write_sideband_packet(&mut pkt, 1, b"line").expect("pkt");
        pkt.extend_from_slice(b"0000");
        let mut cursor = Cursor::new(pkt);
        let (lines, term) = read_until_flush_or_delim(&mut cursor).expect("read");
        assert!(!lines.is_empty());
        assert!(term.is_some());

        let mut flush_pkt = Vec::new();
        write_line_to_vec(&mut flush_pkt, "ok").expect("line");
        write_flush(&mut flush_pkt).expect("flush");
        let mut c2 = Cursor::new(flush_pkt);
        let (lines2, term2) = read_until_flush_or_delim(&mut c2).expect("flush read");
        assert_eq!(lines2, vec!["ok".to_owned()]);
        assert!(term2.is_some());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "uses system git; run with --features full-tests"
    )]
    #[test]
    fn system_git_upload_pack_accepts_grit_written_pkt_lines() {
        use crate::objects::ObjectId;
        use crate::promisor_remote::parse_advertisement;
        use crate::refs::store::paths::{logs_refs_dir_path, logs_refs_subdir_path};
        use crate::refs::{ref_update_flags, verify_branch_commit_cas};
        use crate::repo::{InitOptions, Repository};
        use std::io::{Cursor, Write};
        use std::path::Path;
        use std::process::{Command, Stdio};

        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("SKIP: no system git");
            return;
        }

        fn hermetic_git(cmd: &mut Command) {
            cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
        }

        fn git_head_hex(dir: &Path) -> String {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir).args(["rev-parse", "HEAD"]);
            hermetic_git(&mut cmd);
            let out = cmd.output().expect("git rev-parse");
            assert!(
                out.status.success(),
                "git rev-parse: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        std::fs::write(dir.path().join("pkt.txt"), b"peer\n").expect("write");
        {
            let mut add = Command::new("git");
            add.current_dir(dir.path()).args(["add", "pkt.txt"]);
            hermetic_git(&mut add);
            let out = add.output().expect("git add");
            assert!(
                out.status.success(),
                "git add: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        {
            let mut commit = Command::new("git");
            commit
                .current_dir(dir.path())
                .args(["commit", "-qm", "pkt-line peer"]);
            commit
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@e.com")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@e.com");
            hermetic_git(&mut commit);
            let out = commit.output().expect("git commit");
            assert!(
                out.status.success(),
                "git commit: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let head_hex = git_head_hex(dir.path());

        let git_advert = {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir.path())
                .args(["upload-pack", "--advertise-refs", "."]);
            hermetic_git(&mut cmd);
            cmd.output().expect("git upload-pack --advertise-refs")
        };
        assert!(
            git_advert.status.success(),
            "git upload-pack --advertise-refs: {}",
            String::from_utf8_lossy(&git_advert.stderr)
        );
        assert!(
            !git_advert.stdout.is_empty(),
            "git must emit pkt-line ref advertisement"
        );

        let mut advert = Cursor::new(git_advert.stdout.as_slice());
        let (git_ref_lines, git_term) =
            read_until_flush_or_delim(&mut advert).expect("read git ref pkt-lines");
        assert_eq!(git_term, Some(Packet::Flush));
        assert!(
            git_ref_lines
                .iter()
                .any(|line| line.contains(&head_hex) && line.contains("HEAD")),
            "git ref pkt-lines must advertise HEAD at {head_hex}: {git_ref_lines:?}"
        );

        let mut fetch_request = Vec::new();
        write_line_to_vec(
            &mut fetch_request,
            &format!("want {head_hex} multi_ack side-band-64k"),
        )
        .expect("want line");
        write_flush(&mut fetch_request).expect("request flush");
        write_line_to_vec(&mut fetch_request, "done").expect("done");
        write_flush(&mut fetch_request).expect("done flush");

        let mut upload = Command::new("git");
        upload
            .current_dir(dir.path())
            .args(["upload-pack", "--stateless-rpc", "."])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hermetic_git(&mut upload);
        let mut child = upload.spawn().expect("spawn git upload-pack");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(&fetch_request)
            .expect("write fetch pkt-lines");
        let pack_response = child.wait_with_output().expect("wait upload-pack");
        assert!(
            pack_response.status.success(),
            "git upload-pack must accept grit-written pkt-lines: {}",
            String::from_utf8_lossy(&pack_response.stderr)
        );
        assert!(
            pack_response.stdout.len() > 20,
            "git must respond with a non-trivial pack pkt-line stream"
        );
        let mut pack_reader = Cursor::new(pack_response.stdout.as_slice());
        let (_pack_lines, pack_term) =
            read_until_flush_or_delim(&mut pack_reader).expect("read git pack section");
        assert!(
            pack_term.is_some(),
            "git pack response must terminate a pkt-line section"
        );

        assert!(
            parse_advertisement("version https://git-scm.com/docs/pack-protocol/2\n").is_empty()
        );

        let _ = ref_update_flags("refs/heads/main");
        let oid = ObjectId::from_bytes(&[1u8; 20]).expect("oid");
        verify_branch_commit_cas(Some(oid), Some(oid), "refs/heads/main").expect("cas");

        let git_dir = tempfile::tempdir().expect("t").path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir");
        let _ = logs_refs_dir_path(&git_dir);
        let _ = logs_refs_subdir_path(&git_dir, "heads");
    }
}
