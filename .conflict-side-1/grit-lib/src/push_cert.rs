//! Signed-push certificate generation and verification.
//!
//! Port of the parts of Git's `send-pack.c:generate_push_cert` (client side) and
//! `builtin/receive-pack.c` (server side: nonce HMAC, cert blob, `check_nonce`,
//! and the `GIT_PUSH_CERT*` hook environment) that the grit CLI needs to drive
//! `git push --signed` over the local and smart transports.
//!
//! A push certificate is a text payload of the form
//!
//! ```text
//! certificate version 0.1
//! pusher <key-id> <epoch> <tz>
//! pushee <url>
//! nonce <nonce>
//! push-option <opt>          (zero or more)
//!
//! <old-oid> <new-oid> <refname>   (one per updated ref)
//! ```
//!
//! followed by a detached signature (gpg/gpgsm/ssh) over the payload — exactly
//! the layout [`crate::signing::parse_signed_buffer`] / [`crate::signing::verify_tag`]
//! already understand (signature appended, not header-embedded).

use crate::hash;
use crate::signing::{GpgConfig, SignatureCheck};

/// `NONCE_OK` from receive-pack: the certificate nonce matched what we issued.
pub const NONCE_OK: &str = "OK";

/// Compute the push-cert nonce HMAC-SHA1 over `<path>:<stamp>` keyed by `seed`,
/// returning Git's `"<stamp>-<hex-hmac>"` form (`receive-pack.c:prepare_push_cert_nonce`).
///
/// `path` is the receiver's service directory (its git dir) and `stamp` is the
/// receiver's wall-clock epoch seconds at the moment the advertisement is built.
#[must_use]
pub fn prepare_push_cert_nonce(path: &str, stamp: i64, seed: &str) -> String {
    let text = format!("{path}:{stamp}");
    let mac = hash::hmac_sha1(seed.as_bytes(), text.as_bytes());
    let mut hex = String::with_capacity(40);
    for b in mac {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("{stamp}-{hex}")
}

/// A single ref update line in a push certificate.
pub struct CertRefUpdate {
    /// Old OID (40 zeros for a create).
    pub old_oid: String,
    /// New OID (40 zeros for a delete).
    pub new_oid: String,
    /// Full ref name (`refs/heads/...`).
    pub refname: String,
}

/// Build the unsigned push-certificate payload (`send-pack.c:generate_push_cert`).
///
/// `pusher` is the signing key id (Git uses `get_signing_key_id()`, falling back
/// to the committer ident `"Name <email>"`). `date` is `"<epoch> <tz>"`. `url` and
/// `nonce` are omitted when empty. Returns `None` when there are no updates to send.
#[must_use]
pub fn build_push_cert_payload(
    pusher: &str,
    date: &str,
    url: Option<&str>,
    nonce: Option<&str>,
    push_options: &[String],
    updates: &[CertRefUpdate],
) -> Option<Vec<u8>> {
    if updates.is_empty() {
        return None;
    }
    let mut cert = String::new();
    cert.push_str("certificate version 0.1\n");
    cert.push_str(&format!("pusher {pusher} {date}\n"));
    if let Some(u) = url.filter(|u| !u.is_empty()) {
        cert.push_str(&format!("pushee {u}\n"));
    }
    if let Some(n) = nonce.filter(|n| !n.is_empty()) {
        cert.push_str(&format!("nonce {n}\n"));
    }
    for opt in push_options {
        cert.push_str(&format!("push-option {opt}\n"));
    }
    cert.push('\n');
    for u in updates {
        cert.push_str(&format!("{} {} {}\n", u.old_oid, u.new_oid, u.refname));
    }
    Some(cert.into_bytes())
}

/// The hook-visible certificate environment, mirroring receive-pack's
/// `GIT_PUSH_CERT*` variables.
pub struct PushCertEnv {
    /// `GIT_PUSH_CERT` — OID of the blob the cert was stored as.
    pub cert_oid: String,
    /// `GIT_PUSH_CERT_SIGNER` — `%GS` signer (may be empty).
    pub signer: String,
    /// `GIT_PUSH_CERT_KEY` — `%GK` key id (may be empty).
    pub key: String,
    /// `GIT_PUSH_CERT_STATUS` — single-char `%G?` result.
    pub status: char,
    /// `GIT_PUSH_CERT_NONCE` — the nonce we issued (None when no seed).
    pub nonce: Option<String>,
    /// `GIT_PUSH_CERT_NONCE_STATUS` — `OK`/`SLOP`/`BAD`/... (None when no seed).
    pub nonce_status: Option<String>,
}

impl PushCertEnv {
    /// Materialize the variables as `(name, value)` pairs for hook execution.
    #[must_use]
    pub fn to_env_pairs(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("GIT_PUSH_CERT".to_owned(), self.cert_oid.clone()),
            ("GIT_PUSH_CERT_SIGNER".to_owned(), self.signer.clone()),
            ("GIT_PUSH_CERT_KEY".to_owned(), self.key.clone()),
            ("GIT_PUSH_CERT_STATUS".to_owned(), self.status.to_string()),
        ];
        if let (Some(nonce), Some(nonce_status)) = (&self.nonce, &self.nonce_status) {
            env.push(("GIT_PUSH_CERT_NONCE".to_owned(), nonce.clone()));
            env.push((
                "GIT_PUSH_CERT_NONCE_STATUS".to_owned(),
                nonce_status.clone(),
            ));
        }
        env
    }
}

