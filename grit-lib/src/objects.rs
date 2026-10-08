//! Git object model: object IDs, kinds, and in-memory representations.
//!
//! # Object ID
//!
//! [`ObjectId`] is a 20-byte SHA-1 digest.  It implements `Display` as
//! lowercase hex, `FromStr` from a 40-character hex string, and the standard
//! ordering traits so it can be used as a map key.
//!
//! # Object Kind
//!
//! [`ObjectKind`] represents the four Git object types: blob, tree, commit,
//! and tag.  The raw header byte-slice is parsed with [`ObjectKind::from_bytes`].
//!
//! # Parsed objects
//!
//! [`Object`] bundles a kind and its raw (decompressed, header-stripped) byte
//! content.  Higher-level parsed forms (e.g. [`TreeEntry`], [`CommitData`])
//! live in this module and are produced by fallible `TryFrom<&Object>`
//! conversions.

use std::fmt;
use std::str::FromStr;

use crate::commit_encoding;
use crate::error::{Error, Result};

/// A Git hash algorithm.
///
/// Git supports two object-id hash functions: the historical SHA-1 (20-byte
/// digests, 40 hex chars) and the newer SHA-256 (32-byte digests, 64 hex
/// chars). A repository's algorithm is recorded in `extensions.objectformat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum HashAlgo {
    /// SHA-1 — 20-byte digests.
    #[default]
    Sha1,
    /// SHA-256 — 32-byte digests.
    Sha256,
}

impl HashAlgo {
    /// The raw digest length in bytes (20 for SHA-1, 32 for SHA-256).
    #[must_use]
    pub const fn len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    /// Always false — every hash algorithm has a non-zero digest length.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        false
    }

    /// The hex-encoded digest length (40 for SHA-1, 64 for SHA-256).
    #[must_use]
    pub const fn hex_len(self) -> usize {
        self.len() * 2
    }

    /// The lowercase name as written in `extensions.objectformat`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    /// The `oid_version` byte used in `.idx`/multi-pack-index headers
    /// (SHA-1 → 1, SHA-256 → 2).
    #[must_use]
    pub const fn oid_version(self) -> u8 {
        match self {
            Self::Sha1 => 1,
            Self::Sha256 => 2,
        }
    }

    /// Parse from the name used in `extensions.objectformat` config.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            v if v.eq_ignore_ascii_case("sha1") => Some(Self::Sha1),
            v if v.eq_ignore_ascii_case("sha256") => Some(Self::Sha256),
            _ => None,
        }
    }

    /// The algorithm implied by a raw digest length, if recognised.
    #[must_use]
    pub const fn from_len(len: usize) -> Option<Self> {
        match len {
            20 => Some(Self::Sha1),
            32 => Some(Self::Sha256),
            _ => None,
        }
    }
}

/// Error when converting an on-disk hash-version byte to [`HashAlgo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown Git hash version byte {0} (expected 1 for SHA-1 or 2 for SHA-256)")]
pub struct HashVersionError(pub u8);

impl From<HashAlgo> for u8 {
    fn from(algo: HashAlgo) -> Self {
        algo.oid_version()
    }
}

impl TryFrom<u8> for HashAlgo {
    type Error = HashVersionError;

    fn try_from(version: u8) -> std::result::Result<Self, Self::Error> {
        match version {
            1 => Ok(Self::Sha1),
            2 => Ok(Self::Sha256),
            other => Err(HashVersionError(other)),
        }
    }
}

/// Maximum raw digest length across supported hash algorithms (SHA-256).
const MAX_OID_LEN: usize = 32;

/// Canonical SHA-1 empty tree digest (`4b825dc642cb6eb9a060e54bf8d69288fbee4904`).
const WELL_KNOWN_EMPTY_TREE_CANON: [u8; 20] = [
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88,
    0xfb, 0xee, 0x49, 0x04,
];
/// Legacy typo empty tree digest still referenced by some tests (`…899d69f7c6948d4`).
const WELL_KNOWN_EMPTY_TREE_LEGACY: [u8; 20] = [
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0x99, 0xd6, 0x9f,
    0x7c, 0x69, 0x48, 0xd4,
];

