//! Smart-HTTP [`Connection`] over stateless POST rounds (upload-pack) or a single
//! POST body (receive-pack).
//!
//! Lets [`crate::fetch::fetch_remote`] and [`crate::push::push_remote`] drive
//! smart HTTP without a separate negotiation implementation.

use std::io::{self, Cursor, Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::progress::{Cancel, NeverCancel};
use crate::transport::http::HttpClient;
use crate::transport::rpc_channel::RpcChannel;
use crate::transport::{Connection, Service};

/// Reader that surfaces a stored I/O error on the first read (instead of EOF).
struct ErrRead {
    err: io::Error,
}

impl Read for ErrRead {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(std::mem::replace(
            &mut self.err,
            io::Error::other("transport error already reported"),
        ))
    }
}

/// Receive-pack bodies up to this size are sent in one buffered request; larger
/// ones (a pack, in practice) stream with chunked encoding. Git's default
/// `http.postBuffer`.
const LARGE_POST: usize = 1024 * 1024;

/// Bytes handed to the upload thread at a time.
const STREAM_CHUNK: usize = 256 * 1024;

/// Chunks queued for the upload thread before the writer waits for the network.
const STREAM_QUEUE: usize = 4;

/// A piece of a streamed request body.
enum BodyChunk {
    Data(Vec<u8>),
    End,
}

/// The body of a POST still being produced, read by the request as it is sent.
struct ChunkReader {
    chunks: Receiver<BodyChunk>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.chunk.len() {
            match self.chunks.recv() {
                Ok(BodyChunk::Data(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Ok(BodyChunk::End) => return Ok(0),
                // The writer went away mid-body: fail the request rather than
                // let a truncated body look complete.
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "request body abandoned",
                    ))
                }
            }
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// How often a wait for the receive-pack request checks for a cancel.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// A receive-pack response read to the end, replaying the error that ended it
/// (a reset after the report, say) once its bytes are consumed.
struct ReadResponse {
    bytes: Cursor<Vec<u8>>,
    error: Option<io::Error>,
}

impl ReadResponse {
    fn read_from(mut reader: Box<dyn Read + Send>) -> Self {
        let mut bytes = Vec::new();
        let error = reader.read_to_end(&mut bytes).err();
        Self {
            bytes: Cursor::new(bytes),
            error,
        }
    }
}

impl Read for ReadResponse {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.bytes.read(buf)? {
            0 if !buf.is_empty() => self.error.take().map_or(Ok(0), Err),
            n => Ok(n),
        }
    }
}

/// A receive-pack request running on its own thread, which also reads the
/// (small) report so waiting for it can be interrupted.
struct PendingResponse(Receiver<Result<ReadResponse>>);

impl PendingResponse {
    fn spawn<F>(request: F) -> io::Result<Self>
    where
        F: FnOnce() -> Result<Box<dyn Read + Send>> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("grit-http-post".to_owned())
            .spawn(move || {
                let _ = tx.send(request().map(ReadResponse::read_from));
            })?;
        Ok(Self(rx))
    }

    /// The response, or [`Error::Cancelled`] once `cancel` fires first. A cancelled
    /// request is left to finish on its own thread.
    fn wait(&self, cancel: &dyn Cancel) -> Result<ReadResponse> {
        loop {
            match self.0.recv_timeout(CANCEL_POLL) {
                Ok(response) => return response,
                Err(RecvTimeoutError::Timeout) if cancel.is_cancelled() => {
                    return Err(Error::Cancelled)
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Message("HTTP upload thread panicked".to_owned()))
                }
            }
        }
    }
}

/// A receive-pack POST running on its own thread while the body is written.
struct StreamingPost {
    chunks: SyncSender<BodyChunk>,
    response: PendingResponse,
}

