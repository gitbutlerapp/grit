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
///
/// # Examples
///
/// ```
/// use grit_lib::objects::ObjectId;
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let id = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")?;
/// assert_eq!(id.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
        if s.len() == HashAlgo::Sha1.hex_len() {
            return Self::from_hex_ascii(s.as_bytes());
        }
        s.parse()
    }

    /// Parse a fixed-width lowercase/uppercase ASCII hex OID (40 chars for SHA-1).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] when `bytes` is not exactly 40 hex digits.
    pub fn from_hex_ascii(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HashAlgo::Sha1.hex_len() {
            return Err(Error::InvalidObjectId(
                String::from_utf8_lossy(bytes).into_owned(),
            ));
        }
        let mut out = [0u8; 20];
        let (pairs, remainder) = bytes.as_chunks::<2>();
        for (i, chunk) in pairs.iter().enumerate() {
            out[i] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
        }
        if !remainder.is_empty() {
            return Err(Error::InvalidObjectId(
                String::from_utf8_lossy(bytes).into_owned(),
            ));
        }
        Self::from_bytes(&out)
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

impl std::hash::Hash for ObjectId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(self.as_bytes());
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

#[inline]
fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::InvalidObjectId(format!("invalid hex nibble: {b}"))),
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

/// Borrowed view of an object's kind and uncompressed payload.
///
/// Produced by [`crate::odb::Odb::read_into`](crate::odb::Odb::read_into) and
/// [`crate::odb::store::ObjectStore::read_into`](crate::odb::store::ObjectStore::read_into);
/// the payload bytes live in the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef<'a> {
    /// Object type after resolving any pack delta chain.
    pub kind: ObjectKind,
    /// Uncompressed payload (header stripped).
    pub data: &'a [u8],
}

impl<'a> ObjectRef<'a> {
    /// Borrow `buf` as an object payload with the given `kind`.
    #[must_use]
    pub fn from_buf(kind: ObjectKind, buf: &'a [u8]) -> Self {
        Self { kind, data: buf }
    }
}

impl Object {
    /// Construct a new object from its kind and raw data.
    #[must_use]
    pub fn new(kind: ObjectKind, data: Vec<u8>) -> Self {
        Self { kind, data }
    }

    /// Borrowed view of this object's kind and payload.
    #[must_use]
    pub fn as_ref(&self) -> ObjectRef<'_> {
        ObjectRef {
            kind: self.kind,
            data: self.data.as_slice(),
        }
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

/// A single tree entry borrowing name bytes from the tree object payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntryRef<'a> {
    /// Unix file mode (e.g. `0o100644` for a regular file, `0o040000` for a tree).
    pub mode: u32,
    /// Entry name (file or directory name only, no path separators).
    pub name: &'a [u8],
    /// The object ID of the blob or sub-tree.
    pub oid: ObjectId,
}

impl TreeEntryRef<'_> {
    /// Allocate an owning [`TreeEntry`] with the same mode, name, and OID.
    #[must_use]
    pub fn to_owned(&self) -> TreeEntry {
        TreeEntry {
            mode: self.mode,
            name: self.name.to_vec(),
            oid: self.oid,
        }
    }
}

/// Iterator over tree entries borrowing from the tree object's raw payload.
///
/// Construct with [`Self::with_oid_len`] when the hash width is known, or
/// [`Self::infer_oid_len`] to detect SHA-1 vs SHA-256 like [`parse_tree`].
#[derive(Debug, Clone)]
pub struct TreeRefIter<'a> {
    data: &'a [u8],
    pos: usize,
    oid_len: usize,
    /// Set after the first parse error so callers do not see endless `Err` items.
    failed: bool,
}

impl<'a> TreeRefIter<'a> {
    /// Iterate `data` assuming each entry ends with an OID of `oid_len` bytes.
    #[must_use]
    pub fn with_oid_len(data: &'a [u8], oid_len: usize) -> Self {
        Self {
            data,
            pos: 0,
            oid_len,
            failed: false,
        }
    }

    /// Detect OID width and return an iterator (SHA-1 first, then SHA-256).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when the blob does not parse as a tree.
    pub fn infer_oid_len(data: &'a [u8]) -> Result<Self> {
        match try_parse_tree_oid_len(data, HashAlgo::Sha1.len()) {
            Ok(()) => Ok(Self::with_oid_len(data, HashAlgo::Sha1.len())),
            Err(sha1_err) => match try_parse_tree_oid_len(data, HashAlgo::Sha256.len()) {
                Ok(()) => Ok(Self::with_oid_len(data, HashAlgo::Sha256.len())),
                Err(_) => Err(sha1_err),
            },
        }
    }