/// Compute the receiver-side `GIT_PUSH_CERT*` environment for a signed push,
/// reusing the existing detached-signature verification.
///
/// `signed_cert` is the full `<payload><signature>` buffer; `cert_oid` is the OID
/// it was stored under as a blob; `issued_nonce` is the nonce the receiver
/// advertised (the same value embedded in the cert by the local in-process push,
/// so the nonce status is `OK`).
#[must_use]
pub fn cert_env_from_check(
    check: &SignatureCheck,
    cert_oid: String,
    issued_nonce: Option<String>,
) -> PushCertEnv {
    let nonce_status = issued_nonce.as_ref().map(|_| NONCE_OK.to_owned());
    PushCertEnv {
        cert_oid,
        signer: check.signer.clone().unwrap_or_default(),
        key: check.key.clone().unwrap_or_default(),
        status: check.result,
        nonce: issued_nonce,
        nonce_status,
    }
}

/// Verify a stored push certificate, deriving the signer/key/status from the
/// detached signature exactly as `git verify-tag` does.
///
/// # Errors
///
/// Returns an error when the verifier program cannot be run.
pub fn verify_push_cert(
    cfg: &GpgConfig,
    signed_cert: &[u8],
) -> crate::error::Result<SignatureCheck> {
    crate::signing::verify_tag(cfg, signed_cert)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_matches_git_format() {
        // Shape: "<stamp>-<40 hex>", and is stable for the same inputs.
        let n = prepare_push_cert_nonce("/srv/repo.git", 1_700_000_000, "sekrit");
        let (stamp, hex) = n.split_once('-').expect("nonce has a dash");
        assert_eq!(stamp, "1700000000");
        assert_eq!(hex.len(), 40);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        let again = prepare_push_cert_nonce("/srv/repo.git", 1_700_000_000, "sekrit");
        assert_eq!(n, again);
    }

    #[test]
    fn nonce_changes_with_seed_and_path() {
        let a = prepare_push_cert_nonce("/srv/repo.git", 100, "sekrit");
        let b = prepare_push_cert_nonce("/srv/repo.git", 100, "other");
        let c = prepare_push_cert_nonce("/other/repo.git", 100, "sekrit");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn payload_has_expected_lines() {
        let updates = vec![CertRefUpdate {
            old_oid: "0".repeat(40),
            new_oid: "1".repeat(40),
            refname: "refs/heads/main".to_owned(),
        }];
        let payload = build_push_cert_payload(
            "A U Thor <author@example.com>",
            "1700000000 +0000",
            Some("/srv/repo.git"),
            Some("1700000000-deadbeef"),
            &[],
            &updates,
        )
        .expect("payload built");
        let text = String::from_utf8(payload).expect("utf8");
        assert!(text.starts_with("certificate version 0.1\n"));
        assert!(text.contains("pusher A U Thor <author@example.com> 1700000000 +0000\n"));
        assert!(text.contains("pushee /srv/repo.git\n"));
        assert!(text.contains("nonce 1700000000-deadbeef\n"));
        assert!(text.contains(&format!(
            "{} {} refs/heads/main\n",
            "0".repeat(40),
            "1".repeat(40)
        )));
    }

    #[test]
    fn payload_none_without_updates() {
        assert!(build_push_cert_payload("x", "0 +0000", None, None, &[], &[]).is_none());
    }
}
