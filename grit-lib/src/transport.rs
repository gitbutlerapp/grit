//! Embedder-facing transport abstraction for the Git wire protocols.
//!
//! This module defines a small, embedder-shaped surface over the bidirectional
//! pkt-line channel that every Git transport (git://, ssh, http) exposes:
//!
//! * [`Transport`] — a factory that, given a URL, a [`Service`] and
//!   [`ConnectOptions`], performs the protocol handshake and returns a live
//!   [`Connection`].
//! * [`Connection`] — the duplex pkt-line stream plus the ref/capability
//!   advertisement captured on connect. The negotiation engine in
//!   [`crate::fetch`] drives `want`/`have`/`done` over the connection's reader
//!   and writer; it never assumes a subprocess or global config.
//!
//! Phase 1 ships [`GitDaemonTransport`] (the native `git://` daemon protocol),
//! lifted from the CLI's `git_daemon_url` connector. `ssh` and `http(s)`
//! transports are later phases and implement the same traits.
//!
//! The advertisement parser is hash-algorithm aware: it reads the leading hex
//! run of each ref line, so SHA-256 (64-hex) advertisements parse the same way
//! SHA-1 (40-hex) ones do.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::pkt_line;

pub mod http;

/// The Git service a [`Connection`] speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// `git-upload-pack` — the server side of a fetch/clone.
    UploadPack,
    /// `git-receive-pack` — the server side of a push.
    ReceivePack,
}

impl Service {
    /// The wire service name (`git-upload-pack` / `git-receive-pack`).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Service::UploadPack => "git-upload-pack",
            Service::ReceivePack => "git-receive-pack",
        }
    }
}

/// Options controlling the transport handshake.
///
/// The default requests protocol version 0 (the classic advertisement) with no
/// server options.
#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    /// Requested protocol version (`0`, `1`, or `2`). The server may downgrade.
    pub protocol_version: u8,
    /// `server-option`s to send (protocol v2 `command` arguments / daemon
    /// extra parameters). Ignored by servers that do not support them.
    pub server_options: Vec<String>,
}

/// A live, bidirectional pkt-line connection to a Git service, with the
/// ref/capability advertisement captured during the handshake.
///
/// The fetch/push engines read [`Connection::reader`] and write
/// [`Connection::writer`]; the advertisement accessors expose what the server
/// announced on connect so the caller can resolve `want`s and pick capabilities
/// without re-reading the stream.
pub trait Connection {
    /// The readable half of the pkt-line stream (server -> client).
    fn reader(&mut self) -> &mut dyn Read;

    /// The writable half of the pkt-line stream (client -> server).
    fn writer(&mut self) -> &mut dyn Write;

    /// The refs the server advertised on connect (excluding `HEAD`, the
    /// `capabilities^{}` carrier, and peeled `^{}` lines). Empty for a protocol
    /// v2 connection, whose refs are obtained later via `ls-refs`.
    fn advertised_refs(&self) -> &[(String, ObjectId)];

    /// The capability tokens advertised by the server (from the first ref line
    /// in v0/v1, or the v2 capability block).
    fn capabilities(&self) -> &[String];

    /// The target of the server's `HEAD` symref (e.g. `refs/heads/main`), if it
    /// advertised one.
    fn head_symref(&self) -> Option<&str>;

    /// The negotiated protocol version (`0`, `1`, or `2`).
    fn protocol_version(&self) -> u8;

    /// Half-close the write side of the stream, signalling end-of-input to the
    /// server (the wire equivalent of the CLI's `drop(stdin)`).
    ///
    /// Protocol v2 servers run a persistent `serve_loop`: after streaming the
    /// pack for one `command=fetch` they block reading the next command. A
    /// streaming transport (ssh subprocess, daemon socket) must therefore close
    /// its write half once the fetch is complete, or the server never exits and
    /// teardown (`child.wait()` / socket close) blocks. The default is a no-op
    /// (v0/v1 connections, where the server closes after the single response).
    fn finish_send(&mut self) {}
}

/// A factory that connects to a remote and performs the protocol handshake.
///
/// Implementations legitimately perform socket / subprocess / HTTP I/O; the
/// trait itself makes no such assumption, so embedders can supply their own.
pub trait Transport {
    /// Connect to `url` for `service`, performing the handshake described by
    /// `opts`, and return a live [`Connection`] positioned just past the
    /// advertisement.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL is malformed for this transport, if the
    /// connection cannot be established, or if the advertisement is malformed.
    fn connect(
        &self,
        url: &str,
        service: Service,
        opts: &ConnectOptions,
    ) -> Result<Box<dyn Connection>>;
}

/// The captured ref/capability advertisement for a v0/v1 connection.
#[derive(Clone, Debug, Default)]
pub struct Advertisement {
    /// Advertised refs (name -> oid), excluding `HEAD` and peeled/`capabilities` carriers.
    pub refs: Vec<(String, ObjectId)>,
    /// Server capability tokens (split on whitespace from the first ref line).
    pub capabilities: Vec<String>,
    /// `HEAD` symref target, if advertised via `symref=HEAD:<target>`.
    pub head_symref: Option<String>,
    /// Negotiated protocol version.
    pub protocol_version: u8,
}

