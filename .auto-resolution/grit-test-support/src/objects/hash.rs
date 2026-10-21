//! Object hash algorithm selection for pack fixtures.

/// Hash algorithm used when sealing pack trailers and naming object ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// 160-bit SHA-1 object ids (Git default).
    Sha1,
    /// 256-bit SHA-256 object ids (`--object-format=sha256`).
    Sha256,
}

impl HashAlgo {
    /// Length of an object id for this algorithm, in bytes.
    #[must_use]
    pub const fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    /// Digest the pack prefix (everything before the trailer hash).
    pub(crate) fn digest_pack(&self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => {
                use sha1::{Digest, Sha1};
                Sha1::digest(data).to_vec()
            }
            Self::Sha256 => {
                use sha2::{Digest, Sha256};
                Sha256::digest(data).to_vec()
            }
        }
    }

    /// Git `init` / `hash-object` object format name.
    #[must_use]
    pub const fn git_object_format(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
}