    fn next_entry(&mut self) -> Result<Option<TreeEntryRef<'a>>> {
        parse_tree_entry_at(self.data, &mut self.pos, self.oid_len)
    }
}

/// Parse one tree entry and advance `pos`. Returns `Ok(None)` when `pos` is at end of `data`.
fn parse_tree_entry_at<'a>(
    data: &'a [u8],
    pos: &mut usize,
    oid_len: usize,
) -> Result<Option<TreeEntryRef<'a>>> {
    if *pos >= data.len() {
        return Ok(None);
    }
    let sp = data[*pos..]
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject("tree entry missing space".to_owned()))?;
    let mode_bytes = &data[*pos..*pos + sp];
    let mode = std::str::from_utf8(mode_bytes)
        .ok()
        .and_then(|s| u32::from_str_radix(s, 8).ok())
        .ok_or_else(|| {
            Error::CorruptObject(format!(
                "invalid tree mode: {}",
                String::from_utf8_lossy(mode_bytes)
            ))
        })?;
    *pos += sp + 1;

    let nul = data[*pos..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("tree entry missing NUL".to_owned()))?;
    let name = &data[*pos..*pos + nul];
    *pos += nul + 1;

    if *pos + oid_len > data.len() {
        return Err(Error::CorruptObject("tree entry truncated SHA".to_owned()));
    }
    let oid = ObjectId::from_bytes(&data[*pos..*pos + oid_len])?;
    *pos += oid_len;

    Ok(Some(TreeEntryRef { mode, name, oid }))
}

impl<'a> Iterator for TreeRefIter<'a> {
    type Item = Result<TreeEntryRef<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.data.len() {
            return None;
        }
        match self.next_entry() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(err) => {
                self.failed = true;
                self.pos = self.data.len();
                Some(Err(err))
            }
        }
    }
}

/// Walk `data` once to verify it parses as a tree with the given OID width (no entry allocation).
fn try_parse_tree_oid_len(data: &[u8], oid_len: usize) -> Result<()> {
    let mut pos = 0usize;
    while parse_tree_entry_at(data, &mut pos, oid_len)?.is_some() {}
    Ok(())
}

fn collect_tree_with_oid_len(data: &[u8], oid_len: usize) -> Result<Vec<TreeEntry>> {
    let mut pos = 0usize;
    let mut entries = Vec::with_capacity(data.len() / 32);
    while let Some(entry) = parse_tree_entry_at(data, &mut pos, oid_len)? {
        entries.push(TreeEntry {
            mode: entry.mode,
            name: entry.name.to_vec(),
            oid: entry.oid,
        });
    }
    Ok(entries)
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
    match collect_tree_with_oid_len(data, HashAlgo::Sha1.len()) {
        Ok(entries) => Ok(entries),
        Err(sha1_err) => {
            collect_tree_with_oid_len(data, HashAlgo::Sha256.len()).map_err(|_| sha1_err)
        }
    }
}

/// Parse a tree blob assuming a fixed raw OID width (`oid_len` bytes).
///
/// Returns an error if the data does not parse cleanly into whole entries of
/// that width (used by [`parse_tree`] to detect the hash algorithm).
pub fn parse_tree_with_oid_len(data: &[u8], oid_len: usize) -> Result<Vec<TreeEntry>> {
    collect_tree_with_oid_len(data, oid_len)
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

/// Find a tree entry by name using Git's tree sort order (`O(log n)`).
#[must_use]
pub fn find_tree_entry_by_name<'a>(
    entries: &'a [TreeEntry],
    name: &[u8],
    name_is_tree: bool,
) -> Option<&'a TreeEntry> {
    let idx = entries
        .binary_search_by(|e| tree_entry_cmp(&e.name, e.mode == 0o040000, name, name_is_tree));
    idx.ok().and_then(|i| {
        let entry = &entries[i];
        if entry.name.as_slice() == name {
            Some(entry)
        } else {
            None
        }
    })
}

/// Like [`find_tree_entry_by_name`], trying directory ordering first then file.
#[must_use]
pub fn find_tree_entry_by_name_dir_or_file<'a>(
    entries: &'a [TreeEntry],
    name: &[u8],
) -> Option<&'a TreeEntry> {
    if let Some(entry) = find_tree_entry_by_name(entries, name, true) {
        if entry.mode == 0o040000 {
            return Some(entry);
        }
    }
    find_tree_entry_by_name(entries, name, false)
}