/// A Git object identifier: a SHA-1 (20-byte) or SHA-256 (32-byte) digest.
///
/// The digest is stored in a fixed 32-byte buffer with an explicit length;
/// bytes beyond `len` are always zero, so the derived `Eq`/`Ord`/`Hash`
/// remain correct. The hash algorithm is inferred from the length.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    bytes: [u8; MAX_OID_LEN],
    len: u8,
}

impl ObjectId {
    /// The all-zero SHA-1 object id (Git's "null" OID).
    ///
    /// Used for index placeholders such as intent-to-add entries and for
    /// special cases in plumbing output. For an algorithm-specific null OID
    /// (e.g. 64 zeros in a SHA-256 repo) use [`ObjectId::null`].
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            bytes: [0u8; MAX_OID_LEN],
            len: 20,
        }
    }

    /// The all-zero ("null") object id for a given hash algorithm.
    #[must_use]
    pub const fn null(algo: HashAlgo) -> Self {
        Self {
            bytes: [0u8; MAX_OID_LEN],
            len: algo.len() as u8,
        }
    }

    /// Construct from a raw digest slice (20 bytes for SHA-1, 32 for SHA-256).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] when `bytes` is not a recognised
    /// digest length.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if HashAlgo::from_len(bytes.len()).is_none() {
            return Err(Error::InvalidObjectId(hex::encode(bytes)));
        }
        let mut buf = [0u8; MAX_OID_LEN];
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: buf,
            len: bytes.len() as u8,
        })
    }

    /// Raw digest bytes (20 or 32 bytes depending on the hash algorithm).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The hash algorithm this OID belongs to.
    #[must_use]
    pub fn algo(&self) -> HashAlgo {
        HashAlgo::from_len(self.len as usize).unwrap_or(HashAlgo::Sha1)
    }

    /// Check if this is the null (all-zero) object ID.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }

    /// Whether this OID is Git's well-known empty tree (canonical or legacy typo hash).
    #[must_use]
    pub(crate) fn is_well_known_empty_tree(&self) -> bool {
        self.as_bytes().len() == HashAlgo::Sha1.len()
            && (self.as_bytes() == WELL_KNOWN_EMPTY_TREE_CANON
                || self.as_bytes() == WELL_KNOWN_EMPTY_TREE_LEGACY)
    }

    /// Whether this OID is the canonical SHA-1 empty tree (`4b825dc6…4904`).
    #[must_use]
    pub(crate) fn is_canonical_empty_tree(&self) -> bool {
        self.as_bytes().len() == HashAlgo::Sha1.len()
            && self.as_bytes() == WELL_KNOWN_EMPTY_TREE_CANON
    }

    /// Append `/<xx>/<rest-of-hex>` for this OID under `path` without heap hex strings.
    pub(crate) fn push_loose_relative(&self, path: &mut std::path::PathBuf) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes = self.as_bytes();
        let prefix = [
            HEX[(bytes[0] >> 4) as usize],
            HEX[(bytes[0] & 0xf) as usize],
        ];
        // SAFETY: `prefix` and `suffix` are ASCII hex digits only.
        path.push(unsafe { std::str::from_utf8_unchecked(&prefix) });
        let suffix_len = (bytes.len() - 1) * 2;
        let mut suffix = [0u8; 62];
        for (i, &b) in bytes[1..].iter().enumerate() {
            suffix[i * 2] = HEX[(b >> 4) as usize];
            suffix[i * 2 + 1] = HEX[(b & 0xf) as usize];
        }
        path.push(unsafe { std::str::from_utf8_unchecked(&suffix[..suffix_len]) });
    }

    /// Build `<objects-dir>/<xx>/<rest>` for this OID without intermediate hex [`String`]s.
    #[must_use]
    pub(crate) fn loose_path_in(&self, objects_dir: &std::path::Path) -> std::path::PathBuf {
        let mut path = objects_dir.to_path_buf();
        self.push_loose_relative(&mut path);
        path
    }

    /// Temp loose-object filename (`tmp_<suffix>`) inside a prefix directory.
    #[must_use]
    pub(crate) fn loose_tmp_path_in_prefix(
        &self,
        prefix_dir: &std::path::Path,
    ) -> std::path::PathBuf {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes = self.as_bytes();
        let suffix_len = (bytes.len() - 1) * 2;
        let mut name = [0u8; 4 + 62];
        name[..4].copy_from_slice(b"tmp_");
        for (i, &b) in bytes[1..].iter().enumerate() {
            name[4 + i * 2] = HEX[(b >> 4) as usize];
            name[4 + i * 2 + 1] = HEX[(b & 0xf) as usize];
        }
        prefix_dir.join(unsafe { std::str::from_utf8_unchecked(&name[..4 + suffix_len]) })
    }

    /// Lowercase hex representation (40 or 64 characters).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    /// The two-character directory prefix used by the loose object store.
    ///
    /// Returns the first two hex chars (e.g. `"ab"` for `"ab3f…"`).
    #[must_use]
    pub fn loose_prefix(&self) -> String {
        hex::encode(&self.bytes[..1])
    }

    /// Parse an object ID from a hex string (40 chars for SHA-1, 64 for
    /// SHA-256).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] if the string is not a valid hex OID.
    pub fn from_hex(s: &str) -> Result<Self> {
        s.parse()
    }

    /// The suffix used as the filename inside the loose prefix dir (the digest
    /// minus its first byte: 38 hex chars for SHA-1, 62 for SHA-256).
    #[must_use]
    pub fn loose_suffix(&self) -> String {
        hex::encode(&self.bytes[1..self.len as usize])
    }

    /// Whether `s` is a full-length hex OID for a supported hash algorithm
    /// (40 hex chars for SHA-1 or 64 for SHA-256), case-insensitive.
    #[must_use]
    pub fn is_full_hex(s: &str) -> bool {
        (s.len() == HashAlgo::Sha1.hex_len() || s.len() == HashAlgo::Sha256.hex_len())
            && s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Whether `len` is a valid full hex-OID length (40 or 64).
    #[must_use]
    pub const fn is_hex_len(len: usize) -> bool {
        len == HashAlgo::Sha1.hex_len() || len == HashAlgo::Sha256.hex_len()
    }

    /// Whether `len` is a valid loose-object filename suffix length, i.e. a full
    /// hex OID minus its first byte (38 for SHA-1, 62 for SHA-256).
    #[must_use]
    pub const fn is_loose_suffix_len(len: usize) -> bool {
        len == HashAlgo::Sha1.hex_len() - 2 || len == HashAlgo::Sha256.hex_len() - 2
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != HashAlgo::Sha1.hex_len() && s.len() != HashAlgo::Sha256.hex_len() {
            return Err(Error::InvalidObjectId(s.to_owned()));
        }
        let bytes = hex::decode(s).map_err(|_| Error::InvalidObjectId(s.to_owned()))?;
        Self::from_bytes(&bytes)
    }
}