/// Read and parse a v0/v1 (or v2 preamble) ref advertisement from `reader`,
/// stopping at the first flush packet.
///
/// This is the lib-side, hash-width-aware port of the CLI's `read_advertisement`
/// (`grit/src/fetch_transport.rs`). It records the capability list from the
/// first ref line, the `HEAD` symref, and the negotiated protocol version, and
/// skips the `version N`, `capabilities^{}`, and peeled `^{}` carrier lines.
///
/// # Errors
///
/// Returns an error on I/O failure or if the server sends an `ERR` packet.
pub fn read_advertisement(reader: &mut dyn Read) -> Result<Advertisement> {
    let mut adv = Advertisement {
        protocol_version: 0,
        ..Default::default()
    };
    let mut reader = reader;
    let mut first_ref = true;
    // Set once we see a `version 2` line: every subsequent pkt-line up to the
    // flush is a v2 capability (`agent=…`, `ls-refs=…`, `fetch=…`,
    // `object-format=…`, `server-option`, …), not a ref. The caller obtains the
    // refs later via an `ls-refs` command.
    let mut v2 = false;
    loop {
        match pkt_line::read_packet(&mut reader)? {
            None => break,
            Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => break,
            Some(pkt_line::Packet::ResponseEnd) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if let Some(ver) = line.strip_prefix("version ") {
                    if let Ok(n) = ver.trim().parse::<u8>() {
                        adv.protocol_version = n;
                        if n >= 2 {
                            v2 = true;
                        }
                        continue;
                    }
                }
                if v2 {
                    // v2 capability block: collect every line verbatim and leave
                    // `advertised_refs` empty. `ERR` is still fatal.
                    if let Some(msg) = line.strip_prefix("ERR ") {
                        return Err(Error::Message(format!(
                            "remote error: {}",
                            msg.trim_end()
                        )));
                    }
                    adv.capabilities.push(line.to_string());
                    continue;
                }
                if let Some(msg) = line.strip_prefix("ERR ") {
                    return Err(Error::Message(format!(
                        "remote error: {}",
                        msg.trim_end()
                    )));
                }
                let Some((oid, refname, caps)) = parse_ref_advertisement_line(line) else {
                    continue;
                };
                if first_ref {
                    first_ref = false;
                    adv.capabilities = caps
                        .split_whitespace()
                        .map(std::string::ToString::to_string)
                        .collect();
                }
                if refname == "HEAD" {
                    for cap in caps.split_whitespace() {
                        if let Some(target) = cap.strip_prefix("symref=HEAD:") {
                            adv.head_symref = Some(target.to_string());
                        }
                    }
                }
                // The `0{hex} capabilities^{}` no-refs carrier and peeled `^{}` lines
                // are not fetchable refs.
                if refname == "capabilities^{}" || refname.ends_with("^{}") {
                    continue;
                }
                if refname == "HEAD" {
                    continue;
                }
                adv.refs.push((refname, oid));
            }
        }
    }
    Ok(adv)
}

/// Parse one ref-advertisement line: `<oid-hex> <refname>[\0<caps>]`.
///
/// Hash-width aware: the OID is the leading hex run (40 chars for SHA-1, 64 for
/// SHA-256), so SHA-256 advertisements parse correctly. Returns `None` for
/// non-ref lines (e.g. `shallow <oid>`).
fn parse_ref_advertisement_line(line: &str) -> Option<(ObjectId, String, &str)> {
    let line = line.trim_end_matches('\n');
    // The OID is the maximal leading run of hex digits.
    let hex_len = line
        .as_bytes()
        .iter()
        .take_while(|b| b.is_ascii_hexdigit())
        .count();
    if hex_len != 40 && hex_len != 64 {
        return None;
    }
    let hex = &line[..hex_len];
    let oid = ObjectId::from_hex(hex).ok()?;
    let mut rest = line[hex_len..].trim_start();
    // `git-daemon` uses a single space after the OID; `upload-pack` often uses a tab.
    rest = rest.trim_start_matches([' ', '\t']);
    let (refname, caps) = if let Some(i) = rest.find('\0') {
        (rest[..i].trim(), &rest[i + 1..])
    } else {
        (rest.trim(), "")
    };
    if refname.is_empty() {
        return None;
    }
    Some((oid, refname.to_string(), caps))
}

/// Parsed `git://host[:port]/path` (path includes the leading `/`).
#[derive(Clone, Debug)]
pub struct GitDaemonUrl {
    /// Host name or IP literal.
    pub host: String,
    /// TCP port (defaults to 9418).
    pub port: u16,
    /// Repository path on the daemon (with leading `/`).
    pub path: String,
}

