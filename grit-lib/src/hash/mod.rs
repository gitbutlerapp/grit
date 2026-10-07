//! Incremental hashing for Git object ids and file trailers.
//!
//! Parallel batch helpers: [`hash_objects_parallel`], [`par_hash_with`], and [`Parallelism`].
//!
//! This module is the single owner of the `sha1` and `sha2` crates in
//! `grit-lib`. Callers hash canonical Git object bytes (`"<kind> <len>\\0<payload>"`)
//! via [`HashAlgo::hash_object`] / [`ObjectHasher::for_object`], raw payloads via
//! [`HashAlgo::digest`], and checksummed file bodies via [`HashingWriter`] /
//! [`verify_trailer`]. Push certificate nonces use [`hmac_sha1`].

use std::io::{self, Write};

use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};

mod parallel;

pub use parallel::{
    hash_objects_parallel, par_hash_with, parallel_hash_worthwhile, try_par_hash_with,
    ParallelHashError, Parallelism, PAR_HASH_MIN_ITEMS, PAR_HASH_MIN_TOTAL_BYTES,
};

/// Which implementation the `sha1` / `sha2` dependency selects for `algo` on this CPU.
///
/// Values mirror the `cpufeatures` dispatch inside those crates for the digests
/// Grit uses (SHA-1 and SHA-256). On x86_64, SHA-256 in `sha2` 0.11 only selects
/// SHA-NI or portable code (AVX2 is used for SHA-512, not SHA-256, so there is no
/// separate AVX2 backend variant here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// x86_64 SHA-NI (`sha` target feature) — used for SHA-1 and SHA-256 when available.
    X86ShaNi,
    /// AArch64 SHA2 crypto extensions.
    Aarch64Sha,
    /// Portable software implementation.
    Portable,
}

impl Backend {
    /// Stable lowercase name for scripts and JSON (`x86_sha_ni`, `portable`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86ShaNi => "x86_sha_ni",
            Self::Aarch64Sha => "aarch64_sha",
            Self::Portable => "portable",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Report the hashing backend the linked `sha1` / `sha2` crates would use for `algo`.
///
/// Detection uses the same target features as those crates (`sha` on x86_64, `sha2` on
/// AArch64). `algo` is accepted for a stable signature; both supported algorithms share
/// the same backend on a given CPU.
#[must_use]
pub fn backend(algo: HashAlgo) -> Backend {
    let _ = algo;
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sha") {
            Backend::X86ShaNi
        } else {
            Backend::Portable
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("sha2") {
            Backend::Aarch64Sha
        } else {
            Backend::Portable
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Backend::Portable
    }
}
/// RFC 2104 HMAC block size for SHA-1.
const HMAC_BLOCK_SIZE: usize = 64;

/// Maximum length of a Git object header (`"<kind> <decimal-len>\\0"`) on the stack.
///
/// Longest kind name is `"commit"` (6 bytes) plus space, up to 20 decimal digits
/// for the payload length, plus the trailing NUL.
const OBJECT_HEADER_BUF_LEN: usize = 6 + 1 + 20 + 1;

/// Incremental hasher for Git's supported object-id algorithms.
#[derive(Debug, Clone)]
pub enum ObjectHasher {
    /// SHA-1 (20-byte digests).
    Sha1(Sha1),
    /// SHA-256 (32-byte digests).
    Sha256(Sha256),
}

impl ObjectHasher {
    /// Create a fresh hasher for `algo`.
    #[must_use]
    pub fn new(algo: HashAlgo) -> Self {
        algo.hasher()
    }

    /// The hash algorithm this hasher produces.
    #[must_use]
    pub fn algo(self: &ObjectHasher) -> HashAlgo {
        match self {
            Self::Sha1(_) => HashAlgo::Sha1,
            Self::Sha256(_) => HashAlgo::Sha256,
        }
    }

