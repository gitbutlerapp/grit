//! NTLMv2 client messages (pure Rust, MS-NLMP). NTLMv1 is not implemented.
//!
//! Type-3 payloads and passwords are never logged; [`Credential`](crate::credentials::Credential)
//! already redacts secrets in `Debug`.

use base64::Engine;
use getrandom::getrandom;
use hmac::{Hmac, Mac};
use md4::{Digest as Md4Digest, Md4};
use md5::Md5;

type HmacMd5 = Hmac<Md5>;

const SIGNATURE: &[u8; 8] = b"NTLMSSP\0";

/// Parsed domain and username from a credential (`DOMAIN\user` or `user@domain`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NtlmIdentity {
    pub user: String,
    pub domain: String,
}

/// Parse `DOMAIN\user` or `user@domain` from the credential username field.
#[must_use]
pub(crate) fn identity_from_username(username: &str) -> NtlmIdentity {
    if let Some((domain, user)) = username.split_once('\\') {
        return NtlmIdentity {
            user: user.to_owned(),
            domain: domain.to_owned(),
        };
    }
    if let Some((user, domain)) = username.rsplit_once('@') {
        return NtlmIdentity {
            user: user.to_owned(),
            domain: domain.to_owned(),
        };
    }
    NtlmIdentity {
        user: username.to_owned(),
        domain: String::new(),
    }
}

fn utf16_le(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect::<Vec<_>>()
}

fn nt_hash(password: &str) -> [u8; 16] {
    let data = utf16_le(password);
    let mut hasher = Md4::new();
    hasher.update(&data);
    let out = hasher.finalize();
    let mut hash = [0u8; 16];
    hash.copy_from_slice(&out);
    hash
}

fn ntlmv2_hash(password: &str, user: &str, domain: &str) -> Result<[u8; 16], NtlmError> {
    let key = nt_hash(password);
    let mut data = utf16_le(user);
    data.extend_from_slice(&utf16_le(domain));
    let mut mac = HmacMd5::new_from_slice(&key).map_err(|_| NtlmError::HmacKey)?;
    mac.update(&data);
    let out = mac.finalize().into_bytes();
    let mut hash = [0u8; 16];
    hash.copy_from_slice(&out[..16]);
    Ok(hash)
}

fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// NTLM Type 1 negotiate message.
#[must_use]
pub(crate) fn type1_message() -> Vec<u8> {
    let mut msg = Vec::with_capacity(32);
    msg.extend_from_slice(SIGNATURE);
    write_u32(&mut msg, 1);
    write_u32(&mut msg, 0x0008_8207);
    write_u64(&mut msg, 0);
    write_u64(&mut msg, 0);
    write_u64(&mut msg, 0);
    msg
}

fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Extract server challenge and target info from a Type 2 message.
pub(crate) fn parse_type2(blob: &[u8]) -> Result<Type2, NtlmError> {
    if blob.len() < 32 || blob.get(..8) != Some(SIGNATURE) {
        return Err(NtlmError::InvalidType2);
    }
    let msg_type = read_u32(blob, 8).ok_or(NtlmError::InvalidType2)?;
    if msg_type != 2 {
        return Err(NtlmError::InvalidType2);
    }
    let mut challenge = [0u8; 8];
    challenge.copy_from_slice(blob.get(24..32).ok_or(NtlmError::InvalidType2)?);
    let target_info_len = u16::from_le_bytes(
        blob.get(40..42)
            .ok_or(NtlmError::InvalidType2)?
            .try_into()
            .ok()
            .ok_or(NtlmError::InvalidType2)?,
    ) as usize;
    let target_info_off = read_u32(blob, 44).ok_or(NtlmError::InvalidType2)? as usize;
    let target_info = if target_info_len > 0 && target_info_off + target_info_len <= blob.len() {
        blob[target_info_off..target_info_off + target_info_len].to_vec()
    } else {
        Vec::new()
    };
    Ok(Type2 {
        server_challenge: challenge,
        target_info,
    })
}

/// Fields extracted from an NTLM Type 2 message.
pub(crate) struct Type2 {
    pub server_challenge: [u8; 8],
    pub target_info: Vec<u8>,
}

fn ntlmv2_blob(client_challenge: [u8; 8], timestamp: [u8; 8], target_info: &[u8]) -> Vec<u8> {
    let mut temp = Vec::new();
    temp.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
    temp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    temp.extend_from_slice(&timestamp);
    temp.extend_from_slice(&client_challenge);
    temp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    temp.extend_from_slice(target_info);
    temp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    temp
}

fn nt_proof_str(
    response_key_nt: &[u8; 16],
    server_challenge: &[u8; 8],
    temp: &[u8],
) -> Result<[u8; 16], NtlmError> {
    let mut mac = HmacMd5::new_from_slice(response_key_nt).map_err(|_| NtlmError::HmacKey)?;
    mac.update(server_challenge);
    mac.update(temp);
    let out = mac.finalize().into_bytes();
    let mut proof = [0u8; 16];
    proof.copy_from_slice(&out[..16]);
    Ok(proof)
}