/// Parse a `git://host[:port]/path` URL for the native daemon transport.
///
/// Lifted from the CLI's `git_daemon_url::parse_git_url`. Supports bracketed
/// IPv6 literals and defaults the port to 9418.
///
/// # Errors
///
/// Returns an error if the URL is not `git://`, has an empty host, or is missing
/// a repository path.
pub fn parse_git_url(url: &str) -> Result<GitDaemonUrl> {
    let rest = url
        .strip_prefix("git://")
        .ok_or_else(|| Error::Message(format!("not a git:// URL: {url}")))?;
    let (authority, path_part) = rest
        .find('/')
        .map(|i| (&rest[..i], &rest[i..]))
        .unwrap_or((rest, "/"));
    if path_part.is_empty() || path_part == "/" {
        return Err(Error::Message(
            "git:// URL missing repository path".to_owned(),
        ));
    }
    let path = path_part.to_string();
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped
            .find(']')
            .ok_or_else(|| Error::Message(format!("invalid git:// authority: {authority}")))?;
        let host = stripped[..end].to_string();
        let after = &stripped[end + 1..];
        let port = if let Some(p) = after.strip_prefix(':') {
            p.parse::<u16>()
                .map_err(|_| Error::Message(format!("invalid port in git:// URL: {url}")))?
        } else {
            9418
        };
        (host, port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        let h = h.trim_end_matches(':');
        if p.is_empty() {
            (h.to_string(), 9418)
        } else if p.chars().all(|c| c.is_ascii_digit()) {
            (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| Error::Message(format!("invalid port in git:// URL: {url}")))?,
            )
        } else {
            (authority.to_string(), 9418)
        }
    } else {
        (authority.to_string(), 9418)
    };
    if host.is_empty() {
        return Err(Error::Message("git:// URL has empty host".to_owned()));
    }
    Ok(GitDaemonUrl { host, port, path })
}

/// A live connection to a Git daemon over a duplex TCP socket.
///
/// Holds the read and write halves of the socket (duplicated file descriptors of
/// the same connection) plus the advertisement read on connect.
pub struct GitDaemonConnection {
    reader: TcpStream,
    writer: TcpStream,
    adv: Advertisement,
}

impl Connection for GitDaemonConnection {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut dyn Write {
        &mut self.writer
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.adv.refs
    }

    fn capabilities(&self) -> &[String] {
        &self.adv.capabilities
    }

    fn head_symref(&self) -> Option<&str> {
        self.adv.head_symref.as_deref()
    }

    fn protocol_version(&self) -> u8 {
        self.adv.protocol_version
    }

    fn finish_send(&mut self) {
        // Signal EOF to the daemon's upload-pack so a v2 `serve_loop` exits after
        // the fetch instead of blocking for another command. Best-effort.
        let _ = self.writer.shutdown(std::net::Shutdown::Write);
    }
}

/// The native `git://` daemon transport.
///
/// Connects over TCP, writes the daemon request line (`git-upload-pack
/// <path>\0host=<host>\0[version=N\0]`), and reads the ref advertisement,
/// exposing the socket as a [`Connection`]. Lifted from the CLI's
/// `git_daemon_url::connect_git_daemon_upload_pack`.
#[derive(Clone, Debug, Default)]
pub struct GitDaemonTransport {
    /// Connect timeout. `None` blocks per the OS default.
    pub connect_timeout: Option<Duration>,
    /// Read/write timeout for the established socket.
    pub io_timeout: Option<Duration>,
}

impl GitDaemonTransport {
    /// A transport with the CLI's default timeouts (30s connect, 600s I/O).
    #[must_use]
    pub fn new() -> Self {
        Self {
            connect_timeout: Some(Duration::from_secs(30)),
            io_timeout: Some(Duration::from_secs(600)),
        }
    }

    fn write_request(
        &self,
        stream_w: &mut TcpStream,
        url: &GitDaemonUrl,
        service: Service,
        opts: &ConnectOptions,
    ) -> Result<()> {
        let virtual_host = format!("{}:{}", url.host, url.port);
        let mut inner: Vec<u8> = Vec::new();
        inner.extend_from_slice(service.wire_name().as_bytes());
        inner.push(b' ');
        inner.extend_from_slice(url.path.as_bytes());
        inner.push(0);
        inner.extend_from_slice(b"host=");
        inner.extend_from_slice(virtual_host.as_bytes());
        inner.push(0);
        if opts.protocol_version > 0 {
            // The daemon's extra-parameters block is introduced by an extra NUL.
            inner.push(0);
            inner.extend_from_slice(format!("version={}\0", opts.protocol_version).as_bytes());
        }
        pkt_line::write_packet_raw(stream_w, &inner)?;
        stream_w.flush()?;
        Ok(())
    }
}

impl Transport for GitDaemonTransport {
    fn connect(
        &self,
        url: &str,
        service: Service,
        opts: &ConnectOptions,
    ) -> Result<Box<dyn Connection>> {
        crate::net_trace::net_trace!(
            "git:// connect {url} (service={}, request protocol v{})",
            service.wire_name(),
            opts.protocol_version
        );
        let parsed = parse_git_url(url)?;
        let addr = format!("{}:{}", parsed.host, parsed.port)
            .to_socket_addrs()
            .map_err(|e| {
                Error::Message(format!(
                    "could not resolve git://{}:{}: {e}",
                    parsed.host, parsed.port
                ))
            })?
            .next()
            .ok_or_else(|| {
                Error::Message(format!(
                    "no addresses for git://{}:{}",
                    parsed.host, parsed.port
                ))
            })?;

        let stream = match self.connect_timeout {
            Some(t) => TcpStream::connect_timeout(&addr, t),
            None => TcpStream::connect(addr),
        }
        .map_err(|e| {
            Error::Message(format!(
                "could not connect to git://{}:{}: {e}",
                parsed.host, parsed.port
            ))
        })?;
        if let Some(t) = self.io_timeout {
            let _ = stream.set_read_timeout(Some(t));
            let _ = stream.set_write_timeout(Some(t));
        }

        let mut writer = stream
            .try_clone()
            .map_err(|e| Error::Message(format!("dup git:// socket: {e}")))?;
        self.write_request(&mut writer, &parsed, service, opts)?;

        let mut reader = stream;
        let adv = read_advertisement(&mut reader)?;
        crate::net_trace::net_trace!(
            "git:// connected: protocol v{}, {} ref(s) advertised",
            adv.protocol_version,
            adv.refs.len()
        );

        Ok(Box::new(GitDaemonConnection {
            reader,
            writer,
            adv,
        }))
    }
}

