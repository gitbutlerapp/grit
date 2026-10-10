//! Server side of the Git wire protocol: answering fetches and accepting pushes.
//!
//! This module lets a process act as the remote end of a clone, fetch, or push.
//! [`upload_pack`] serves objects to a fetching client (protocol v0, v1, and v2)
//! and [`receive_pack`] accepts a pushed pack plus ref updates. Both operate on
//! plain byte streams, so the same code serves a duplex connection (ssh, a
//! local pipe) and the stateless request/response exchanges of smart HTTP.
//!
//! The functions take an already-opened [`Repository`] and explicit options; they
//! never read the process environment or print to the terminal. Callers are
//! expected to map `GIT_PROTOCOL` to a [`ProtocolVersion`] and to load
//! configuration at their own boundary.

mod receive_pack;
mod upload_pack;

pub use receive_pack::{
    receive_pack, HookRunRecord, ReceiveOutcome, ReceivePolicy, RefUpdateResult,
};
pub use upload_pack::upload_pack;

use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;

/// Errors produced while serving a fetch or a push.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The peer sent something that does not follow the wire protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The client asked for an object that is not an advertised ref tip.
    #[error("not our ref: {0}")]
    NotOurRef(ObjectId),
    /// The client requested a v2 command this server does not implement.
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    /// The client and repository disagree on the object hash algorithm.
    #[error("object format mismatch: client wants {client}, repository uses {repository}")]
    ObjectFormatMismatch {
        /// Hash algorithm named by the client.
        client: String,
        /// Hash algorithm of the served repository.
        repository: &'static str,
    },
    /// Reading or writing repository data failed.
    #[error(transparent)]
    Repository(#[from] crate::error::Error),
    /// The underlying stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for serving operations.
pub type Result<T> = std::result::Result<T, ServeError>;

/// Wire protocol version requested by the client.
///
/// Version 1 is version 0 with an explicit `version 1` line in front of the
/// advertisement. Version 2 replaces the ref advertisement with a capability
/// list and command requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolVersion {
    /// The original protocol, used when the client asks for nothing else.
    #[default]
    V0,
    /// Version 0 framing announced with a `version 1` line.
    V1,
    /// Command-based protocol version 2.
    V2,
}

impl ProtocolVersion {
    /// Pick the highest version named in a `GIT_PROTOCOL` style value.
    ///
    /// The value is a colon-separated list of `key=value` entries; only
    /// `version=N` entries are considered. Unknown or missing versions fall
    /// back to [`ProtocolVersion::V0`].
    ///
    /// # Parameters
    ///
    /// - `value`: the raw value, for example `version=2` or `version=2:foo=bar`.
    ///
    /// # Returns
    ///
    /// The highest supported version the value asks for.
    #[must_use]
    pub fn from_git_protocol(value: &str) -> Self {
        value
            .split(':')
            .filter_map(|entry| entry.strip_prefix("version="))
            .filter_map(|v| match v {
                "2" => Some(Self::V2),
                "1" => Some(Self::V1),
                "0" => Some(Self::V0),
                _ => None,
            })
            .max_by_key(|v| match v {
                Self::V0 => 0,
                Self::V1 => 1,
                Self::V2 => 2,
            })
            .unwrap_or_default()
    }
}

/// How a serving session is framed on the wire.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Protocol version the client asked for.
    pub protocol: ProtocolVersion,
    /// Handle exactly one request from the input and stop, without printing the
    /// advertisement first. Smart HTTP uses this for its POST requests.
    pub stateless_rpc: bool,
    /// Print the ref (or capability) advertisement and stop. Smart HTTP uses
    /// this to answer `GET info/refs`.
    pub advertise_refs: bool,
    /// Value for the `agent=` capability, such as `grit/0.5.0`.
    pub agent: String,
    /// Ref patterns hidden from the advertisement (`transfer.hideRefs` and the
    /// service-specific `uploadpack.hideRefs` / `receive.hideRefs`).
    pub hidden_refs: Vec<String>,
}

/// One ref as it appears in an advertisement.
#[derive(Debug, Clone)]
pub(crate) struct AdvertisedRef {
    /// Full ref name, such as `refs/heads/main`.
    pub(crate) name: String,
    /// Object the ref points at.
    pub(crate) oid: ObjectId,
    /// For an annotated tag, the non-tag object it ultimately points at.
    pub(crate) peeled: Option<ObjectId>,
}

