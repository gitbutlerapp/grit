//! Stop reachability walks at objects stored in `info/alternates` (Git `pack-objects --local`).

use std::sync::Arc;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::odb::store::{CompositeStore, ObjectStore};
use crate::odb::Odb;

/// Cached alternate object stores for one maintenance or enumeration pass.
pub(crate) struct AlternateSources {
    inner: Option<Arc<CompositeStore>>,
}

impl AlternateSources {
    /// Load the alternate chain when configured; empty when alternates are off or absent.
    ///
    /// # Errors
    ///
    /// Propagates failures while opening alternate object directories.
    pub fn load(odb: &Odb) -> Result<Self> {
        let sources = odb.sources()?;
        Ok(Self {
            inner: (!sources.stores().is_empty()).then_some(sources),
        })
    }

    /// Whether any alternate object directory is in use.
    #[must_use]
    pub fn in_use(&self) -> bool {
        self.inner.is_some()
    }

    /// Whether `oid` is stored in an alternate directory (loose or pack).
    #[must_use]
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|s| s.contains(oid).unwrap_or(false))
    }

    /// Alternate stores only (for pack-only enumeration).
    #[must_use]
    pub fn composite(&self) -> Option<Arc<CompositeStore>> {
        self.inner.as_ref().map(Arc::clone)
    }
}