/// Parse an octal tree-entry mode without going through `str`.
fn parse_tree_mode_octal(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 7 {
        return None;
    }
    bytes.iter().try_fold(0u32, |acc, &b| match b {
        b'0'..=b'7' => Some(acc * 8 + u32::from(b - b'0')),
        _ => None,
    })
}

/// Find one tree entry by name without building a full [`TreeEntry`] vector.
///
/// Scans entries in stored order and returns the first whose name equals `name`
/// (file or directory). Because Git sorts entries by `name` (with a `/` trailer for
/// directories), the scan stops at the first entry that sorts after `name/`, so only
/// the prefix of the tree up to the match is decoded; corruption after that point is
/// not reported.
///
/// # Errors
///
/// [`Error::CorruptObject`] when an entry scanned before the stop point is malformed.
pub(crate) fn find_tree_entry_by_name_in_data(
    data: &[u8],
    oid_len: usize,
    name: &[u8],
) -> Result<Option<TreeEntry>> {
    let mut pos = 0usize;
    while pos < data.len() {
        let rest = &data[pos..];
        let sp = memchr::memchr(b' ', rest)
            .ok_or_else(|| Error::CorruptObject("tree entry missing space".to_owned()))?;
        let mode = parse_tree_mode_octal(&rest[..sp])
            .ok_or_else(|| Error::CorruptObject("invalid tree mode".to_owned()))?;
        let name_start = sp + 1;
        let nul = memchr::memchr(0, &rest[name_start..])
            .ok_or_else(|| Error::CorruptObject("tree entry missing NUL".to_owned()))?;
        let entry_name = &rest[name_start..name_start + nul];
        let oid_start = name_start + nul + 1;
        let oid_end = oid_start + oid_len;
        if oid_end > rest.len() {
            return Err(Error::CorruptObject("tree entry truncated SHA".to_owned()));
        }
        if entry_name == name {
            return Ok(Some(TreeEntry {
                mode,
                name: entry_name.to_vec(),
                oid: ObjectId::from_bytes(&rest[oid_start..oid_end])?,
            }));
        }
        if tree_entry_cmp(entry_name, mode == 0o040000, name, true) == std::cmp::Ordering::Greater {
            return Ok(None);
        }
        pos += oid_end;
    }
    Ok(None)
}

/// Parsed representation of a commit object.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Raw bytes for headers not represented in the fields above (`gpgsig`, `mergetag`, …),
    /// including each line's trailing `\n` and any continuation lines (leading space).
    ///
    /// [`serialize_commit`] always emits `tree`, `parent`, `author`, `committer`, and `encoding`
    /// from the structured fields, then appends these bytes before the blank line and message.
    pub extra_headers: Vec<u8>,
}

/// Parse commit headers (tree, parents, author, committer) and skip the message body.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if required headers are missing.
/// First non-blank line of a commit object's message (header scan only).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the commit headers are malformed.
pub fn read_commit_subject_line(data: &[u8]) -> Result<String> {
    let mut pos = 0usize;
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
            let message = std::str::from_utf8(body).unwrap_or("");
            return Ok(message
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(str::trim)
                .unwrap_or("(no subject)")
                .to_owned());
        }
        pos = after_nl;
    }
    Err(Error::CorruptObject(
        "commit missing blank line before message".to_owned(),
    ))
}

pub(crate) fn parse_oid_field(value: &[u8]) -> Result<ObjectId> {
    let s = std::str::from_utf8(value)
        .map_err(|_| Error::CorruptObject("commit header object id is not UTF-8".to_owned()))?;
    s.trim()
        .parse::<ObjectId>()
        .map_err(|_| Error::CorruptObject("invalid object id in commit header".to_owned()))
}

/// Parse only `tree` and `parent` headers from a commit object (stops at the blank line).
///
/// Used on hot paths such as blame history walks that do not need author/committer data.
pub(crate) fn parse_commit_tree_and_parents(data: &[u8]) -> Result<(ObjectId, Vec<ObjectId>)> {
    let commit = super::CommitRef::parse(data)?;
    Ok((commit.tree(), commit.parents().to_vec()))
}

/// Parse the raw data of a commit object.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if required headers are missing.
pub fn parse_commit(data: &[u8]) -> Result<CommitData> {
    Ok(super::CommitRef::parse(data)?.to_owned())
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
    /// Extra header lines after `tagger` and before the blank line (raw bytes, LF-terminated).
    pub extra_headers: Vec<u8>,
    /// The tag message (everything after the blank line).
    pub message: String,
}