impl StreamingPost {
    fn start<C: HttpClient + 'static>(
        client: Arc<C>,
        url: String,
        content_type: String,
        accept: String,
    ) -> io::Result<Self> {
        let (chunks, rx) = std::sync::mpsc::sync_channel(STREAM_QUEUE);
        let response = PendingResponse::spawn(move || {
            let mut body = ChunkReader {
                chunks: rx,
                chunk: Vec::new(),
                pos: 0,
            };
            client.post_streaming(&url, &content_type, &accept, &mut body, None)
        })?;
        Ok(Self { chunks, response })
    }

    /// Queue `chunk`; when the request already ended, its error.
    fn send(self, chunk: Vec<u8>) -> std::result::Result<Self, Error> {
        match self.chunks.send(BodyChunk::Data(chunk)) {
            Ok(()) => Ok(self),
            Err(_) => Err(self.ended_early()),
        }
    }

    /// Queue `item` while the upload thread makes room, unless `cancel` fires first.
    fn send_until(&self, mut item: BodyChunk, cancel: &dyn Cancel) -> Result<()> {
        loop {
            match self.chunks.try_send(item) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(back)) => {
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    item = back;
                    std::thread::sleep(CANCEL_POLL);
                }
                Err(TrySendError::Disconnected(_)) => return Err(self.ended_early()),
            }
        }
    }

    /// Why a request whose body could not be queued ended.
    fn ended_early(&self) -> Error {
        match self.response.wait(&NeverCancel) {
            Err(e) => e,
            Ok(_) => Error::Message("server answered before the request body was sent".into()),
        }
    }

    /// Queue the last `chunk`, end the body and wait for the response. When
    /// `cancel` fires before the body ended, dropping `self` abandons it.
    fn finish(self, chunk: Vec<u8>, cancel: &dyn Cancel) -> Result<ReadResponse> {
        self.send_until(BodyChunk::Data(chunk), cancel)?;
        self.send_until(BodyChunk::End, cancel)?;
        self.response.wait(cancel)
    }
}

/// A live smart-HTTP connection backed by [`HttpClient`] stateless RPC.
///
/// A receive-pack request body larger than Git's default `http.postBuffer`
/// (1 MiB) is not held in memory: after a small probe request settles
/// authentication, it streams to the server through
/// [`HttpClient::post_streaming`] while the caller is still writing it.
pub struct StatelessHttpConnection<C: HttpClient> {
    client: Arc<C>,
    service: Service,
    repo_url: String,
    post_url: String,
    content_type: String,
    accept: String,
    git_protocol: Option<String>,
    adv_refs: Vec<(String, ObjectId)>,
    caps: Vec<String>,
    head_symref: Option<String>,
    protocol_version: u8,
    object_format: String,
    pending: Vec<u8>,
    response: Option<Box<dyn Read + Send>>,
    fail_read: Option<ErrRead>,
    empty_response: Cursor<Vec<u8>>,
    recv_body: Vec<u8>,
    recv_stream: Option<StreamingPost>,
    recv_posted: bool,
}

