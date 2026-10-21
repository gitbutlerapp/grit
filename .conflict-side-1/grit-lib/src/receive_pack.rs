//! Receive-pack configuration and pack-header helpers.
//!
//! These helpers match Git `receive-pack` decisions without performing process spawning
//! or repository mutation.

use crate::config::{parse_git_config_int_strict, ConfigSet};

/// Effective `receive.unpacklimit` / `transfer.unpacklimit` / default (100), matching Git
/// `receive-pack` (`receive` wins when set non-negative).
#[must_use]
pub fn receive_unpack_limit(cfg: &ConfigSet) -> i32 {
    let recv = cfg
        .get("receive.unpacklimit")
        .and_then(|v| parse_git_config_int_strict(&v).ok());
    let xfer = cfg
        .get("transfer.unpacklimit")
        .and_then(|v| parse_git_config_int_strict(&v).ok());
    match (recv, xfer) {
        (Some(r), _) if r >= 0 => r as i32,
        (_, Some(t)) if t >= 0 => t as i32,
        _ => 100,
    }
}

/// `receive.maxInputSize` as a byte cap (`0` or unset = unlimited).
#[must_use]
pub fn max_input_size_from_config(cfg: &ConfigSet) -> Option<u64> {
    match cfg.get_i64("receive.maxinputsize") {
        None => None,
        Some(Ok(v)) if v <= 0 => None,
        Some(Ok(v)) => Some(v as u64),
        Some(Err(_)) => None,
    }
}

/// Object count from a packfile header.
#[must_use]
pub fn pack_object_count(pack: &[u8]) -> Option<u32> {
    if pack.len() < 12 || &pack[0..4] != b"PACK" {
        return None;
    }
    Some(u32::from_be_bytes(pack[8..12].try_into().ok()?))
}

/// Whether receive-pack should ingest this pack via `unpack-objects`.
#[must_use]
pub fn should_use_unpack_objects(pack: &[u8], cfg: &ConfigSet) -> bool {
    let unpack_limit = receive_unpack_limit(cfg);
    let nr_objects = pack_object_count(pack).unwrap_or(0);
    i64::from(unpack_limit) > 0 && i64::from(nr_objects) < i64::from(unpack_limit)
}