/// Parse the raw data of a tag object.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if required headers are missing or malformed.
pub fn parse_tag(data: &[u8]) -> Result<TagData> {
    use super::extra_headers::next_header_block;

    #[derive(Clone, Copy)]
    enum Continuation {
        None,
        Extra,
    }

    let mut pos = 0usize;
    let mut object = None;
    let mut object_type = None;
    let mut tag_name = None;
    let mut tagger = None;
    let mut extra_headers = Vec::new();
    let mut cont = Continuation::None;
    let mut past_tagger = false;

    while pos < data.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < data.len() && data[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &data[line_start..line_end];
        let after_nl = (line_end + 1).min(data.len());
        if line.is_empty() {
            let body = data.get(after_nl..).unwrap_or_default();
            let message = std::str::from_utf8(body)
                .map_err(|_| Error::CorruptObject("tag message is not valid UTF-8".to_owned()))?
                .to_owned();
            return Ok(TagData {
                object: object
                    .ok_or_else(|| Error::CorruptObject("tag missing object header".to_owned()))?,
                object_type: object_type
                    .ok_or_else(|| Error::CorruptObject("tag missing type header".to_owned()))?,
                tag: tag_name
                    .ok_or_else(|| Error::CorruptObject("tag missing tag header".to_owned()))?,
                tagger,
                extra_headers,
                message,
            });
        }

        if past_tagger {
            let (_, block_end, _) = next_header_block(data, line_start);
            extra_headers.extend_from_slice(&data[line_start..block_end]);
            pos = block_end;
            continue;
        }

        if line.first() == Some(&b' ') {
            match cont {
                Continuation::Extra => {
                    extra_headers.extend_from_slice(&data[line_start..after_nl]);
                }
                Continuation::None => {}
            }
            pos = after_nl;
            continue;
        }

        let key_end = line
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::CorruptObject("malformed tag header line".to_owned()))?;
        let key = &line[..key_end];
        let rest = line.get(key_end + 1..).unwrap_or_default();

        match key {
            b"object" => {
                let line_str = std::str::from_utf8(rest).map_err(|_| {
                    Error::CorruptObject("tag object line is not valid UTF-8".to_owned())
                })?;
                object = Some(line_str.trim().parse::<ObjectId>()?);
                cont = Continuation::None;
            }
            b"type" => {
                let typ = std::str::from_utf8(rest).map_err(|_| {
                    Error::CorruptObject("tag type line is not valid UTF-8".to_owned())
                })?;
                let typ = typ.trim();
                if ObjectKind::from_tag_type_field(typ.as_bytes()).is_none() {
                    return Err(Error::CorruptObject(format!(
                        "invalid 'type' value in tag: {typ}"
                    )));
                }
                object_type = Some(typ.to_owned());
                cont = Continuation::None;
            }
            b"tag" => {
                let line_str = std::str::from_utf8(rest).map_err(|_| {
                    Error::CorruptObject("tag name line is not valid UTF-8".to_owned())
                })?;
                tag_name = Some(line_str.trim().to_owned());
                cont = Continuation::None;
            }
            b"tagger" => {
                tagger = Some(
                    std::str::from_utf8(rest)
                        .map_err(|_| {
                            Error::CorruptObject("tag tagger line is not valid UTF-8".to_owned())
                        })?
                        .to_owned(),
                );
                past_tagger = true;
                cont = Continuation::None;
            }
            _ => {
                cont = Continuation::Extra;
                extra_headers.extend_from_slice(&data[line_start..after_nl]);
            }
        }
        pos = after_nl;
    }

    Err(Error::CorruptObject(
        "tag missing blank line before message".to_owned(),
    ))
}