/// Build NTLM Type 3 authenticate message for NTLMv2.
pub(crate) fn type3_message(
    identity: &NtlmIdentity,
    password: &str,
    type2: &Type2,
    workstation: &str,
    timestamp: Option<[u8; 8]>,
) -> Result<Vec<u8>, NtlmError> {
    let mut client_challenge = [0u8; 8];
    getrandom(&mut client_challenge).map_err(|_| NtlmError::Random)?;
    let timestamp = timestamp.ok_or(NtlmError::MissingTimestamp)?;

    let temp = ntlmv2_blob(client_challenge, timestamp, &type2.target_info);
    let response_key = ntlmv2_hash(
        password,
        &identity.user.to_uppercase(),
        &identity.domain.to_uppercase(),
    )?;
    let proof = nt_proof_str(&response_key, &type2.server_challenge, &temp)?;

    let mut nt_response = Vec::with_capacity(16 + temp.len());
    nt_response.extend_from_slice(&proof);
    nt_response.extend_from_slice(&temp);

    let lm_response: Vec<u8> = vec![0u8; 24];

    let user_utf = utf16_le(&identity.user);
    let domain_utf = utf16_le(&identity.domain);
    let workstation_utf = utf16_le(workstation);

    let mut msg = Vec::new();
    msg.extend_from_slice(SIGNATURE);
    write_u32(&mut msg, 3);
    let base_offset: u32 = 64;
    let mut offset = base_offset;

    let reserve = |off: &mut u32, data: &[u8]| -> Result<(u16, u16, u32), NtlmError> {
        let len = u16::try_from(data.len()).map_err(|_| NtlmError::MessageTooLarge)?;
        let start = *off;
        *off = off
            .checked_add(u32::try_from(data.len()).map_err(|_| NtlmError::MessageTooLarge)?)
            .ok_or(NtlmError::MessageTooLarge)?;
        Ok((len, len, start))
    };

    let (lm_len, lm_max, lm_off) = reserve(&mut offset, &lm_response)?;
    let (nt_len, nt_max, nt_off) = reserve(&mut offset, &nt_response)?;
    let (dom_len, dom_max, dom_off) = reserve(&mut offset, &domain_utf)?;
    let (usr_len, usr_max, usr_off) = reserve(&mut offset, &user_utf)?;
    let (ws_len, ws_max, ws_off) = reserve(&mut offset, &workstation_utf)?;

    msg.extend_from_slice(&lm_len.to_le_bytes());
    msg.extend_from_slice(&lm_max.to_le_bytes());
    msg.extend_from_slice(&lm_off.to_le_bytes());
    msg.extend_from_slice(&nt_len.to_le_bytes());
    msg.extend_from_slice(&nt_max.to_le_bytes());
    msg.extend_from_slice(&nt_off.to_le_bytes());
    msg.extend_from_slice(&dom_len.to_le_bytes());
    msg.extend_from_slice(&dom_max.to_le_bytes());
    msg.extend_from_slice(&dom_off.to_le_bytes());
    msg.extend_from_slice(&usr_len.to_le_bytes());
    msg.extend_from_slice(&usr_max.to_le_bytes());
    msg.extend_from_slice(&usr_off.to_le_bytes());
    msg.extend_from_slice(&ws_len.to_le_bytes());
    msg.extend_from_slice(&ws_max.to_le_bytes());
    msg.extend_from_slice(&ws_off.to_le_bytes());
    msg.extend_from_slice(&0u16.to_le_bytes());
    msg.extend_from_slice(&0u16.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    write_u32(&mut msg, 0x0008_8205);
    msg.extend_from_slice(&lm_response);
    msg.extend_from_slice(&nt_response);
    msg.extend_from_slice(&domain_utf);
    msg.extend_from_slice(&user_utf);
    msg.extend_from_slice(&workstation_utf);
    Ok(msg)
}

/// Unix epoch time as an NTLM FILETIME (100 ns intervals since 1601-01-01 UTC).
#[must_use]
pub(crate) fn ntlmv2_timestamp_from_unix(secs: u64, subsec_nanos: u32) -> [u8; 8] {
    let ft = (secs + 11_644_473_600) * 10_000_000 + u64::from(subsec_nanos) / 100;
    ft.to_le_bytes()
}

/// Encode a Type 1 or Type 3 message as an `Authorization: NTLM …` header value.
#[must_use]
pub(crate) fn authorization_header(message: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(message);
    format!("NTLM {encoded}")
}

/// Decode a base64 Type 2 challenge from a `WWW-Authenticate` tail.
pub(crate) fn decode_type2_b64(b64: &str) -> Result<Vec<u8>, NtlmError> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| NtlmError::InvalidType2)
}

/// Errors while building or parsing NTLM messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NtlmError {
    InvalidType2,
    Random,
    HmacKey,
    MessageTooLarge,
    MissingTimestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type1_starts_with_signature_and_type() {
        let msg = type1_message();
        assert!(msg.starts_with(SIGNATURE));
        assert_eq!(read_u32(&msg, 8), Some(1));
    }

    #[test]
    fn identity_parses_backslash_and_at() {
        let a = identity_from_username(r"DOM\alice");
        assert_eq!(a.domain, "DOM");
        assert_eq!(a.user, "alice");
        let b = identity_from_username("bob@CORP");
        assert_eq!(b.user, "bob");
        assert_eq!(b.domain, "CORP");
    }

    #[test]
    fn ntlmv2_hmac_input_is_server_challenge_plus_temp() {
        let type2 = Type2 {
            server_challenge: *b"12345678",
            target_info: vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x00],
        };
        let ts = [0u8; 8];
        let client = [1u8; 8];
        let temp = ntlmv2_blob(client, ts, &type2.target_info);
        assert_eq!(temp.len(), 28 + type2.target_info.len() + 4);
        assert!(!temp.contains(&type2.server_challenge[0]));
        let key = ntlmv2_hash("Password", "USER", "DOMAIN").unwrap();
        let proof = nt_proof_str(&key, &type2.server_challenge, &temp).unwrap();
        assert_eq!(proof.len(), 16);
        let t3 = type3_message(
            &NtlmIdentity {
                user: "User".into(),
                domain: "Domain".into(),
            },
            "Password",
            &type2,
            "HOST",
            Some(ts),
        )
        .unwrap();
        assert!(t3.len() > 80);
    }
}
