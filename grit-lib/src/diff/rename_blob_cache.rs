//! Cached blob bytes for rename detection (basename + inexact phases).

use crate::odb::Odb;

use super::DiffEntry;

pub(crate) struct RenameBlobCache {
    deleted_bytes: Vec<Option<Vec<u8>>>,
    added_bytes: Vec<Option<Vec<u8>>>,
}

impl RenameBlobCache {
    pub(crate) fn new(deleted_len: usize, added_len: usize) -> Self {
        Self {
            deleted_bytes: vec![None; deleted_len],
            added_bytes: vec![None; added_len],
        }
    }

    pub(crate) fn deleted_blob(
        &mut self,
        odb: &Odb,
        deleted: &[DiffEntry],
        di: usize,
    ) -> Option<&[u8]> {
        if self.deleted_bytes[di].is_none() {
            let del = &deleted[di];
            self.deleted_bytes[di] = odb.read(&del.old_oid).ok().map(|o| o.data);
        }
        self.deleted_bytes[di].as_deref()
    }

    pub(crate) fn added_blob(
        &mut self,
        odb: &Odb,
        added: &[DiffEntry],
        ai: usize,
        work_root: Option<&std::path::Path>,
    ) -> Option<&[u8]> {
        if self.added_bytes[ai].is_none() {
            self.added_bytes[ai] = read_added_entry_bytes(odb, &added[ai], work_root);
        }
        self.added_bytes[ai].as_deref()
    }

    pub(crate) fn take_deleted_blob(&mut self, di: usize) -> Option<Vec<u8>> {
        self.deleted_bytes[di].take()
    }

    pub(crate) fn take_added_blob(&mut self, ai: usize) -> Option<Vec<u8>> {
        self.added_bytes[ai].take()
    }

    pub(crate) fn insert_deleted_bytes(&mut self, di: usize, data: Vec<u8>) {
        self.deleted_bytes[di] = Some(data);
    }

    pub(crate) fn insert_added_bytes(&mut self, ai: usize, data: Vec<u8>) {
        self.added_bytes[ai] = Some(data);
    }
}

fn read_added_entry_bytes(
    odb: &Odb,
    entry: &DiffEntry,
    work_root: Option<&std::path::Path>,
) -> Option<Vec<u8>> {
    if !entry.new_oid.is_zero() {
        if let Ok(obj) = odb.read(&entry.new_oid) {
            return Some(obj.data);
        }
    }
    let path = entry.new_path.as_deref()?;
    let root = work_root?;
    std::fs::read(root.join(path)).ok()
}