/// The four Git object types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A raw file snapshot.
    Blob,
    /// A directory listing.
    Tree,
    /// A snapshot with metadata and parentage.
    Commit,
    /// An annotated tag.
    Tag,
}

impl ObjectKind {
    /// Parse from the ASCII keyword used in Git object headers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownObjectType`] for unrecognised strings.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        match b {
            b"blob" => Ok(Self::Blob),
            b"tree" => Ok(Self::Tree),
            b"commit" => Ok(Self::Commit),
            b"tag" => Ok(Self::Tag),
            other => Err(Error::UnknownObjectType(
                String::from_utf8_lossy(other).into_owned(),
            )),
        }
    }

    /// Parse the `type` field on an annotated tag object (Git `type_from_string_gently` rules).
    ///
    /// The tag header line is `type <typename>\n` where `typename` must match a known object type
    /// keyword **exactly** (no extra characters, no strict prefix of a longer keyword).
    #[must_use]
    pub fn from_tag_type_field(line: &[u8]) -> Option<Self> {
        fn keyword_matches(canonical: &[u8], field: &[u8]) -> bool {
            if field.is_empty() {
                return false;
            }
            for (i, &bc) in field.iter().enumerate() {
                let sc = canonical.get(i).copied().unwrap_or(0);
                if sc != bc {
                    return false;
                }
            }
            canonical.get(field.len()).copied().unwrap_or(0) == 0
        }

        const NAMES: &[(ObjectKind, &[u8])] = &[
            (ObjectKind::Blob, b"blob"),
            (ObjectKind::Tree, b"tree"),
            (ObjectKind::Commit, b"commit"),
            (ObjectKind::Tag, b"tag"),
        ];
        for &(kind, name) in NAMES {
            if keyword_matches(name, line) {
                return Some(kind);
            }
        }
        None
    }

    /// The ASCII keyword for this kind (used in object headers).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ObjectKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::from_bytes(s.as_bytes())
    }
}