// ===========================================================================
// ssh transport
// ===========================================================================
//
// Lifted from the CLI's `ssh_transport` (`grit/src/ssh_transport.rs`): the
// scp-style / `ssh://` / `git+ssh://` URL parser (matching the behavior of Git's
// `connect.c` `parse_connect_url`/`host_end`/`get_host_and_port`) and the
// `GIT_SSH_COMMAND` / `GIT_SSH` subprocess spawn. The remote command is the
// usual `git-upload-pack '<path>'`, shell-quoted exactly like Git's
// `sq_quote_buf`.
//
// The CLI's plink/putty variant detection and `ssh -G` probe are intentionally
// *not* ported here: this is the embedder-facing core, and OpenSSH `-p <port>`
// covers the common case. The ssh program/command is pluggable via
// [`SshTransport::ssh_command`] so embedders never depend on process globals.
//
// Spawning a subprocess for ssh is correct (ssh is not git); the no-process
// rule is about the *public API shape* (no argv/stdout/global-config
// assumptions), which the [`Transport`]/[`Connection`] traits honor.

/// A parsed SSH remote (scp-style `host:path`, `ssh://`, or `git+ssh://`).
///
/// `ssh_host` is the `user@host` token passed to the ssh program (brackets
/// already stripped for IPv6 literals); `path` is the repository path sent to
/// the remote `git-upload-pack`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshUrl {
    /// Host (and optional `user@`) as passed to ssh, with IPv6 brackets removed.
    pub ssh_host: String,
    /// Repository path on the remote (passed to `git-upload-pack`).
    pub path: String,
    /// Whether the URL was scp-style (`host:path`) rather than `ssh://`.
    pub scp_style: bool,
    /// Numeric port (`ssh://host:port/...` or `[host:port]:path`), if any.
    pub port: Option<String>,
}

/// True when `url` is an SSH transport address (`ssh://`, `git+ssh://`, or
/// scp-style `host:path`) rather than a plain local path.
///
/// Mirrors Git's `url_is_local_not_ssh` (`connect.c`): a string is local unless
/// it is `host:path` with no `/` before the first `:`.
#[must_use]
pub fn is_ssh_url(url: &str) -> bool {
    let u = url.trim();
    if u.starts_with("ext::") {
        return false;
    }
    if u.starts_with("ssh://") || u.starts_with("git+ssh://") {
        return true;
    }
    if u.contains("://") {
        return false;
    }
    !url_is_local_not_ssh(u)
}

/// Git `url_is_local_not_ssh` (`connect.c`): local unless `host:path` with no
/// `/` before the `:`.
fn url_is_local_not_ssh(url: &str) -> bool {
    let colon = url.find(':');
    let slash = url.find('/');
    match colon {
        None => true,
        Some(ci) => slash.is_some_and(|si| si < ci),
    }
}

/// Parse and validate `url` as Git would for SSH (scp-style, `ssh://`, or
/// `git+ssh://`).
///
/// Lifted verbatim from the CLI's `ssh_transport::parse_ssh_url`, a faithful
/// port of Git's `connect.c` URL parsing (bracketed IPv6, `user@host:port`, the
/// `~`-home path tweak, percent-decoding of `ssh://` paths).
///
/// # Errors
///
/// Returns an error if the URL has an empty host or path, the host starts with
/// `-`, or a percent-escape is malformed.
pub fn parse_ssh_url(url: &str) -> Result<SshUrl> {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("git+ssh://") {
        return parse_ssh_url_form(rest);
    }
    if let Some(rest) = u.strip_prefix("ssh://") {
        return parse_ssh_url_form(rest);
    }
    parse_scp_style(u)
}

fn parse_ssh_url_form(rest: &str) -> Result<SshUrl> {
    let after_slashes = rest.strip_prefix("//").unwrap_or(rest);
    let (authority, path_with_sep) = split_ssh_authority_and_path(after_slashes);
    let (user_host, port) = parse_authority_host_port(authority)?;
    if user_host.starts_with('-') {
        return Err(Error::Message("ssh: hostname starts with '-'".to_owned()));
    }
    // Git: for PROTO_SSH, if `path[1] == '~'`, advance past the leading separator
    // so `ssh://host/~repo` yields `~repo` (server-side home-dir expansion).
    let path_after_tilde = if path_with_sep.as_bytes().get(1) == Some(&b'~') {
        &path_with_sep[1..]
    } else {
        path_with_sep.as_str()
    };
    let path = normalize_ssh_url_path(path_after_tilde)?;
    Ok(SshUrl {
        ssh_host: user_host,
        path,
        scp_style: false,
        port,
    })
}

/// Split `host/path` into `(authority, path_including_leading_slash)`.
fn split_ssh_authority_and_path(s: &str) -> (&str, String) {
    let mut depth = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => return (&s[..i], s[i..].to_string()),
            _ => {}
        }
    }
    (s, String::new())
}