    /// Feed more bytes into the digest.
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
        }
    }

    /// Create a hasher that already consumed the Git object header for `kind` and `len`.
    ///
    /// The header is written into a stack buffer (no heap allocation).
    #[must_use]
    pub fn for_object(algo: HashAlgo, kind: ObjectKind, len: u64) -> Self {
        let mut hasher = Self::new(algo);
        let mut header = [0u8; OBJECT_HEADER_BUF_LEN];
        let header_len = write_object_header(&mut header, kind, len);
        hasher.update(&header[..header_len]);
        hasher
    }

    /// Finish hashing and return the object id.
    #[must_use]
    pub fn finalize(self) -> ObjectId {
        match self {
            Self::Sha1(h) => ObjectId::from_bytes(h.finalize().as_slice())
                .unwrap_or_else(|_| unreachable!("SHA-1 digest is 20 bytes")),
            Self::Sha256(h) => ObjectId::from_bytes(h.finalize().as_slice())
                .unwrap_or_else(|_| unreachable!("SHA-256 digest is 32 bytes")),
        }
    }

    /// Write the raw digest into `out` and return the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] when `out` is shorter than the digest
    /// length for this algorithm (a diagnostic string names the required size).
    pub fn finalize_into(self, out: &mut [u8]) -> Result<usize> {
        match self {
            Self::Sha1(h) => {
                let digest = h.finalize();
                let len = digest.len();
                if out.len() < len {
                    return Err(Error::InvalidObjectId(format!(
                        "digest buffer too short: need {len} bytes, have {}",
                        out.len()
                    )));
                }
                out[..len].copy_from_slice(&digest);
                Ok(len)
            }
            Self::Sha256(h) => {
                let digest = h.finalize();
                let len = digest.len();
                if out.len() < len {
                    return Err(Error::InvalidObjectId(format!(
                        "digest buffer too short: need {len} bytes, have {}",
                        out.len()
                    )));
                }
                out[..len].copy_from_slice(&digest);
                Ok(len)
            }
        }
    }
}

impl Write for ObjectHasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HashAlgo {
    /// Return a fresh incremental hasher for this algorithm.
    #[must_use]
    pub fn hasher(self) -> ObjectHasher {
        match self {
            Self::Sha1 => ObjectHasher::Sha1(Sha1::new()),
            Self::Sha256 => ObjectHasher::Sha256(Sha256::new()),
        }
    }

    /// Hash a byte slice with no Git object header.
    #[must_use]
    pub fn digest(self, data: &[u8]) -> ObjectId {
        let mut hasher = self.hasher();
        hasher.update(data);
        hasher.finalize()
    }

    /// Hash canonical Git store bytes for `kind` and `data`.
    #[must_use]
    pub fn hash_object(self, kind: ObjectKind, data: &[u8]) -> ObjectId {
        hash_object(self, kind, data)
    }
}

/// Hash canonical Git store bytes for `kind` and `data`.
///
/// Equivalent to [`HashAlgo::hash_object`] but accepts the algorithm as the
/// first argument.
#[must_use]
pub fn hash_object(algo: HashAlgo, kind: ObjectKind, data: &[u8]) -> ObjectId {
    let len = u64::try_from(data.len()).unwrap_or(u64::MAX); // clamp on platforms where usize exceeds u64::MAX
    let mut hasher = ObjectHasher::for_object(algo, kind, len);
    hasher.update(data);
    hasher.finalize()
}

/// A writer that forwards bytes to an inner sink while accumulating a checksum.
///
/// Call [`Self::finish`] to append the raw digest trailer to the inner writer
/// and obtain the digest as an [`ObjectId`].
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: ObjectHasher,
}

impl<W: Write> HashingWriter<W> {
    /// Wrap `inner`, hashing every byte written through this adapter.
    #[must_use]
    pub fn new(algo: HashAlgo, inner: W) -> Self {
        Self {
            inner,
            hasher: algo.hasher(),
        }
    }

