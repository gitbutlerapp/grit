//! Object position order for pack and MIDX bitmaps (Git pack offset / MIDX pseudo-pack order).

use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::midx::load_midx_reuse_tables;
use crate::objects::ObjectId;
use crate::pack::PackIndex;
use crate::pack_rev::{rev_path_for_index, try_rev_positions_in_pack_order};
use std::fs;

/// Maps between bitmap bit positions and object ids for one pack or MIDX namespace.
#[derive(Debug)]
pub(crate) enum BitmapOrder {
    Pack {
        index: Arc<PackIndex>,
        /// Index row for bitmap rank `r` is `rank_to_index_row[r]`.
        rank_to_index_row: Vec<u32>,
        index_row_to_rank: Vec<u32>,
    },
    Midx {
        oids: Vec<ObjectId>,
        rid_order: Vec<u32>,
        oid_idx_to_rank: Vec<u32>,
    },
}

impl BitmapOrder {
    pub(crate) fn object_count(&self) -> u32 {
        match self {
            Self::Pack {
                rank_to_index_row, ..
            } => u32::try_from(rank_to_index_row.len()).unwrap_or(u32::MAX),
            Self::Midx { oids, .. } => u32::try_from(oids.len()).unwrap_or(u32::MAX),
        }
    }

    pub(crate) fn oid_at(&self, position: u32) -> Result<ObjectId> {
        let pos = usize::try_from(position)
            .map_err(|_| Error::CorruptObject("bitmap position out of range".to_owned()))?;
        match self {
            Self::Pack {
                index,
                rank_to_index_row,
                ..
            } => {
                let row = *rank_to_index_row.get(pos).ok_or_else(|| {
                    Error::CorruptObject("bitmap position out of range".to_owned())
                })? as usize;
                ObjectId::from_bytes(index.oid_at(row))
            }
            Self::Midx {
                oids, rid_order, ..
            } => {
                let oid_idx = *rid_order.get(pos).ok_or_else(|| {
                    Error::CorruptObject("bitmap position out of range".to_owned())
                })? as usize;
                oids.get(oid_idx).copied().ok_or_else(|| {
                    Error::CorruptObject("MIDX reverse index out of range".to_owned())
                })
            }
        }
    }

    pub(crate) fn position_of(&self, oid: &ObjectId) -> Option<u32> {
        match self {
            Self::Pack {
                index,
                index_row_to_rank,
                ..
            } => {
                let row = index.find_position(oid)?;
                index_row_to_rank.get(row).copied()
            }
            Self::Midx {
                oids,
                oid_idx_to_rank,
                ..
            } => {
                let oid_idx = oids.binary_search(oid).ok()?;
                Some(oid_idx_to_rank[oid_idx])
            }
        }
    }

    /// Pack-index or MIDX object row (Git `commit_pos` / name-hash cache index).
    pub(crate) fn storage_row_for_oid(&self, oid: &ObjectId) -> Option<u32> {
        match self {
            Self::Pack { index, .. } => u32::try_from(index.find_position(oid)?).ok(),
            Self::Midx { oids, .. } => u32::try_from(oids.binary_search(oid).ok()?).ok(),
        }
    }

    /// Row in the pack index or MIDX object list for bitmap rank `position`.
    pub(crate) fn storage_row_for_bitmap_rank(&self, position: u32) -> Option<u32> {
        match self {
            Self::Pack {
                rank_to_index_row, ..
            } => rank_to_index_row.get(position as usize).copied(),
            Self::Midx { rid_order, .. } => rid_order.get(position as usize).copied(),
        }
    }

    pub(crate) fn oid_at_storage_row(&self, row: u32) -> Result<ObjectId> {
        match self {
            Self::Pack { index, .. } => ObjectId::from_bytes(index.oid_at(row as usize)),
            Self::Midx { oids, .. } => oids
                .get(row as usize)
                .copied()
                .ok_or_else(|| Error::CorruptObject("MIDX object row out of range".to_owned())),
        }
    }

    pub(crate) fn load_pack(_objects_dir: &Path, idx: Arc<PackIndex>) -> Result<Self> {
        let n = idx.len();
        let rev_path = rev_path_for_index(&idx.idx_path);
        let rank_to_index_row = if let Ok(data) = fs::read(&rev_path) {
            if let Some(order) = try_rev_positions_in_pack_order(&data, n) {
                order
            } else {
                compute_rank_order_from_offsets(&idx)?
            }
        } else {
            compute_rank_order_from_offsets(&idx)?
        };
        let mut index_row_to_rank = vec![u32::MAX; n];
        for (rank, &row) in rank_to_index_row.iter().enumerate() {
            let ri = row as usize;
            if ri < n {
                index_row_to_rank[ri] = u32::try_from(rank).unwrap_or(u32::MAX);
            }
        }
        Ok(Self::Pack {
            index: idx,
            rank_to_index_row,
            index_row_to_rank,
        })
    }

    pub(crate) fn pack_index(&self) -> Option<Arc<PackIndex>> {
        match self {
            Self::Pack { index, .. } => Some(Arc::clone(index)),
            Self::Midx { .. } => None,
        }
    }

    pub(crate) fn load_midx(objects_dir: &Path) -> Result<Option<Self>> {
        let tables = match load_midx_reuse_tables(objects_dir)? {
            Some(t) => t,
            None => return Ok(None),
        };
        Ok(Some(Self::Midx {
            oids: tables.oids,
            rid_order: tables.rid_order,
            oid_idx_to_rank: tables.oid_idx_to_rank,
        }))
    }
}

fn compute_rank_order_from_offsets(idx: &PackIndex) -> Result<Vec<u32>> {
    let n = idx.len();
    let mut rows: Vec<(u64, u32)> = (0..n)
        .map(|i| (idx.offset_at(i), u32::try_from(i).unwrap_or(u32::MAX)))
        .collect();
    rows.sort_by_key(|(off, _)| *off);
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}