/// Result of Git's `host_end()` (`connect.c`) with `removebrackets`.
struct HostEnd {
    host: String,
    rest: String,
    bracketed: bool,
}

/// Faithful port of Git's `host_end()` (`connect.c`) with `removebrackets = 1`.
fn host_end_remove_brackets(authority: &str) -> HostEnd {
    let start_off = match authority.find("@[") {
        Some(at) => at + 1,
        None => 0,
    };
    let prefix = &authority[..start_off];
    let start = &authority[start_off..];
    if let Some(rest) = start.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let inner = &rest[..close];
            let after = &rest[close + 1..];
            return HostEnd {
                host: format!("{prefix}{inner}"),
                rest: after.to_string(),
                bracketed: true,
            };
        }
    }
    HostEnd {
        host: authority.to_string(),
        rest: authority.to_string(),
        bracketed: false,
    }
}

/// Faithful port of Git's `get_host_and_port()` (`connect.c`).
fn get_host_and_port(he: HostEnd) -> (String, Option<String>) {
    let HostEnd {
        host,
        rest,
        bracketed,
    } = he;
    let Some(ci) = rest.find(':') else {
        return (host, None);
    };
    let tail = &rest[ci + 1..];
    let is_port = !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
        && tail.parse::<u32>().is_ok_and(|n| n < 65536);
    if is_port {
        let trimmed_host = if bracketed {
            host
        } else {
            host[..ci].to_string()
        };
        return (trimmed_host, Some(tail.to_string()));
    }
    if tail.is_empty() {
        let trimmed_host = if bracketed {
            host
        } else {
            host[..ci].to_string()
        };
        return (trimmed_host, None);
    }
    (host, None)
}

/// Faithful port of Git's `get_port()` (`connect.c`) fallback.
fn get_port(host: String) -> (String, Option<String>) {
    let Some(ci) = host.find(':') else {
        return (host, None);
    };
    let tail = &host[ci + 1..];
    if !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
        && tail.parse::<u32>().is_ok_and(|n| n < 65536)
    {
        let h = host[..ci].to_string();
        let p = tail.to_string();
        return (h, Some(p));
    }
    (host, None)
}

/// Split `authority` into `user@host` (or `host`) and optional port.
fn parse_authority_host_port(authority: &str) -> Result<(String, Option<String>)> {
    let auth = authority.trim();
    if auth.is_empty() {
        return Err(Error::Message("ssh: empty host".to_owned()));
    }
    let (ssh_host, port) = get_host_and_port(host_end_remove_brackets(auth));
    let (ssh_host, port) = match port {
        Some(p) => (ssh_host, Some(p)),
        None => get_port(ssh_host),
    };
    if ssh_host.is_empty() {
        return Err(Error::Message("ssh: empty host".to_owned()));
    }
    if ssh_host.starts_with('-') {
        return Err(Error::Message("ssh: hostname starts with '-'".to_owned()));
    }
    Ok((ssh_host, port))
}

fn parse_scp_style(u: &str) -> Result<SshUrl> {
    let he = host_end_remove_brackets(u);
    let sep_search_start = if he.bracketed {
        u.find(']')
            .map(|i| i + 1)
            .ok_or_else(|| Error::Message("ssh: malformed host".to_owned()))?
    } else {
        0
    };
    let rel_colon = u[sep_search_start..]
        .find(':')
        .ok_or_else(|| Error::Message("ssh: no ':' in scp-style url".to_owned()))?;
    let colon_pos = sep_search_start + rel_colon;
    let host = &u[..colon_pos];
    let mut path = &u[colon_pos + 1..];

    if host.is_empty() || path.is_empty() {
        return Err(Error::Message("ssh: empty host or path".to_owned()));
    }
    if host.starts_with('-') {
        return Err(Error::Message("ssh: hostname starts with '-'".to_owned()));
    }
    if path.as_bytes().get(1) == Some(&b'~') {
        path = &path[1..];
    }
    if path.starts_with('-') {
        return Err(Error::Message("ssh: path starts with '-'".to_owned()));
    }
    let (ssh_host, port) = parse_authority_host_port(host)?;
    Ok(SshUrl {
        ssh_host,
        path: path.to_owned(),
        scp_style: true,
        port,
    })
}

fn normalize_ssh_url_path(path_part: &str) -> Result<String> {
    if path_part.is_empty() {
        return Ok(String::new());
    }
    let decoded = percent_decode_path(path_part)?;
    if decoded.starts_with('-') {
        return Err(Error::Message("ssh: path starts with '-'".to_owned()));
    }
    Ok(decoded)
}