    /// Borrow the inner writer without finishing the checksum.
    #[must_use]
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Mutably borrow the inner writer without finishing the checksum.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Append the checksum trailer to the inner writer and return it with the digest.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors from writing the trailer to `inner`.
    pub fn finish(mut self) -> io::Result<(W, ObjectId)> {
        let digest = self.hasher.finalize();
        self.inner.write_all(digest.as_bytes())?;
        Ok((self.inner, digest))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Checksum at the end of `bytes` did not match a digest of the leading body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailerMismatch {
    /// Digest computed over the body (everything before the trailer).
    pub expected: Vec<u8>,
    /// Trailing bytes found in `bytes`.
    pub found: Vec<u8>,
}

impl std::fmt::Display for TrailerMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "trailer checksum mismatch (expected {}, found {})",
            hex::encode(&self.expected),
            hex::encode(&self.found)
        )
    }
}

impl std::error::Error for TrailerMismatch {}

/// Verify that `bytes` ends with the hash of the preceding body.
///
/// # Errors
///
/// Returns [`TrailerMismatch`] when the trailer does not match, or when `bytes`
/// is shorter than one digest for `algo`.
pub fn verify_trailer(algo: HashAlgo, bytes: &[u8]) -> std::result::Result<(), TrailerMismatch> {
    let trailer_len = algo.len();
    if bytes.len() < trailer_len {
        return Err(TrailerMismatch {
            expected: Vec::new(),
            found: bytes.to_vec(),
        });
    }
    let split = bytes.len() - trailer_len;
    let (body, trailer) = bytes.split_at(split);
    let expected = algo.digest(body);
    if expected.as_bytes() == trailer {
        Ok(())
    } else {
        Err(TrailerMismatch {
            expected: expected.as_bytes().to_vec(),
            found: trailer.to_vec(),
        })
    }
}

/// RFC 2104 HMAC-SHA1 (Git push certificate nonces and related signing).
///
/// `key_in` is the HMAC key material; `text` is the authenticated message.
/// Returns the 20-byte authenticator.
#[must_use]
pub fn hmac_sha1(key_in: &[u8], text: &[u8]) -> [u8; 20] {
    let mut key = [0u8; HMAC_BLOCK_SIZE];
    if key_in.len() > HMAC_BLOCK_SIZE {
        let mut hasher = Sha1::new();
        hasher.update(key_in);
        let digest = hasher.finalize();
        key[..20].copy_from_slice(&digest);
    } else {
        key[..key_in.len()].copy_from_slice(key_in);
    }

    let mut k_ipad = [0u8; HMAC_BLOCK_SIZE];
    let mut k_opad = [0u8; HMAC_BLOCK_SIZE];
    for i in 0..HMAC_BLOCK_SIZE {
        k_ipad[i] = key[i] ^ 0x36;
        k_opad[i] = key[i] ^ 0x5c;
    }

    let mut inner = Sha1::new();
    inner.update(k_ipad);
    inner.update(text);
    let inner_digest = inner.finalize();

    let mut outer = Sha1::new();
    outer.update(k_opad);
    outer.update(inner_digest);
    let outer_digest = outer.finalize();

    let mut out = [0u8; 20];
    out.copy_from_slice(&outer_digest);
    out
}

/// Write `"<kind> <len>\\0"` into `buf` and return the number of bytes used.
fn write_object_header(buf: &mut [u8], kind: ObjectKind, len: u64) -> usize {
    let kind_bytes = kind.as_str().as_bytes();
    let mut pos = 0;
    buf[pos..pos + kind_bytes.len()].copy_from_slice(kind_bytes);
    pos += kind_bytes.len();
    buf[pos] = b' ';
    pos += 1;
    pos += write_decimal(&mut buf[pos..], len);
    buf[pos] = 0;
    pos + 1
}