/// Kind and uncompressed size of a Git object without loading its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Object type after resolving any pack delta chain.
    pub kind: ObjectKind,
    /// Uncompressed byte length of the object body (after the header NUL).
    pub size: u64,
}

/// A decompressed, header-stripped Git object.
#[derive(Debug, Clone)]
pub struct Object {
    /// The type of this object.
    pub kind: ObjectKind,
    /// Raw byte content (everything after the NUL in the header).
    pub data: Vec<u8>,
}

impl Object {
    /// Construct a new object from its kind and raw data.
    #[must_use]
    pub fn new(kind: ObjectKind, data: Vec<u8>) -> Self {
        Self { kind, data }
    }

    /// Serialize to the canonical Git object format: `"<kind> <size>\0<data>"`.
    #[must_use]
    pub fn to_store_bytes(&self) -> Vec<u8> {
        let header = format!("{} {}\0", self.kind, self.data.len());
        let mut out = Vec::with_capacity(header.len() + self.data.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.data);
        out
    }
}

/// A single entry in a Git tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Unix file mode (e.g. `0o100644` for a regular file, `0o040000` for a tree).
    pub mode: u32,
    /// Entry name (file or directory name only, no path separators).
    pub name: Vec<u8>,
    /// The object ID of the blob or sub-tree.
    pub oid: ObjectId,
}

impl TreeEntry {
    /// Format the mode as Git does: no leading zero, minimal digits.
    ///
    /// Git uses `"40000"` for trees (not `"040000"`), and `"100644"` for blobs.
    #[must_use]
    pub fn mode_str(&self) -> String {
        // Git omits the leading zero for tree mode
        if self.mode == 0o040000 {
            "40000".to_owned()
        } else {
            format!("{:o}", self.mode)
        }
    }
}

/// Parse the raw data of a tree object into its entries.
///
/// # Format
///
/// Each entry is `"<mode> <name>\0<20-byte-sha1>"` concatenated with no
/// separator between entries.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if the data is malformed.
pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>> {
    // A tree blob does not record its OID width; it is fixed by the repo's
    // hash algorithm. Rather than thread `HashAlgo` through ~140 call sites,
    // infer the width: a well-formed tree only parses cleanly (consuming the
    // whole buffer) with the correct OID length. Try SHA-1 (20) first, then
    // SHA-256 (32).
    match parse_tree_with_oid_len(data, HashAlgo::Sha1.len()) {
        Ok(entries) => Ok(entries),
        Err(sha1_err) => {
            parse_tree_with_oid_len(data, HashAlgo::Sha256.len()).map_err(|_| sha1_err)
        }
    }
}