fn percent_decode_path(path: &str) -> Result<String> {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars
                .next()
                .ok_or_else(|| Error::Message("ssh: bad % escape".to_owned()))?;
            let h2 = chars
                .next()
                .ok_or_else(|| Error::Message("ssh: bad % escape".to_owned()))?;
            let byte = u8::from_str_radix(&format!("{h1}{h2}"), 16)
                .map_err(|_| Error::Message("ssh: bad % escape".to_owned()))?;
            out.push(byte as char);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Shell-quote `s` with single quotes like Git's `sq_quote_buf` (`git/quote.c`).
fn sq_quote_shell_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("'\\''"),
            '!' => out.push_str("'\\!'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// The remote command run on the far side of the ssh connection,
/// `git-upload-pack '<path>'` (or `<service> '<path>'`).
fn remote_service_cmd(service: Service, quoted_path: &str) -> String {
    format!("{} {quoted_path}", service.wire_name())
}

/// How the [`SshTransport`] invokes ssh.
///
/// `Auto` reproduces Git's precedence: `$GIT_SSH_COMMAND` (a shell command
/// line, run via `sh -c`), else `$GIT_SSH` (a program, no shell), else the
/// `ssh` program. Embedders that do not want to depend on process-global env
/// can pin a [`SshCommand::Program`] or [`SshCommand::ShellCommand`] explicitly.
#[derive(Clone, Debug, Default)]
pub enum SshCommand {
    /// Resolve from the environment: `GIT_SSH_COMMAND`, then `GIT_SSH`, then
    /// the `ssh` program. This is the default and matches Git.
    #[default]
    Auto,
    /// A bare program invoked directly (no shell), like Git's `$GIT_SSH`. The
    /// argv is `[program, <-p port>, host, remote_cmd]`.
    Program(OsString),
    /// A shell command line run via `sh -c`, like Git's `$GIT_SSH_COMMAND`. The
    /// command is appended with `<-p port> host remote_cmd`.
    ShellCommand(OsString),
}

impl SshCommand {
    /// Resolve `Auto` against the current environment to a concrete variant.
    fn resolve(&self) -> SshCommand {
        match self {
            SshCommand::Auto => {
                if let Some(c) =
                    std::env::var_os("GIT_SSH_COMMAND").filter(|v| !v.is_empty())
                {
                    SshCommand::ShellCommand(c)
                } else if let Some(p) = std::env::var_os("GIT_SSH").filter(|v| !v.is_empty()) {
                    SshCommand::Program(p)
                } else {
                    SshCommand::Program(OsString::from("ssh"))
                }
            }
            other => other.clone(),
        }
    }
}

/// A live connection to a remote Git service over an ssh subprocess.
///
/// The child's stdin/stdout are the pkt-line stream; the advertisement is read
/// on connect. Dropping the connection closes the pipes (signalling EOF to the
/// remote) and reaps the child.
pub struct SshConnection {
    child: Child,
    // `Option` so [`Connection::finish_send`] can drop stdin (sending EOF to the
    // remote `git-upload-pack`) without consuming the connection.
    writer: Option<ChildStdin>,
    reader: ChildStdout,
    adv: Advertisement,
}

impl Connection for SshConnection {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut dyn Write {
        self.writer
            .as_mut()
            .expect("ssh connection writer used after finish_send")
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.adv.refs
    }

    fn capabilities(&self) -> &[String] {
        &self.adv.capabilities
    }

    fn head_symref(&self) -> Option<&str> {
        self.adv.head_symref.as_deref()
    }

    fn protocol_version(&self) -> u8 {
        self.adv.protocol_version
    }

    fn finish_send(&mut self) {
        // Dropping the child's stdin closes the pipe, signalling EOF so the
        // remote `git-upload-pack` v2 `serve_loop` exits instead of blocking for
        // another command (which would hang the `child.wait()` in `Drop`).
        self.writer = None;
    }
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        // Close the write half (child stdin) *before* waiting: a remote blocked
        // reading its input — e.g. a `git-receive-pack` still waiting for the
        // command list after a client-side-only push decision (non-ff reject,
        // up-to-date) sent nothing — only exits once it sees EOF. Dropping the
        // `ChildStdin` here signals that EOF; otherwise `child.wait()` would
        // deadlock against a process that never terminates. (Fields drop after
        // `drop()` returns, i.e. after the wait, so we must close it explicitly.)
        self.writer = None;
        // Best-effort reap so we don't leak zombies if the caller drops mid-stream.
        let _ = self.child.wait();
    }
}

/// The `ssh` transport: spawn `ssh [opts] <host> git-upload-pack '<path>'` and
/// expose the child's stdio as a [`Connection`].
///
/// URL parsing and the `GIT_SSH_COMMAND`/`GIT_SSH`/`ssh` spawn are lifted from
/// the CLI's `ssh_transport`. The ssh program is pluggable via [`Self::ssh_command`]
/// so embedders can inject their own ssh (or a recording shim) without touching
/// process globals; the default ([`SshCommand::Auto`]) reproduces Git's
/// precedence.
#[derive(Clone, Debug, Default)]
pub struct SshTransport {
    /// How to invoke ssh. Defaults to [`SshCommand::Auto`] (env, then `ssh`).
    pub ssh_command: SshCommand,
}

impl SshTransport {
    /// A transport that resolves ssh from the environment (`GIT_SSH_COMMAND` /
    /// `GIT_SSH`), falling back to the `ssh` program — Git's default behavior.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A transport pinned to a specific ssh *program* (no shell), like
    /// `$GIT_SSH`.
    #[must_use]
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            ssh_command: SshCommand::Program(program.into()),
        }
    }

    /// A transport pinned to a specific ssh *shell command line* (run via
    /// `sh -c`), like `$GIT_SSH_COMMAND`.
    #[must_use]
    pub fn with_shell_command(command: impl Into<OsString>) -> Self {
        Self {
            ssh_command: SshCommand::ShellCommand(command.into()),
        }
    }

    /// Build and spawn the ssh child for `spec`/`service`, returning the live
    /// child with piped stdin/stdout.
    fn spawn(&self, spec: &SshUrl, service: Service, opts: &ConnectOptions) -> Result<Child> {
        let quoted_path = sq_quote_shell_arg(&spec.path);
        let remote_cmd = remote_service_cmd(service, &quoted_path);
        let port = spec.port.as_deref();

        let mut command = match self.ssh_command.resolve() {
            SshCommand::ShellCommand(cmd) => {
                // Reproduce Git's `GIT_SSH_COMMAND`: run the command line through
                // a shell, appending the (shell-quoted) host and remote command.
                let cmd = cmd.to_string_lossy();
                let port_opt = match port {
                    Some(p) => format!(" -p {}", shell_words::quote(p)),
                    None => String::new(),
                };
                let script = format!(
                    "{cmd}{port_opt} {} {}",
                    shell_words::quote(&spec.ssh_host),
                    shell_words::quote(&remote_cmd),
                );
                let mut c = Command::new("sh");
                c.arg("-c").arg(script);
                c
            }
            SshCommand::Program(prog) => {
                // Reproduce Git's `$GIT_SSH` / default `ssh`: direct argv, no shell.
                let mut c = Command::new(&prog);
                if let Some(p) = port {
                    c.arg("-p").arg(p);
                }
                c.arg(&spec.ssh_host).arg(&remote_cmd);
                c
            }
            // `resolve()` never returns `Auto`.
            SshCommand::Auto => unreachable!("SshCommand::resolve never yields Auto"),
        };

        // Request the wire protocol version the same way Git does: export
        // `GIT_PROTOCOL=version=N` into the ssh process environment. OpenSSH
        // forwards it (Git ships a `SendEnv GIT_PROTOCOL` default) and the remote
        // `git-upload-pack` reads it to switch to v2; servers that don't see it
        // fall back to the v0 advertisement, which `read_advertisement` still
        // parses. Only set it for v1/v2 so a plain v0 request is unchanged.
        if opts.protocol_version > 0 {
            command.env("GIT_PROTOCOL", format!("version={}", opts.protocol_version));
        }

        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Message(format!("failed to spawn ssh for {}: {e}", spec.ssh_host)))
    }
}