fn write_decimal(out: &mut [u8], mut n: u64) -> usize {
    if n == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut digits = 0usize;
    while n > 0 {
        tmp[digits] = b'0' + (n % 10) as u8;
        digits += 1;
        n /= 10;
    }
    for i in 0..digits {
        out[i] = tmp[digits - 1 - i];
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hex_oid(_algo: HashAlgo, hex: &str) -> ObjectId {
        ObjectId::from_hex(hex).expect("test vector hex")
    }

    fn assert_digest(algo: HashAlgo, data: &[u8], expected_hex: &str) {
        let got = algo.digest(data);
        assert_eq!(got, hex_oid(algo, expected_hex), "digest mismatch");
    }

    #[test]
    fn sha1_nist_vectors() {
        assert_digest(
            HashAlgo::Sha1,
            b"",
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
        );
        assert_digest(
            HashAlgo::Sha1,
            b"abc",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
        );
        assert_digest(
            HashAlgo::Sha1,
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_digest(
            HashAlgo::Sha1,
            &million_a,
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f",
        );
    }

    #[test]
    fn sha256_nist_vectors() {
        assert_digest(
            HashAlgo::Sha256,
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_digest(
            HashAlgo::Sha256,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
        assert_digest(
            HashAlgo::Sha256,
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_digest(
            HashAlgo::Sha256,
            &million_a,
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
        );
    }

    #[test]
    fn incremental_matches_oneshot_random_splits() {
        let data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        for algo in [HashAlgo::Sha1, HashAlgo::Sha256] {
            let oneshot = algo.digest(&data);
            let mut state = 0xdead_beef_u64;
            let mut pos = 0usize;
            let mut hasher = algo.hasher();
            while pos < data.len() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let remaining = data.len() - pos;
                let chunk = if remaining == 1 {
                    1
                } else {
                    (state as usize % remaining).max(1)
                };
                hasher.update(&data[pos..pos + chunk]);
                pos += chunk;
            }
            assert_eq!(hasher.finalize(), oneshot);
        }
    }

    #[test]
    fn object_header_encoding() {
        let kinds = [
            (ObjectKind::Blob, "blob"),
            (ObjectKind::Tree, "tree"),
            (ObjectKind::Commit, "commit"),
            (ObjectKind::Tag, "tag"),
        ];
        let lens = [0_u64, 1, 4_294_967_297];
        let cases: Vec<(ObjectKind, u64, String)> = kinds
            .iter()
            .flat_map(|&(kind, name)| {
                lens.iter()
                    .map(move |&len| (kind, len, format!("{name} {len}\0")))
            })
            .collect();
        for (kind, len, expect) in &cases {
            let mut buf = [0u8; OBJECT_HEADER_BUF_LEN];
            let n = write_object_header(&mut buf, *kind, *len);
            assert_eq!(
                &buf[..n],
                expect.as_bytes(),
                "header for {kind:?} len {len}"
            );
        }
    }

    #[test]
    fn hash_object_all_kinds() {
        for kind in [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::Tag,
        ] {
            for payload in [b"" as &[u8], b"x"] {
                let mut hasher =
                    ObjectHasher::for_object(HashAlgo::Sha1, kind, payload.len() as u64);
                hasher.update(payload);
                assert_eq!(
                    hash_object(HashAlgo::Sha1, kind, payload),
                    hasher.finalize()
                );
            }
        }
    }

    /// Inner writer that accepts at most `max` bytes per `write` call.
    struct PartialWrite {
        max: usize,
        buf: Vec<u8>,
        fail: bool,
    }

    impl Write for PartialWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "simulated write failure",
                ));
            }
            let n = buf.len().min(self.max);
            self.buf.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn hashing_writer_partial_write_checksum() {
        let inner = PartialWrite {
            max: 2,
            buf: Vec::new(),
            fail: false,
        };
        let mut w = HashingWriter::new(HashAlgo::Sha1, inner);
        let n = w.write(b"abcdef").expect("write");
        assert_eq!(n, 2);
        assert_eq!(w.get_ref().buf, b"ab");
        let (_, digest) = w.finish().expect("finish");
        assert_eq!(digest, HashAlgo::Sha1.digest(b"ab"));
        assert_ne!(digest, HashAlgo::Sha1.digest(b"abcdef"));
    }

    #[test]
    fn hashing_writer_write_error_does_not_update_checksum() {
        let inner = PartialWrite {
            max: 64,
            buf: Vec::new(),
            fail: true,
        };
        let mut w = HashingWriter::new(HashAlgo::Sha1, inner);
        assert!(w.write(b"abcdef").is_err());
        w.get_mut().fail = false;
        w.write_all(b"ab").expect("write");
        let (_, digest) = w.finish().expect("finish");
        assert_eq!(digest, HashAlgo::Sha1.digest(b"ab"));
    }

    #[test]
    fn hashing_writer_trailer_matches_digest() {
        let body = b"index payload bytes";
        let mut buf = Vec::new();
        {
            let mut w = HashingWriter::new(HashAlgo::Sha1, Cursor::new(&mut buf));
            w.write_all(body).unwrap();
            let (_, digest) = w.finish().unwrap();
            assert_eq!(digest, HashAlgo::Sha1.digest(body));
            assert_eq!(&buf[body.len()..], digest.as_bytes());
        }
        verify_trailer(HashAlgo::Sha1, &buf).expect("trailer valid");
    }

    #[test]
    fn verify_trailer_mismatch_is_typed() {
        let body = b"payload";
        let mut file = body.to_vec();
        file.extend_from_slice(&[0u8; 20]);
        let err = verify_trailer(HashAlgo::Sha1, &file).unwrap_err();
        assert!(matches!(err, TrailerMismatch { .. }));
        assert_ne!(err.expected, err.found);
    }

    #[test]
    fn hmac_sha1_rfc2202_vectors() {
        let key20 = [0x0b_u8; 20];
        let mac = hmac_sha1(&key20, b"Hi There");
        assert_eq!(hex::encode(mac), "b617318655057264e28bc0b6fb378c8ef146be00");

        let key4 = b"Jefe";
        let mac = hmac_sha1(key4, b"what do ya want for nothing?");
        assert_eq!(hex::encode(mac), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");

        let key = [0xaa_u8; 20];
        let data = [0xdd_u8; 50];
        let mac = hmac_sha1(&key, &data);
        assert_eq!(hex::encode(mac), "125d7342b9ac11cd91a39af48aa17b4f63f175d3");

        let key = hex::decode("0102030405060708090a0b0c0d0e0f10111213141516171819").unwrap();
        let data = [0xcd_u8; 50];
        let mac = hmac_sha1(&key, &data);
        assert_eq!(hex::encode(mac), "4c9007f4026250c6bc8414f9bf50c86c2d7235da");

        let key = [0x0c_u8; 20];
        let mac = hmac_sha1(&key, b"Test With Truncation");
        assert_eq!(hex::encode(mac), "4c1a03424b55e07fe7f27be1d58bb9324a9a5a04");

        let key = [0xaa_u8; 80];
        let mac = hmac_sha1(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(hex::encode(mac), "aa4ae5e15272d00e95705637ce8a3b55ed402112");

        let key = [0xaa_u8; 80];
        let mac = hmac_sha1(
            &key,
            b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data",
        );
        assert_eq!(hex::encode(mac), "e8e99d0f45237d786d6bbaa7965c7808bbff1a91");
    }

    #[test]
    fn finalize_into_and_write_trait() {
        let mut hasher = HashAlgo::Sha256.hasher();
        hasher.write_all(b"via write trait").unwrap();
        let mut out = [0u8; 32];
        let n = hasher.finalize_into(&mut out).unwrap();
        assert_eq!(n, 32);
        assert_eq!(
            HashAlgo::Sha256.digest(b"via write trait"),
            ObjectId::from_bytes(&out).unwrap()
        );
    }

    #[test]
    fn backend_matches_cpu() {
        let expected = {
            #[cfg(target_arch = "x86_64")]
            {
                if std::arch::is_x86_feature_detected!("sha") {
                    Backend::X86ShaNi
                } else {
                    Backend::Portable
                }
            }
            #[cfg(target_arch = "aarch64")]
            {
                if std::arch::is_aarch64_feature_detected!("sha2") {
                    Backend::Aarch64Sha
                } else {
                    Backend::Portable
                }
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            {
                Backend::Portable
            }
        };
        assert_eq!(super::backend(HashAlgo::Sha1), expected);
    }

    #[test]
    fn sha256_backend_matches_sha2_dispatcher() {
        #[cfg(target_arch = "x86_64")]
        {
            let got = super::backend(HashAlgo::Sha256);
            if std::arch::is_x86_feature_detected!("sha") {
                assert_eq!(got, Backend::X86ShaNi);
            } else {
                assert_eq!(got, Backend::Portable);
            }
        }
    }
}