impl<C: HttpClient + 'static> StatelessHttpConnection<C> {
    /// Build a connection after `info/refs` discovery.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: C,
        service: Service,
        repo_url: String,
        protocol_version: u8,
        adv_refs: Vec<(String, ObjectId)>,
        caps: Vec<String>,
        head_symref: Option<String>,
        object_format: String,
        git_protocol: Option<String>,
    ) -> Self {
        let wire = service.wire_name();
        let base = repo_url.trim_end_matches('/');
        let post_url = format!("{base}/{wire}");
        let content_type = format!("application/x-{wire}-request");
        let accept = format!("application/x-{wire}-result");
        Self {
            client: Arc::new(client),
            service,
            repo_url,
            post_url,
            content_type,
            accept,
            git_protocol,
            adv_refs,
            caps,
            head_symref,
            protocol_version,
            object_format,
            pending: Vec::new(),
            response: None,
            fail_read: None,
            empty_response: Cursor::new(Vec::new()),
            recv_body: Vec::new(),
            recv_stream: None,
            recv_posted: false,
        }
    }

    /// Repository URL used for subsequent POSTs.
    #[must_use]
    pub fn repo_url(&self) -> &str {
        &self.repo_url
    }

    /// The server's advertised object format (`sha1` or `sha256`).
    #[must_use]
    pub fn object_format(&self) -> &str {
        &self.object_format
    }

    fn drain_response(&mut self) {
        if let Some(mut r) = self.response.take() {
            let _ = crate::pack_receive::copy_reader_to_writer(&mut r, &mut io::sink());
        }
    }

    fn set_transport_err(&mut self, err: &Error) {
        self.fail_read = Some(ErrRead {
            err: io::Error::other(err.to_string()),
        });
        self.response = None;
    }

    fn upload_pack_post_due(&self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        // Stateless RPC posts on section flushes and on the final `done` pkt-line
        // (there is no trailing `0000` after `done` before reading the ACK/pack).
        self.pending.ends_with(b"0000") || self.pending.ends_with(b"done\n")
    }

    fn maybe_post_on_flush(&mut self) -> Result<()> {
        if self.service != Service::UploadPack {
            return Ok(());
        }
        if self.upload_pack_post_due() {
            self.post_upload_pack_round()?;
        }
        Ok(())
    }

    fn post_upload_pack_round(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // Each POST carries a complete request: a v2 command, or a v0/v1 want
        // block plus haves that the negotiation restates itself, as Git's
        // stateless upload-pack stops reading at the first flush after the haves.
        let body = std::mem::take(&mut self.pending);
        self.response = Some(self.request(&body)?);
        Ok(())
    }

    /// Send the receive-pack request (or the rest of its streamed body) and wait
    /// for the report, giving up once `cancel` fires.
    fn post_receive_pack(&mut self, cancel: &dyn Cancel) -> Result<()> {
        if self.recv_posted {
            return Ok(());
        }
        self.recv_posted = true;
        let body = std::mem::take(&mut self.recv_body);
        let response = match self.recv_stream.take() {
            Some(stream) => stream.finish(body, cancel),
            None => {
                let client = Arc::clone(&self.client);
                let (url, content_type, accept) = (
                    self.post_url.clone(),
                    self.content_type.clone(),
                    self.accept.clone(),
                );
                PendingResponse::spawn(move || {
                    client.post_into_reader(&url, &content_type, &accept, &body, None)
                })
                .map_err(Error::Io)
                .and_then(|pending| pending.wait(cancel))
            }
        };
        let response = response.inspect_err(|e| self.set_transport_err(e))?;
        self.response = Some(Box::new(response));
        Ok(())
    }

    /// Buffer receive-pack body bytes, streaming them once the body outgrows
    /// [`LARGE_POST`].
    fn write_receive_pack(&mut self, buf: &[u8]) -> Result<()> {
        if self.recv_posted {
            return Err(Error::Message(
                "receive-pack request already sent".to_owned(),
            ));
        }
        self.recv_body.extend_from_slice(buf);
        if self.recv_stream.is_none() {
            if self.recv_body.len() <= LARGE_POST {
                return Ok(());
            }
            self.probe_receive_pack()?;
            self.recv_stream = Some(StreamingPost::start(
                Arc::clone(&self.client),
                self.post_url.clone(),
                self.content_type.clone(),
                self.accept.clone(),
            )?);
        }
        if self.recv_body.len() < STREAM_CHUNK {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.recv_body, Vec::with_capacity(STREAM_CHUNK));
        if let Some(stream) = self.recv_stream.take() {
            match stream.send(chunk) {
                Ok(stream) => self.recv_stream = Some(stream),
                Err(e) => {
                    self.recv_posted = true;
                    self.set_transport_err(&e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Post an empty command list before a streamed body, as Git does, so an
    /// authentication challenge is answered on a request that can be resent.
    fn probe_receive_pack(&mut self) -> Result<()> {
        let mut reply = self.client.post_into_reader(
            &self.post_url,
            &self.content_type,
            &self.accept,
            b"0000",
            None,
        )?;
        io::copy(&mut reply, &mut io::sink()).map_err(Error::Io)?;
        Ok(())
    }

    fn ensure_response_for_read(&mut self) -> Result<()> {
        if self.service == Service::ReceivePack && !self.recv_posted {
            self.post_receive_pack(&NeverCancel)?;
        }
        Ok(())
    }
}

impl<C: HttpClient + 'static> RpcChannel for StatelessHttpConnection<C> {
    fn request(&mut self, body: &[u8]) -> Result<Box<dyn Read + Send>> {
        self.drain_response();
        self.client
            .post_into_reader(
                &self.post_url,
                &self.content_type,
                &self.accept,
                body,
                self.git_protocol.as_deref(),
            )
            .inspect_err(|e| {
                self.set_transport_err(e);
            })
    }
}

impl<C: HttpClient + 'static> Write for StatelessHttpConnection<C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.service {
            Service::UploadPack => {
                self.pending.extend_from_slice(buf);
                self.maybe_post_on_flush()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                Ok(buf.len())
            }
            Service::ReceivePack => {
                // A large object arrives in one write; taking a chunk at a time keeps
                // the buffered body bounded.
                let taken = &buf[..buf.len().min(STREAM_CHUNK)];
                self.write_receive_pack(taken)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                Ok(taken.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.service == Service::UploadPack {
            self.maybe_post_on_flush()
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        Ok(())
    }
}

impl<C: HttpClient + 'static> Connection for StatelessHttpConnection<C> {
    fn reader(&mut self) -> &mut dyn Read {
        if self.fail_read.is_none() {
            if let Err(e) = self.ensure_response_for_read() {
                self.fail_read = Some(ErrRead {
                    err: io::Error::other(e.to_string()),
                });
            }
        }
        if let Some(r) = self.fail_read.as_mut() {
            return r;
        }
        match self.response.as_mut() {
            Some(r) => r.as_mut(),
            None => &mut self.empty_response,
        }
    }

    fn writer(&mut self) -> &mut dyn Write {
        self
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.adv_refs
    }

    fn capabilities(&self) -> &[String] {
        &self.caps
    }

    fn head_symref(&self) -> Option<&str> {
        self.head_symref.as_deref()
    }

    fn protocol_version(&self) -> u8 {
        self.protocol_version
    }

    fn stateless_rpc(&self) -> bool {
        true
    }

    fn finish_send(&mut self) {
        if self.service == Service::ReceivePack && !self.recv_posted {
            if let Err(e) = self.post_receive_pack(&NeverCancel) {
                self.set_transport_err(&e);
            }
        }
    }

    fn wait_for_response(&mut self, cancel: &dyn Cancel) -> Result<()> {
        if self.service == Service::ReceivePack {
            self.post_receive_pack(cancel)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    /// What the fake server saw.
    #[derive(Default)]
    struct Seen {
        /// Bodies of buffered POSTs, in order.
        buffered: Vec<Vec<u8>>,
        /// The streamed body, once read to the end.
        streamed: Option<Vec<u8>>,
        /// Largest single read of the streamed body.
        largest_read: usize,
    }

    /// A receive-pack endpoint that reads streamed bodies like a server would.
    struct FakeServer {
        seen: Arc<Mutex<Seen>>,
        /// Fail a streamed POST before reading its body.
        refuse_stream: bool,
        /// Where a streamed POST reports how reading its body ended.
        body_end: Option<Mutex<mpsc::Sender<io::Result<usize>>>>,
    }

    impl FakeServer {
        fn new() -> (Self, Arc<Mutex<Seen>>) {
            let seen = Arc::new(Mutex::new(Seen::default()));
            let server = Self {
                seen: Arc::clone(&seen),
                refuse_stream: false,
                body_end: None,
            };
            (server, seen)
        }
    }

    impl HttpClient for FakeServer {
        fn get(&self, _: &str, _: Option<&str>) -> Result<Vec<u8>> {
            Err(Error::Message("no GETs here".to_owned()))
        }

        fn post(&self, _: &str, _: &str, _: &str, body: &[u8], _: Option<&str>) -> Result<Vec<u8>> {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .buffered
                .push(body.to_vec());
            Ok(b"report".to_vec())
        }

        fn post_streaming(
            &self,
            _: &str,
            _: &str,
            _: &str,
            body: &mut dyn Read,
            _: Option<&str>,
        ) -> Result<Box<dyn Read + Send>> {
            if self.refuse_stream {
                return Err(Error::Message("HTTP 500 from the fake server".to_owned()));
            }
            let mut got = Vec::new();
            let mut largest = 0;
            let mut buf = vec![0u8; 1 << 20];
            let end = loop {
                match body.read(&mut buf) {
                    Ok(0) => break Ok(got.len()),
                    Ok(n) => {
                        largest = largest.max(n);
                        got.extend_from_slice(&buf[..n]);
                    }
                    Err(e) => break Err(e),
                }
            };
            let failed = end.is_err();
            if let Some(tx) = &self.body_end {
                let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(end);
            }
            if failed {
                return Err(Error::Message("body did not finish".to_owned()));
            }
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.streamed = Some(got);
            seen.largest_read = largest;
            Ok(Box::new(Cursor::new(b"streamed report".to_vec())))
        }
    }

    fn receive_pack(server: FakeServer) -> StatelessHttpConnection<FakeServer> {
        StatelessHttpConnection::new(
            server,
            Service::ReceivePack,
            "http://h/r.git".to_owned(),
            0,
            Vec::new(),
            Vec::new(),
            None,
            "sha1".to_owned(),
            None,
        )
    }

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn read_reply(conn: &mut StatelessHttpConnection<FakeServer>) -> io::Result<Vec<u8>> {
        let mut reply = Vec::new();
        conn.reader().read_to_end(&mut reply)?;
        Ok(reply)
    }

    #[test]
    fn v0_upload_pack_posts_each_request_as_written() {
        let (server, seen) = FakeServer::new();
        let mut conn = StatelessHttpConnection::new(
            server,
            Service::UploadPack,
            "http://h/r.git".to_owned(),
            0,
            Vec::new(),
            Vec::new(),
            None,
            "sha1".to_owned(),
            None,
        );
        let wants = b"0032want 0123456789012345678901234567890123456789\n0000".to_vec();
        let have = b"0032have 1123456789012345678901234567890123456789\n";
        let mut round = wants.clone();
        round.extend_from_slice(have);
        round.extend_from_slice(b"0000");
        let mut last = wants.clone();
        last.extend_from_slice(b"0009done\n");
        for body in [&wants, &round, &last] {
            conn.writer().write_all(body).unwrap();
            assert_eq!(read_reply(&mut conn).unwrap(), b"report");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.buffered, vec![wants, round, last]);
    }

    #[test]
    fn small_receive_pack_bodies_are_posted_once_whole() {
        let (server, seen) = FakeServer::new();
        let mut conn = receive_pack(server);
        let data = body(4096);
        conn.writer().write_all(&data).unwrap();
        assert_eq!(read_reply(&mut conn).unwrap(), b"report");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.buffered, vec![data]);
        assert!(seen.streamed.is_none());
    }

    #[test]
    fn large_receive_pack_bodies_stream_after_a_probe_without_being_held() {
        let (server, seen) = FakeServer::new();
        let mut conn = receive_pack(server);
        let data = body(6 * LARGE_POST + 12345);
        for piece in data.chunks(10_000) {
            conn.writer().write_all(piece).unwrap();
            assert!(
                conn.recv_body.len() <= LARGE_POST + 10_000,
                "the connection holds at most one post buffer"
            );
        }
        assert!(conn.recv_stream.is_some());
        assert_eq!(read_reply(&mut conn).unwrap(), b"streamed report");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.buffered,
            vec![b"0000".to_vec()],
            "only the probe is buffered"
        );
        assert_eq!(seen.streamed.as_deref(), Some(&data[..]));
        assert!(seen.largest_read <= LARGE_POST + 10_000);
    }

    #[test]
    fn one_huge_write_is_streamed_a_chunk_at_a_time() {
        let (server, seen) = FakeServer::new();
        let mut conn = receive_pack(server);
        let data = body(8 * LARGE_POST);
        let first = conn.writer().write(&data).unwrap();
        assert!(first <= STREAM_CHUNK, "took {first} bytes in one write");
        conn.writer().write_all(&data[first..]).unwrap();
        assert!(conn.recv_body.len() <= STREAM_CHUNK);
        assert_eq!(read_reply(&mut conn).unwrap(), b"streamed report");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.streamed.as_deref(), Some(&data[..]));
        assert!(seen.largest_read <= LARGE_POST + STREAM_CHUNK);
    }

    #[test]
    fn a_refused_stream_fails_the_write_with_the_server_error() {
        let (mut server, _) = FakeServer::new();
        server.refuse_stream = true;
        let mut conn = receive_pack(server);
        let data = body(LARGE_POST);
        let mut failure = None;
        for _ in 0..(STREAM_QUEUE + 8) {
            if let Err(e) = conn.writer().write_all(&data) {
                failure = Some(e);
                break;
            }
        }
        let failure = failure.expect("writes fail once the request is refused");
        assert!(failure.to_string().contains("HTTP 500"), "{failure}");
        let after = read_reply(&mut conn).unwrap_err();
        assert!(after.to_string().contains("HTTP 500"), "{after}");
    }

    #[test]
    fn dropping_a_connection_mid_body_fails_the_request_instead_of_ending_it() {
        let (mut server, _) = FakeServer::new();
        let (tx, rx) = mpsc::channel();
        server.body_end = Some(Mutex::new(tx));
        let mut conn = receive_pack(server);
        conn.writer().write_all(&body(2 * LARGE_POST)).unwrap();
        drop(conn);
        let end = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the request thread reports how the body ended");
        let err = end.expect_err("an abandoned body must not look complete");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