/// Parse a tree blob assuming a fixed raw OID width (`oid_len` bytes).
///
/// Returns an error if the data does not parse cleanly into whole entries of
/// that width (used by [`parse_tree`] to detect the hash algorithm).
pub fn parse_tree_with_oid_len(data: &[u8], oid_len: usize) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        // Find the space separating mode from name
        let sp = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::CorruptObject("tree entry missing space".to_owned()))?;
        let mode_bytes = &data[pos..pos + sp];
        let mode = std::str::from_utf8(mode_bytes)
            .ok()
            .and_then(|s| u32::from_str_radix(s, 8).ok())
            .ok_or_else(|| {
                Error::CorruptObject(format!(
                    "invalid tree mode: {}",
                    String::from_utf8_lossy(mode_bytes)
                ))
            })?;
        pos += sp + 1;

        // Find the NUL separating name from the raw OID
        let nul = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::CorruptObject("tree entry missing NUL".to_owned()))?;
        let name = data[pos..pos + nul].to_vec();
        pos += nul + 1;

        if pos + oid_len > data.len() {
            return Err(Error::CorruptObject("tree entry truncated SHA".to_owned()));
        }
        let oid = ObjectId::from_bytes(&data[pos..pos + oid_len])?;
        pos += oid_len;

        entries.push(TreeEntry { mode, name, oid });
    }

    Ok(entries)
}

/// Build the raw bytes of a tree object from a slice of entries.
///
/// Entries **must** already be sorted in Git tree order (see [`tree_entry_cmp`])
/// before calling this function.
#[must_use]
pub fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(e.mode_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(e.oid.as_bytes());
    }
    out
}

/// Git's tree-entry sort comparator.
///
/// Trees are sorted byte-by-byte by `"<name>"` for blobs and `"<name>/"` for
/// sub-trees, so a directory `foo` sorts after a file `foo-bar` but before
/// `fooz`.  This matches `base_name_compare` in `tree.c`.
///
/// # Parameters
///
/// - `a_name`: name bytes of the first entry
/// - `a_is_tree`: whether the first entry is a sub-tree (`mode == 0o040000`)
/// - `b_name`: name bytes of the second entry
/// - `b_is_tree`: whether the second entry is a sub-tree
#[must_use]
pub fn tree_entry_cmp(
    a_name: &[u8],
    a_is_tree: bool,
    b_name: &[u8],
    b_is_tree: bool,
) -> std::cmp::Ordering {
    let a_trailer = if a_is_tree { b'/' } else { 0u8 };
    let b_trailer = if b_is_tree { b'/' } else { 0u8 };

    let min_len = a_name.len().min(b_name.len());
    let cmp = a_name[..min_len].cmp(&b_name[..min_len]);
    if cmp != std::cmp::Ordering::Equal {
        return cmp;
    }
    // Names share a prefix; compare the next character (or trailer).
    let ac = a_name.get(min_len).copied().unwrap_or(a_trailer);
    let bc = b_name.get(min_len).copied().unwrap_or(b_trailer);
    ac.cmp(&bc)
}

/// Parsed representation of a commit object.
#[derive(Debug, Clone)]
pub struct CommitData {
    /// The tree this commit points to.
    pub tree: ObjectId,
    /// Parent commit IDs (zero or more).
    pub parents: Vec<ObjectId>,
    /// Author field decoded to Unicode (using `encoding` when present, else UTF-8).
    pub author: String,
    /// Committer field decoded to Unicode.
    pub committer: String,
    /// Exact `author` header payload bytes as stored in the object (after `author `).
    ///
    /// Empty means treat [`Self::author`] as UTF-8 when serializing (new commits).
    pub author_raw: Vec<u8>,
    /// Exact `committer` header payload bytes as stored in the object.
    pub committer_raw: Vec<u8>,
    /// Optional encoding override (e.g. `"UTF-8"`).
    pub encoding: Option<String>,
    /// Commit message (everything after the blank line).
    pub message: String,
    /// Optional raw message bytes for non-UTF-8 commit messages.
    /// When set, `serialize_commit` uses these bytes instead of `message`.
    #[doc = "Optional raw message bytes for non-UTF-8 messages."]
    pub raw_message: Option<Vec<u8>>,
}