impl Transport for SshTransport {
    fn connect(
        &self,
        url: &str,
        service: Service,
        opts: &ConnectOptions,
    ) -> Result<Box<dyn Connection>> {
        crate::net_trace::net_trace!(
            "ssh connect {url} (service={}, request protocol v{})",
            service.wire_name(),
            opts.protocol_version
        );
        let spec = parse_ssh_url(url)?;
        let mut child = self.spawn(&spec, service, opts)?;

        let writer = child
            .stdin
            .take()
            .ok_or_else(|| Error::Message("ssh child has no stdin".to_owned()))?;
        let mut reader = child
            .stdout
            .take()
            .ok_or_else(|| Error::Message("ssh child has no stdout".to_owned()))?;

        let adv = read_advertisement(&mut reader)?;
        crate::net_trace::net_trace!(
            "ssh connected: protocol v{}, {} ref(s) advertised",
            adv.protocol_version,
            adv.refs.len()
        );

        Ok(Box::new(SshConnection {
            child,
            writer: Some(writer),
            reader,
            adv,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_url_defaults_and_ports() {
        let u = parse_git_url("git://example.com/repo.git").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 9418);
        assert_eq!(u.path, "/repo.git");

        let u = parse_git_url("git://example.com:9999/a/b").unwrap();
        assert_eq!(u.port, 9999);
        assert_eq!(u.path, "/a/b");

        let u = parse_git_url("git://[::1]:1234/x").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 1234);
        assert_eq!(u.path, "/x");

        assert!(parse_git_url("https://x/y").is_err());
        assert!(parse_git_url("git://host").is_err());
    }

    #[test]
    fn parse_advertisement_line_sha1_and_sha256() {
        let sha1 = "1234567890123456789012345678901234567890 refs/heads/main\0caps here";
        let (oid, name, caps) = parse_ref_advertisement_line(sha1).unwrap();
        assert_eq!(oid.to_hex(), "1234567890123456789012345678901234567890");
        assert_eq!(name, "refs/heads/main");
        assert_eq!(caps, "caps here");

        let hex64 = "0".repeat(64);
        let line = format!("{hex64} refs/heads/x");
        let (oid, name, caps) = parse_ref_advertisement_line(&line).unwrap();
        assert_eq!(oid.to_hex().len(), 64);
        assert_eq!(name, "refs/heads/x");
        assert_eq!(caps, "");

        assert!(parse_ref_advertisement_line("shallow abc").is_none());
    }

    #[test]
    fn read_advertisement_captures_refs_caps_and_symref() {
        let mut buf: Vec<u8> = Vec::new();
        let main = "1111111111111111111111111111111111111111";
        let head = format!(
            "{main} HEAD\0multi_ack symref=HEAD:refs/heads/main agent=git/2",
        );
        pkt_line::write_line_to_vec(&mut buf, &head).unwrap();
        let r = format!("{main} refs/heads/main");
        pkt_line::write_line_to_vec(&mut buf, &r).unwrap();
        let tag = "2222222222222222222222222222222222222222";
        let t = format!("{tag} refs/tags/v1");
        pkt_line::write_line_to_vec(&mut buf, &t).unwrap();
        let peeled = format!("{main} refs/tags/v1^{{}}");
        pkt_line::write_line_to_vec(&mut buf, &peeled).unwrap();
        buf.extend_from_slice(b"0000");

        let mut cur = std::io::Cursor::new(buf);
        let adv = read_advertisement(&mut cur).unwrap();
        assert_eq!(adv.head_symref.as_deref(), Some("refs/heads/main"));
        assert!(adv.capabilities.iter().any(|c| c == "multi_ack"));
        // HEAD, capabilities and peeled lines excluded; main + v1 recorded.
        let names: Vec<&str> = adv.refs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["refs/heads/main", "refs/tags/v1"]);
    }

    #[test]
    fn read_advertisement_v2_captures_caps_and_no_refs() {
        // A v2 advertisement: `version 2`, capability lines, flush — and no refs.
        let mut buf: Vec<u8> = Vec::new();
        pkt_line::write_line_to_vec(&mut buf, "version 2").unwrap();
        pkt_line::write_line_to_vec(&mut buf, "agent=git/2.43.0").unwrap();
        pkt_line::write_line_to_vec(&mut buf, "ls-refs=unborn").unwrap();
        pkt_line::write_line_to_vec(&mut buf, "fetch=shallow wait-for-done filter").unwrap();
        pkt_line::write_line_to_vec(&mut buf, "object-format=sha1").unwrap();
        buf.extend_from_slice(b"0000");

        let mut cur = std::io::Cursor::new(buf);
        let adv = read_advertisement(&mut cur).unwrap();
        assert_eq!(adv.protocol_version, 2);
        assert!(adv.refs.is_empty(), "v2 advertisement carries no refs");
        assert!(adv.capabilities.iter().any(|c| c == "agent=git/2.43.0"));
        assert!(adv
            .capabilities
            .iter()
            .any(|c| c == "fetch=shallow wait-for-done filter"));
        assert!(adv.capabilities.iter().any(|c| c == "object-format=sha1"));
        assert!(adv.head_symref.is_none());
    }

    #[test]
    fn is_ssh_url_classification() {
        assert!(is_ssh_url("ssh://host/repo.git"));
        assert!(is_ssh_url("git+ssh://host/repo.git"));
        assert!(is_ssh_url("user@host:repo.git"));
        assert!(is_ssh_url("host:path/to/repo"));
        // Plain local paths and other schemes are not ssh.
        assert!(!is_ssh_url("/abs/local/repo"));
        assert!(!is_ssh_url("./relative"));
        assert!(!is_ssh_url("git://host/repo.git"));
        assert!(!is_ssh_url("https://host/repo.git"));
        assert!(!is_ssh_url("ext::sh -c foo"));
        // `host:path` with a `/` before the `:` is a local path, not ssh.
        assert!(!is_ssh_url("./a:b"));
    }

    #[test]
    fn parse_scp_style_url() {
        let u = parse_ssh_url("git@example.com:my/repo.git").unwrap();
        assert_eq!(u.ssh_host, "git@example.com");
        assert_eq!(u.path, "my/repo.git");
        assert!(u.scp_style);
        assert_eq!(u.port, None);
    }

    #[test]
    fn parse_ssh_scheme_url_with_port() {
        let u = parse_ssh_url("ssh://git@example.com:2222/srv/repo.git").unwrap();
        assert_eq!(u.ssh_host, "git@example.com");
        assert_eq!(u.path, "/srv/repo.git");
        assert!(!u.scp_style);
        assert_eq!(u.port.as_deref(), Some("2222"));
    }

    #[test]
    fn parse_ssh_url_ipv6_and_tilde() {
        let u = parse_ssh_url("ssh://git@[::1]:2222/~/repo.git").unwrap();
        assert_eq!(u.ssh_host, "git@::1");
        assert_eq!(u.port.as_deref(), Some("2222"));
        // The `~` home-dir form drops the leading separator.
        assert_eq!(u.path, "~/repo.git");

        // scp-style bracketed host with embedded port.
        let u = parse_ssh_url("[git@host:2200]:repo.git").unwrap();
        assert_eq!(u.ssh_host, "git@host");
        assert_eq!(u.port.as_deref(), Some("2200"));
        assert_eq!(u.path, "repo.git");
    }

    #[test]
    fn parse_ssh_url_rejects_bad_inputs() {
        assert!(parse_ssh_url("ssh://-badhost/repo").is_err());
        assert!(parse_ssh_url("host:-dashpath").is_err());
        assert!(parse_ssh_url("host:").is_err());
    }

    #[test]
    fn remote_command_is_shell_quoted() {
        let cmd = remote_service_cmd(Service::UploadPack, &sq_quote_shell_arg("/srv/repo.git"));
        assert_eq!(cmd, "git-upload-pack '/srv/repo.git'");
        // A single quote in the path is escaped Git-style.
        let q = sq_quote_shell_arg("a'b");
        assert_eq!(q, "'a'\\''b'");
        // receive-pack uses the matching service name.
        let cmd = remote_service_cmd(Service::ReceivePack, &sq_quote_shell_arg("p"));
        assert_eq!(cmd, "git-receive-pack 'p'");
    }
}
