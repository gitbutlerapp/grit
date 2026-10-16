//! Smart-HTTP [`Connection`] over stateless POST rounds (upload-pack) or a single
//! POST body (receive-pack).
//!
//! Lets [`crate::fetch::fetch_remote`] and [`crate::push::push_remote`] drive
//! smart HTTP without a separate negotiation implementation.

use std::io::{self, Cursor, Read, Write};

use crate::error::{Error, Result};
use crate::objects::ObjectId;
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

/// A live smart-HTTP connection backed by [`HttpClient`] stateless RPC.
pub struct StatelessHttpConnection<C: HttpClient> {
    client: C,
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
    /// Bytes already sent on prior upload-pack POSTs (stateless replay prefix).
    sent_state: Vec<u8>,
    pending: Vec<u8>,
    response: Option<Box<dyn Read + Send>>,
    fail_read: Option<ErrRead>,
    empty_response: Cursor<Vec<u8>>,
    recv_body: Vec<u8>,
    recv_posted: bool,
}

impl<C: HttpClient> StatelessHttpConnection<C> {
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
            client,
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
            sent_state: Vec::new(),
            pending: Vec::new(),
            response: None,
            fail_read: None,
            empty_response: Cursor::new(Vec::new()),
            recv_body: Vec::new(),
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
            let _ = io::copy(&mut r, &mut io::sink());
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
        let body = if self.protocol_version >= 2 {
            // Protocol v2 stateless HTTP sends one command per POST (ls-refs, fetch, …),
            // not a cumulative v0/v1 want/have transcript.
            std::mem::take(&mut self.pending)
        } else {
            let mut body = self.sent_state.clone();
            body.extend_from_slice(&self.pending);
            self.sent_state.extend_from_slice(&self.pending);
            self.pending.clear();
            body
        };
        self.response = Some(self.request(&body)?);
        Ok(())
    }

    fn post_receive_pack(&mut self) -> Result<()> {
        if self.recv_posted {
            return Ok(());
        }
        self.response = Some(
            self.client
                .post_into_reader(
                    &self.post_url,
                    &self.content_type,
                    &self.accept,
                    &self.recv_body,
                    None,
                )
                .inspect_err(|e| {
                    self.set_transport_err(e);
                })?,
        );
        self.recv_posted = true;
        Ok(())
    }

    fn ensure_response_for_read(&mut self) -> Result<()> {
        if self.service == Service::ReceivePack && !self.recv_posted {
            self.post_receive_pack()?;
        }
        Ok(())
    }
}

impl<C: HttpClient> RpcChannel for StatelessHttpConnection<C> {
    fn is_stateless(&self) -> bool {
        true
    }

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

impl<C: HttpClient> Write for StatelessHttpConnection<C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.service {
            Service::UploadPack => {
                self.pending.extend_from_slice(buf);
                self.maybe_post_on_flush()
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
            Service::ReceivePack => self.recv_body.extend_from_slice(buf),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.service == Service::UploadPack {
            self.maybe_post_on_flush()
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        Ok(())
    }
}

impl<C: HttpClient> Connection for StatelessHttpConnection<C> {
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
            if let Err(e) = self.post_receive_pack() {
                self.set_transport_err(&e);
            }
        }
    }
}