/// Parse the raw data of a commit object.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if required headers are missing.
pub fn parse_commit(data: &[u8]) -> Result<CommitData> {
    // Header lines are mostly ASCII; author/committer payloads may match the `encoding` header.
    // Continuation lines (leading SP) append to the previous header for author/committer, or are
    // skipped for multiline headers Git allows (`gpgsig`, `mergetag`, …).
    #[derive(Clone, Copy)]
    enum Continuation {
        Author,
        Committer,
        Multiline,
        Ignore,
    }

    let mut pos = 0usize;
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author_raw: Option<Vec<u8>> = None;
    let mut committer_raw: Option<Vec<u8>> = None;
    let mut encoding: Option<String> = None;
    let mut cont = Continuation::Ignore;

    while pos < data.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < data.len() && data[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &data[line_start..line_end];
        let after_nl = line_end.saturating_add(1);
        if line.is_empty() {
            let body = data.get(after_nl..).unwrap_or_default();
            let message = commit_encoding::decode_bytes(encoding.as_deref(), body);
            // Preserve the exact message tail: Git allows commits whose log ends without a
            // final newline (`commit-tree` from a file). `serialize_commit` appends `\n` when
            // only `message` is set, so keep raw bytes when the body is not LF-terminated.
            let has_non_utf8_encoding = encoding.as_deref().is_some_and(|label| {
                !label.eq_ignore_ascii_case("utf-8") && !label.eq_ignore_ascii_case("utf8")
            });
            let raw_message = if body.is_empty() {
                None
            } else if has_non_utf8_encoding
                || std::str::from_utf8(body).is_err()
                || !body.ends_with(b"\n")
            {
                Some(body.to_vec())
            } else {
                None
            };
            let author_bytes = author_raw
                .ok_or_else(|| Error::CorruptObject("commit missing author header".to_owned()))?;
            let committer_bytes = committer_raw.ok_or_else(|| {
                Error::CorruptObject("commit missing committer header".to_owned())
            })?;
            let author = commit_encoding::decode_bytes(encoding.as_deref(), &author_bytes);
            let committer = commit_encoding::decode_bytes(encoding.as_deref(), &committer_bytes);
            return Ok(CommitData {
                tree: tree
                    .ok_or_else(|| Error::CorruptObject("commit missing tree header".to_owned()))?,
                parents,
                author,
                committer,
                author_raw: author_bytes,
                committer_raw: committer_bytes,
                encoding,
                message,
                raw_message,
            });
        }

        if line.first() == Some(&b' ') {
            let rest = line.get(1..).unwrap_or_default();
            match cont {
                Continuation::Author => {
                    let a = author_raw.as_mut().ok_or_else(|| {
                        Error::CorruptObject("orphan header continuation".to_owned())
                    })?;
                    a.extend_from_slice(rest);
                }
                Continuation::Committer => {
                    let c = committer_raw.as_mut().ok_or_else(|| {
                        Error::CorruptObject("orphan header continuation".to_owned())
                    })?;
                    c.extend_from_slice(rest);
                }
                Continuation::Multiline | Continuation::Ignore => {}
            }
            pos = after_nl;
            continue;
        }

        let key_end = line
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::CorruptObject("malformed commit header line".to_owned()))?;
        let key = &line[..key_end];
        let rest = line.get(key_end + 1..).unwrap_or_default();

        match key {
            b"tree" => {
                let line_str = std::str::from_utf8(rest).map_err(|_| {
                    Error::CorruptObject("commit tree line is not valid UTF-8".to_owned())
                })?;
                tree = Some(line_str.trim().parse::<ObjectId>()?);
                cont = Continuation::Ignore;
            }
            b"parent" => {
                let line_str = std::str::from_utf8(rest).map_err(|_| {
                    Error::CorruptObject("commit parent line is not valid UTF-8".to_owned())
                })?;
                parents.push(line_str.trim().parse::<ObjectId>()?);
                cont = Continuation::Ignore;
            }
            b"author" => {
                author_raw = Some(rest.to_vec());
                cont = Continuation::Author;
            }
            b"committer" => {
                committer_raw = Some(rest.to_vec());
                cont = Continuation::Committer;
            }
            b"encoding" => {
                let line_str = std::str::from_utf8(rest).map_err(|_| {
                    Error::CorruptObject("commit encoding line is not valid UTF-8".to_owned())
                })?;
                encoding = Some(line_str.to_owned());
                cont = Continuation::Ignore;
            }
            _ => {
                cont = Continuation::Multiline;
            }
        }
        pos = after_nl;
    }

    Err(Error::CorruptObject(
        "commit missing blank line before message".to_owned(),
    ))
}