/// List the repository's refs in advertisement order, skipping hidden ones.
///
/// Refs whose names are not safe to send, or that point at missing objects,
/// are left out rather than failing the whole advertisement.
pub(crate) fn advertised_refs(repo: &Repository, hidden: &[String]) -> Result<Vec<AdvertisedRef>> {
    let mut refs = crate::refs::list_refs(&repo.git_dir, "refs/")?;
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(refs.len());
    for (name, oid) in refs {
        if !crate::refs::is_valid_fetch_advertised_ref(&name)
            || crate::hide_refs::ref_is_hidden(&name, &name, hidden)
        {
            continue;
        }
        let Ok(peeled) = peel_tag(repo, oid) else {
            continue;
        };
        out.push(AdvertisedRef {
            name,
            oid,
            peeled: (peeled != oid).then_some(peeled),
        });
    }
    Ok(out)
}

/// Follow annotated tags until a non-tag object is reached.
///
/// # Errors
///
/// Fails when an object in the chain is missing or a tag cannot be parsed.
pub(crate) fn peel_tag(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    // Bound the walk so a corrupt tag cycle cannot loop forever.
    for _ in 0..64 {
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Tag {
            return Ok(oid);
        }
        oid = crate::objects::parse_tag(&obj.data)?.object;
    }
    Err(ServeError::Protocol(format!("tag chain too deep at {oid}")))
}

/// Where `HEAD` points: its symbolic target (if any) and resolved object (if born).
pub(crate) struct HeadInfo {
    /// Ref `HEAD` names, such as `refs/heads/main`, when `HEAD` is symbolic.
    pub(crate) target: Option<String>,
    /// Object `HEAD` resolves to; `None` on an unborn branch.
    pub(crate) oid: Option<ObjectId>,
}

/// Read `HEAD` for the advertisement.
pub(crate) fn head_info(repo: &Repository) -> HeadInfo {
    HeadInfo {
        target: crate::refs::read_symbolic_ref(&repo.git_dir, "HEAD")
            .ok()
            .flatten(),
        oid: crate::refs::resolve_ref(&repo.git_dir, "HEAD").ok(),
    }
}

/// Write a pkt-line carrying `data` followed by a newline.
pub(crate) fn write_line(out: &mut dyn std::io::Write, data: &str) -> Result<()> {
    let mut buf = Vec::with_capacity(data.len() + 5);
    crate::pkt_line::write_line_to_vec(&mut buf, data)?;
    out.write_all(&buf)?;
    Ok(())
}

/// Write a flush packet.
pub(crate) fn write_flush(out: &mut dyn std::io::Write) -> Result<()> {
    out.write_all(crate::pkt_line::FLUSH.as_bytes())?;
    Ok(())
}

/// Read one packet, normalising a data line by dropping its trailing newline.
pub(crate) fn read_packet(input: &mut dyn std::io::Read) -> Result<Option<Packet>> {
    let pkt = crate::pkt_line::read_packet(&mut &mut *input)?;
    Ok(pkt.map(|p| match p {
        crate::pkt_line::Packet::Data(s) => Packet::Data(s.trim_end_matches('\n').to_owned()),
        crate::pkt_line::Packet::Flush => Packet::Flush,
        crate::pkt_line::Packet::Delim => Packet::Delim,
        crate::pkt_line::Packet::ResponseEnd => Packet::ResponseEnd,
    }))
}

/// A packet read from the client, with data lines already trimmed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Packet {
    /// A data line without its trailing newline.
    Data(String),
    /// `0000`.
    Flush,
    /// `0001`.
    Delim,
    /// `0002`.
    ResponseEnd,
}

/// Parse a full-width hex object id for the repository's hash algorithm.
pub(crate) fn parse_oid(repo: &Repository, hex: &str) -> Result<ObjectId> {
    let oid = ObjectId::from_hex(hex)
        .map_err(|_| ServeError::Protocol(format!("invalid object id '{hex}'")))?;
    if oid.algo() != repo.odb.hash_algo() {
        return Err(ServeError::Protocol(format!(
            "object id '{hex}' does not match the repository hash algorithm"
        )));
    }
    Ok(oid)
}

/// Check a client's `object-format=` capability against the repository.
pub(crate) fn check_object_format(repo: &Repository, client: Option<&str>) -> Result<()> {
    let ours = repo.odb.hash_algo().name();
    match client {
        Some(name) if name != ours => Err(ServeError::ObjectFormatMismatch {
            client: name.to_owned(),
            repository: ours,
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::ProtocolVersion;

    #[test]
    fn git_protocol_picks_highest_version() {
        assert_eq!(ProtocolVersion::from_git_protocol(""), ProtocolVersion::V0);
        assert_eq!(
            ProtocolVersion::from_git_protocol("version=1"),
            ProtocolVersion::V1
        );
        assert_eq!(
            ProtocolVersion::from_git_protocol("version=2:object-format=sha1"),
            ProtocolVersion::V2
        );
        assert_eq!(
            ProtocolVersion::from_git_protocol("version=9"),
            ProtocolVersion::V0
        );
    }
}
