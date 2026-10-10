//! Internal RPC abstraction shared by duplex and smart-HTTP transports.
//!
//! [`RpcChannel`] models one round-trip of the Git upload-pack/receive-pack
//! protocols: send a pkt-line request body, read the response stream. Duplex
//! connections ([`crate::transport::Connection`]) implement this by writing to
//! the socket; smart HTTP ([`crate::transport::stateless_http::StatelessHttpConnection`])
//! replays prior state and POSTs each round per gitprotocol-http.

use std::io::Read;

use crate::error::Result;

/// A single protocol round over upload-pack or receive-pack.
#[expect(
    dead_code,
    reason = "narrow transport RPC surface; wired in follow-up refactors"
)]
pub(crate) trait RpcChannel {
    /// Whether each [`request`](Self::request) is an independent HTTP POST that
    /// must resend prior negotiation state (smart HTTP), vs. one long duplex stream.
    fn is_stateless(&self) -> bool;

    /// Send `body` and return the server's response bytes as a stream.
    ///
    /// # Errors
    ///
    /// Returns an error on transport or HTTP failures.
    fn request(&mut self, body: &[u8]) -> Result<Box<dyn Read + Send>>;
}