/// Value after `prefix` on the first header line that starts with `prefix`, scanning until a blank
/// line (Git tag headers). Returns `None` if no such line exists before the body.
#[must_use]
pub fn tag_header_field(data: &[u8], prefix: &[u8]) -> Option<String> {
    let mut pos = 0usize;
    while pos < data.len() {
        let rest = &data[pos..];
        let nl = rest.iter().position(|&b| b == b'\n');
        let line = if let Some(i) = nl { &rest[..i] } else { rest };
        if line.is_empty() {
            break;
        }
        if let Some(after) = line.strip_prefix(prefix) {
            return Some(String::from_utf8_lossy(after).trim().to_owned());
        }
        pos += line.len().saturating_add(nl.map(|_| 1).unwrap_or(0));
        if nl.is_none() {
            break;
        }
    }
    None
}

/// OID from the first `object <hex>` line in the tag header block, if hex parses.
#[must_use]
pub fn tag_object_line_oid(data: &[u8]) -> Option<ObjectId> {
    let s = tag_header_field(data, b"object ")?;
    s.parse().ok()
}

/// Parsed representation of an annotated tag object.
#[derive(Debug, Clone)]
pub struct TagData {
    /// The object this tag points to.
    pub object: ObjectId,
    /// The type of the tagged object (e.g. `"commit"`).
    pub object_type: String,
    /// The short tag name (without `refs/tags/` prefix).
    pub tag: String,
    /// The tagger identity and timestamp (raw Git format).
    pub tagger: Option<String>,
    /// The tag message (everything after the blank line).
    pub message: String,
}

/// Parse the raw data of a tag object.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if required headers are missing or malformed.
pub fn parse_tag(data: &[u8]) -> Result<TagData> {
    let text = std::str::from_utf8(data)
        .map_err(|_| Error::CorruptObject("tag is not valid UTF-8".to_owned()))?;

    let mut object = None;
    let mut object_type = None;
    let mut tag_name = None;
    let mut tagger = None;
    let mut message = String::new();
    let mut in_message = false;

    for line in text.split('\n') {
        if in_message {
            message.push_str(line);
            message.push('\n');
            continue;
        }
        if line.is_empty() {
            in_message = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("object ") {
            object = Some(rest.trim().parse::<ObjectId>()?);
        } else if let Some(rest) = line.strip_prefix("type ") {
            let typ = rest.trim();
            if ObjectKind::from_tag_type_field(typ.as_bytes()).is_none() {
                return Err(Error::CorruptObject(format!(
                    "invalid 'type' value in tag: {typ}"
                )));
            }
            object_type = Some(typ.to_owned());
        } else if let Some(rest) = line.strip_prefix("tag ") {
            tag_name = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("tagger ") {
            tagger = Some(rest.to_owned());
        }
    }

    // Strip one trailing newline that split adds
    if message.ends_with('\n') {
        message.pop();
    }

    Ok(TagData {
        object: object
            .ok_or_else(|| Error::CorruptObject("tag missing object header".to_owned()))?,
        object_type: object_type
            .ok_or_else(|| Error::CorruptObject("tag missing type header".to_owned()))?,
        tag: tag_name.ok_or_else(|| Error::CorruptObject("tag missing tag header".to_owned()))?,
        tagger,
        message,
    })
}

/// Serialize a [`TagData`] into the raw bytes suitable for storage as a tag object.
///
/// The caller is responsible for supplying a correctly-formatted `tagger` string
/// (including timestamp and timezone) when present.
#[must_use]
pub fn serialize_tag(t: &TagData) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!("object {}\n", t.object));
    out.push_str(&format!("type {}\n", t.object_type));
    out.push_str(&format!("tag {}\n", t.tag));
    if let Some(ref tagger) = t.tagger {
        out.push_str(&format!("tagger {tagger}\n"));
    }
    out.push('\n');
    // Only add message if non-empty (don't add extra blank line for empty message)
    let msg = t.message.trim_end_matches('\n');
    if !msg.is_empty() {
        out.push_str(msg);
        out.push('\n');
    }
    out.into_bytes()
}

