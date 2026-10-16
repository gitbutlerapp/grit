//! Minimal fixtures for `odb_backend_pack` (deep delta read micro-benchmarks only).

use std::sync::OnceLock;

use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use tempfile::TempDir;

#[path = "fixture.rs"]
mod fixture;

use fixture::{build_delta_chain_pack, install_indexed_pack};

pub struct PackDeltaBench {
    pub _root: TempDir,
    pub odb: Odb,
    pub packed_delta_oid: ObjectId,
}

impl PackDeltaBench {
    fn build() -> Self {
        let root = tempfile::tempdir().expect("pack delta bench tempdir");
        let objects = root.path().join("objects");
        let (delta_pack, packed_delta_oid, _depth) = build_delta_chain_pack(80);
        install_indexed_pack(&objects, "delta", &delta_pack);
        let odb = Odb::new(&objects);
        Self {
            _root: root,
            odb,
            packed_delta_oid,
        }
    }
}

/// Shared deep-delta pack read fixture (no 10k/100k backend setup).
pub fn global() -> &'static PackDeltaBench {
    static FIX: OnceLock<PackDeltaBench> = OnceLock::new();
    FIX.get_or_init(PackDeltaBench::build)
}