/// Serialize a [`TagData`] into the raw bytes suitable for storage as a tag object.
///
/// The caller is responsible for supplying a correctly-formatted `tagger` string
/// (including timestamp and timezone) when present.
#[must_use]
pub fn serialize_tag(t: &TagData) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("object {}\n", t.object).as_bytes());
    out.extend_from_slice(format!("type {}\n", t.object_type).as_bytes());
    out.extend_from_slice(format!("tag {}\n", t.tag).as_bytes());
    if let Some(ref tagger) = t.tagger {
        out.extend_from_slice(format!("tagger {tagger}\n").as_bytes());
    }
    out.extend_from_slice(&t.extra_headers);
    out.push(b'\n');
    let msg = t.message.trim_end_matches('\n');
    if !msg.is_empty() {
        out.extend_from_slice(msg.as_bytes());
        out.push(b'\n');
    }
    out
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
    out.extend_from_slice(&c.extra_headers);
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
    fn commit_ref_matches_parse_commit_headers() {
        let raw = concat!(
            "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
            "parent 1111111111111111111111111111111111111111\n",
            "author A <a@example.com> 1 +0000\n",
            "committer C <c@example.com> 2 +0000\n",
            "\n",
            "ignored body\n",
        );
        let full = parse_commit(raw.as_bytes()).expect("parse");
        let link = crate::objects::CommitRef::parse(raw.as_bytes()).expect("CommitRef");
        assert_eq!(link.tree(), full.tree);
        assert_eq!(link.parents(), full.parents.as_slice());
        assert_eq!(link.author(), full.author);
        assert_eq!(link.committer(), full.committer);
    }

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
        assert_eq!(serialize_commit(&c), raw.as_bytes());
    }

    #[test]
    fn serialize_commit_applies_public_field_edits_after_parse() {
        let raw = concat!(
            "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
            "author A <a@example.com> 1 +0000\n",
            "committer C <c@example.com> 1 +0000\n",
            "\n",
            "msg\n",
        );
        let mut commit = parse_commit(raw.as_bytes()).expect("parse");
        let zero = ObjectId::from_hex("0000000000000000000000000000000000000000").unwrap();
        commit.tree = zero;
        let serialized = serialize_commit(&commit);
        let tree_line = format!("tree {}\n", zero.to_hex());
        assert!(
            serialized.starts_with(tree_line.as_bytes()),
            "serialize_commit ignored the updated public tree field"
        );
    }

    #[test]
    fn parse_commit_preserves_gpgsig_mergetag_bytes_exactly() {
        let raw = concat!(
            "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
            "author A <a@example.com> 1 +0000\n",
            "committer C <c@example.com> 1 +0000\n",
            "encoding ISO-8859-1\n",
            "gpgsig -----BEGIN PGP SIGNATURE-----\n",
            " line2\n",
            " -----END PGP SIGNATURE-----\n",
            "mergetag object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
            " type commit\n",
            " tag merge-tag\n",
            " tagger T <t@example.com> 1 +0000\n",
            " -----BEGIN PGP SIGNATURE-----\n",
            " inner\n",
            " -----END PGP SIGNATURE-----\n",
            "\n",
            "msg-only\n",
        );
        let parsed = parse_commit(raw.as_bytes()).expect("parse");
        assert_eq!(serialize_commit(&parsed), raw.as_bytes());
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
    fn parse_commit_non_commit_bytes_does_not_panic() {
        let tree_bytes = b"100644 blob 0123456789012345678901234567890123456789\tf";
        let err = parse_commit(tree_bytes).expect_err("tree bytes are not a commit");
        assert!(matches!(err, Error::CorruptObject(_)));
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

    #[test]
    fn tree_ref_iter_yields_at_most_one_error() {
        let bad = b"100644 incomplete";
        let mut iter = TreeRefIter::with_oid_len(bad, HashAlgo::Sha1.len());
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }

    #[test]
    fn tree_ref_iter_public_api_smoke() {
        let data = serialize_tree(&[TreeEntry {
            mode: 0o100644,
            name: b"a".to_vec(),
            oid: ObjectId::from_hex("0123456789012345678901234567890123456789").unwrap(),
        }]);
        let mut iter = TreeRefIter::infer_oid_len(&data).expect("infer");
        let entry = iter.next().unwrap().expect("entry");
        assert_eq!(entry.name, b"a");
        let _owned = entry.to_owned();
        let _vec = parse_tree(&data).expect("parse");
        let _ = TreeRefIter::with_oid_len(&data, HashAlgo::Sha1.len());
    }

    #[test]
    fn find_tree_entry_by_name_in_data_locates_directory_in_large_tree() -> Result<()> {
        let dummy = ObjectId::from_hex("0123456789012345678901234567890123456789")?;
        let mut entries: Vec<TreeEntry> = (1..=60)
            .map(|i| TreeEntry {
                mode: 0o100644,
                name: format!("f{i}.txt").into_bytes(),
                oid: dummy,
            })
            .collect();
        entries.push(TreeEntry {
            mode: 0o040000,
            name: b"sub".to_vec(),
            oid: dummy,
        });
        entries.sort_by(|a, b| {
            tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000)
        });
        let data = serialize_tree(&entries);
        assert!(entries.len() >= 48);
        let found = find_tree_entry_by_name_in_data(&data, HashAlgo::Sha1.len(), b"sub")?
            .expect("sub directory in wide tree");
        assert_eq!(found.mode, 0o040000);
        assert_eq!(found.name, b"sub");
        Ok(())
    }

    fn sorted_tree_data(entries: &mut [TreeEntry]) -> Vec<u8> {
        entries.sort_by(|a, b| {
            tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000)
        });
        serialize_tree(entries)
    }

    #[test]
    fn find_tree_entry_by_name_in_data_matches_parse_tree_for_every_name() -> Result<()> {
        let oid_a = ObjectId::from_hex("1111111111111111111111111111111111111111")?;
        let oid_b = ObjectId::from_hex("2222222222222222222222222222222222222222")?;
        let mut entries = vec![
            TreeEntry {
                mode: 0o040000,
                name: b"foo".to_vec(),
                oid: oid_a,
            },
            TreeEntry {
                mode: 0o100644,
                name: b"foo.c".to_vec(),
                oid: oid_b,
            },
            TreeEntry {
                mode: 0o100755,
                name: b"foo-bar".to_vec(),
                oid: oid_b,
            },
            TreeEntry {
                mode: 0o120000,
                name: b"link".to_vec(),
                oid: oid_a,
            },
            TreeEntry {
                mode: 0o160000,
                name: b"sub".to_vec(),
                oid: oid_b,
            },
            TreeEntry {
                mode: 0o100644,
                name: b"README".to_vec(),
                oid: oid_a,
            },
        ];
        let data = sorted_tree_data(&mut entries);
        for expected in parse_tree(&data)? {
            let found =
                find_tree_entry_by_name_in_data(&data, HashAlgo::Sha1.len(), &expected.name)?
                    .expect("entry present");
            assert_eq!(found, expected);
        }
        for missing in [&b"fo"[..], b"foo/", b"foo.d", b"zzz", b"", b"AAA", b"lin"] {
            assert!(
                find_tree_entry_by_name_in_data(&data, HashAlgo::Sha1.len(), missing)?.is_none(),
                "unexpected match for {:?}",
                String::from_utf8_lossy(missing)
            );
        }
        Ok(())
    }

    #[test]
    fn find_tree_entry_by_name_in_data_stops_after_sorted_position() -> Result<()> {
        let oid = ObjectId::from_hex("3333333333333333333333333333333333333333")?;
        let mut entries = vec![
            TreeEntry {
                mode: 0o100644,
                name: b"a".to_vec(),
                oid,
            },
            TreeEntry {
                mode: 0o100644,
                name: b"c".to_vec(),
                oid,
            },
        ];
        let mut data = sorted_tree_data(&mut entries);
        data.extend_from_slice(b"garbage without separators");
        let found = find_tree_entry_by_name_in_data(&data, HashAlgo::Sha1.len(), b"a")?;
        assert_eq!(found.map(|e| e.name), Some(b"a".to_vec()));
        assert!(find_tree_entry_by_name_in_data(&data, HashAlgo::Sha1.len(), b"b")?.is_none());
        assert!(matches!(
            find_tree_entry_by_name_in_data(&data, HashAlgo::Sha1.len(), b"d"),
            Err(Error::CorruptObject(_))
        ));
        Ok(())
    }

    #[test]
    fn find_tree_entry_by_name_in_data_rejects_bad_mode_and_truncated_oid() {
        let bad_mode = b"10x644 a\0aaaaaaaaaaaaaaaaaaaa";
        assert!(matches!(
            find_tree_entry_by_name_in_data(bad_mode, HashAlgo::Sha1.len(), b"a"),
            Err(Error::CorruptObject(_))
        ));
        let truncated = b"100644 a\0short";
        assert!(matches!(
            find_tree_entry_by_name_in_data(truncated, HashAlgo::Sha1.len(), b"a"),
            Err(Error::CorruptObject(_))
        ));
    }

    #[test]
    fn find_tree_entry_by_name_in_data_reads_sha256_width() -> Result<()> {
        let oid = ObjectId::from_bytes(&[0x5au8; 32])?;
        let mut raw = b"100644 file\0".to_vec();
        raw.extend_from_slice(oid.as_bytes());
        let found =
            find_tree_entry_by_name_in_data(&raw, HashAlgo::Sha256.len(), b"file")?.expect("file");
        assert_eq!(found.oid, oid);
        assert_eq!(found.mode, 0o100644);
        Ok(())
    }
}