/// Serialize a [`CommitData`] into the raw bytes suitable for storage.
///
/// The caller is responsible for supplying a correctly-formatted `author` and
/// `committer` string (including timestamp and timezone).
///
/// The message body is written exactly as given: `git commit` and `git commit-tree -m`
/// supply a trailing LF; `git commit-tree` reading from stdin or `-F` does not add one.
#[must_use]
pub fn serialize_commit(c: &CommitData) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("tree {}\n", c.tree).as_bytes());
    for p in &c.parents {
        out.extend_from_slice(format!("parent {p}\n").as_bytes());
    }
    out.extend_from_slice(b"author ");
    if c.author_raw.is_empty() {
        out.extend_from_slice(c.author.as_bytes());
    } else {
        out.extend_from_slice(&c.author_raw);
    }
    out.push(b'\n');
    out.extend_from_slice(b"committer ");
    if c.committer_raw.is_empty() {
        out.extend_from_slice(c.committer.as_bytes());
    } else {
        out.extend_from_slice(&c.committer_raw);
    }
    out.push(b'\n');
    if let Some(enc) = &c.encoding {
        out.extend_from_slice(format!("encoding {enc}\n").as_bytes());
    }
    out.push(b'\n');
    if let Some(raw) = &c.raw_message {
        out.extend_from_slice(raw);
    } else if !c.message.is_empty() {
        out.extend_from_slice(c.message.as_bytes());
    }
    out
}

#[cfg(test)]
mod commit_parse_tests {
    use super::*;

    #[test]
    fn parse_commit_skips_multiline_gpgsig_continuation() {
        let raw = concat!(
            "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
            "author A U Thor <author@example.com> 1 +0000\n",
            "committer C O Mitter <committer@example.com> 1 +0000\n",
            "gpgsig -----BEGIN PGP SIGNATURE-----\n",
            " abcdef\n",
            " -----END PGP SIGNATURE-----\n",
            "\n",
            "msg\n",
        );
        let c = parse_commit(raw.as_bytes()).expect("parse signed commit");
        assert_eq!(c.tree.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(c.message, "msg\n");
    }

    #[test]
    fn tree_entry_mode_str_and_tag_object_line_oid() {
        let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
        let entry = TreeEntry {
            mode: 0o040000,
            name: b"dir".to_vec(),
            oid,
        };
        assert_eq!(entry.mode_str(), "40000");
        let tag_body = format!("object {}\ntype commit\ntag t\n\n", oid.to_hex());
        assert_eq!(tag_object_line_oid(tag_body.as_bytes()), Some(oid));
    }

    #[test]
    fn parse_tree_with_sha256_oid_width() {
        let oid_bytes = [0x6eu8; 32];
        let oid = ObjectId::from_bytes(&oid_bytes).expect("oid");
        let mut raw = b"100644 file\0".to_vec();
        raw.extend_from_slice(oid.as_bytes());
        let entries = parse_tree_with_oid_len(&raw, HashAlgo::Sha256.len()).expect("tree");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, oid);
    }

    #[test]
    fn object_id_loose_path_helpers() {
        let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
        assert_eq!(oid.loose_prefix(), "4b");
        assert_eq!(oid.loose_suffix().len(), 38);
        assert!(ObjectId::is_hex_len(40));
        assert!(ObjectId::is_loose_suffix_len(38));
    }
}
