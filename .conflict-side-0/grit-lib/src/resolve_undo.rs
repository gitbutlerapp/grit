//! Git index `REUC` (resolve-undo) extension — records unmerged stages when a conflict is resolved.
//!
//! Format matches Git's `resolve-undo.c`: for each path, NUL-terminated name, three octal modes with
//! NUL terminators, then raw SHA-1s for non-zero modes.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::index::IndexEntry;
use crate::objects::ObjectId;

/// Per-path undo data: up to three conflict stages (index 0 = stage 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveUndoRecord {
    /// File modes for stages 1–3 (`0` means absent).
    pub modes: [u32; 3],
    /// Blob OIDs for stages 1–3 (only meaningful when `modes[i] != 0`).
    pub oids: [ObjectId; 3],
}

impl Default for ResolveUndoRecord {
    fn default() -> Self {
        Self {
            modes: [0; 3],
            oids: [ObjectId::zero(); 3],
        }
    }
}

/// Parse the `REUC` extension payload into a path → record map.
pub fn parse_resolve_undo_payload(data: &[u8]) -> Result<BTreeMap<Vec<u8>, ResolveUndoRecord>> {
    let mut out: BTreeMap<Vec<u8>, ResolveUndoRecord> = BTreeMap::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let Some(nul) = data[pos..].iter().position(|&b| b == 0) else {
            return Err(Error::IndexError("truncated resolve-undo path".to_owned()));
        };
        let path = data[pos..pos + nul].to_vec();
        pos += nul + 1;
        let mut modes = [0u32; 3];
        for m in &mut modes {
            let Some(term) = data[pos..].iter().position(|&b| b == 0) else {
                return Err(Error::IndexError("truncated resolve-undo mode".to_owned()));
            };
            let slice = &data[pos..pos + term];
            let s = std::str::from_utf8(slice)
                .map_err(|_| Error::IndexError("invalid UTF-8 in resolve-undo mode".to_owned()))?;
            *m = u32::from_str_radix(s, 8)
                .map_err(|_| Error::IndexError(format!("invalid resolve-undo mode '{s}'")))?;
            pos += term + 1;
        }
        let mut oids = [ObjectId::zero(); 3];
        for i in 0..3 {
            if modes[i] == 0 {
                continue;
            }
            if pos + 20 > data.len() {
                return Err(Error::IndexError(
                    "truncated resolve-undo object id".to_owned(),
                ));
            }
            oids[i] = ObjectId::from_bytes(&data[pos..pos + 20])
                .map_err(|e| Error::IndexError(e.to_string()))?;
            pos += 20;
        }
        out.insert(path, ResolveUndoRecord { modes, oids });
    }
    Ok(out)
}

/// Serialise resolve-undo records to the `REUC` extension body (sorted by path).
pub fn write_resolve_undo_payload(map: &BTreeMap<Vec<u8>, ResolveUndoRecord>) -> Vec<u8> {
    let mut sb = Vec::new();
    for (path, ru) in map {
        if !ru.modes.iter().any(|m| *m != 0) {
            continue;
        }
        sb.extend_from_slice(path);
        sb.push(0);
        for m in &ru.modes {
            let s = format!("{:o}", m);
            sb.extend_from_slice(s.as_bytes());
            sb.push(0);
        }
        for i in 0..3 {
            if ru.modes[i] != 0 {
                sb.extend_from_slice(ru.oids[i].as_bytes());
            }
        }
    }
    sb
}

/// Merge one unmerged index entry into the resolve-undo map for its path.
pub fn record_resolve_undo_for_entry(
    map: &mut Option<BTreeMap<Vec<u8>, ResolveUndoRecord>>,
    entry: &IndexEntry,
) {
    let stage = entry.stage();
    if stage == 0 || stage > 3 {
        return;
    }
    let idx = (stage - 1) as usize;
    let m = map.get_or_insert_with(BTreeMap::new);
    let ru = m.entry(entry.path.clone()).or_default();
    ru.modes[idx] = entry.mode;
    ru.oids[idx] = entry.oid;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexEntry, MODE_REGULAR};

    #[test]
    fn payload_roundtrip_and_record_merge() {
        let oid = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").expect("oid");
        let mut map = BTreeMap::new();
        map.insert(
            b"f".to_vec(),
            ResolveUndoRecord {
                modes: [0o100644, 0, 0o100644],
                oids: [oid, ObjectId::zero(), oid],
            },
        );
        let payload = write_resolve_undo_payload(&map);
        assert_eq!(parse_resolve_undo_payload(&payload).expect("parse"), map);

        let mut merged = None;
        let mut entry = IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: 4 | (2 << 12),
            flags_extended: None,
            path: b"f".to_vec(),
            base_index_pos: 0,
        };
        record_resolve_undo_for_entry(&mut merged, &entry);
        entry.flags = (entry.flags & 0x0FFF) | (3 << 12);
        record_resolve_undo_for_entry(&mut merged, &entry);
        assert!(merged
            .as_ref()
            .is_some_and(|m| m.contains_key(b"f".as_slice())));
    }
}
