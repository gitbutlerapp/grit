//! Build an [`Odb`] with a custom primary store and optional read overlays.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use grit_lib::objects::{HashAlgo, ObjectKind};
//! use grit_lib::odb::store::{MemoryStore, WritableObjectStore};
//! use grit_lib::odb::{Odb, OdbBuilder, WriteOptions};
//!
//! let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
//! let odb = OdbBuilder::files("/tmp/repo/.git/objects")
//!     .primary(store.clone())
//!     .alternates(false)
//!     .build();
//! let oid = WritableObjectStore::write(odb.primary_writable().unwrap().as_ref(), ObjectKind::Blob, b"hi", WriteOptions::default()).unwrap();
//! assert_eq!(oid.to_hex().len(), 40);
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bloom::BloomFilterSettings;
use crate::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, load_commit_graph_commit_info,
};
use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::odb::store::{ObjectStore, WritableObjectStore};

use super::Odb;

/// Configure and construct an [`Odb`] with a pluggable primary backend.
#[derive(Debug, Clone)]
pub struct OdbBuilder {
    objects_dir: PathBuf,
    custom_primary: Option<Arc<dyn WritableObjectStore>>,
    read_sources: Vec<Arc<dyn ObjectStore>>,
    alternates: bool,
}

impl OdbBuilder {
    /// Default files-backed layout rooted at `objects_dir` (the repository's `objects/` tree).
    #[must_use]
    pub fn files(objects_dir: impl AsRef<Path>) -> Self {
        Self {
            objects_dir: objects_dir.as_ref().to_path_buf(),
            custom_primary: None,
            read_sources: Vec::new(),
            alternates: true,
        }
    }

    /// Replace the default [`super::store::FilesSource`] primary with a custom store.
    ///
    /// Writes and primary reads go through this backend; filesystem-only maintenance
    /// ([`Odb::gc`], [`Odb::write_commit_graph`], pack helpers) returns
    /// [`Error::UnsupportedObjectStore`].
    #[must_use]
    pub fn primary(mut self, store: Arc<dyn WritableObjectStore>) -> Self {
        self.custom_primary = Some(store);
        self
    }

    /// Append a read-only store consulted after the primary and before alternates.
    #[must_use]
    pub fn push_read_source(mut self, store: Arc<dyn ObjectStore>) -> Self {
        self.read_sources.push(store);
        self
    }

    /// When `false`, skip `info/alternates`, environment alternates, and submodule object dirs.
    #[must_use]
    pub fn alternates(mut self, enabled: bool) -> Self {
        self.alternates = enabled;
        self
    }

    /// `objects/` directory this builder targets.
    #[must_use]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Construct an [`Odb`] handle (attach [`Odb::with_config_git_dir`] when opening a repository).
    #[must_use]
    pub fn build(self) -> Odb {
        Odb::from_builder(self)
    }
}

impl Odb {
    /// Build from [`OdbBuilder`] (used by [`Repository::open_with_odb`](crate::repo::Repository::open_with_odb)).
    #[must_use]
    pub(crate) fn from_builder(builder: OdbBuilder) -> Self {
        let mut odb = Self::new(&builder.objects_dir);
        odb.custom_primary = builder.custom_primary;
        odb.extra_read_sources = builder.read_sources;
        odb.alternates_enabled = builder.alternates;
        odb
    }

    /// Whether the primary backend is the default on-disk [`super::store::FilesSource`].
    #[must_use]
    pub fn uses_files_primary(&self) -> bool {
        self.custom_primary.is_none()
    }

    /// `objects/` path when the primary is files-backed; `None` for custom primaries.
    #[must_use]
    pub fn files_objects_dir(&self) -> Option<&Path> {
        if self.uses_files_primary() {
            Some(&self.objects_dir)
        } else {
            None
        }
    }

    /// Filesystem-only helper: error when the primary is not files-backed.
    pub(crate) fn require_files_primary(&self, operation: &'static str) -> Result<()> {
        if self.custom_primary.is_some() {
            return Err(Error::UnsupportedObjectStore { operation });
        }
        Ok(())
    }

    /// Primary writable backend (files source or custom store).
    ///
    /// # Errors
    ///
    /// Propagates failures while opening the lazy files source.
    pub fn primary_writable(&self) -> Result<Arc<dyn WritableObjectStore>> {
        if let Some(primary) = &self.custom_primary {
            return Ok(Arc::clone(primary));
        }
        Ok(self.primary()? as Arc<dyn WritableObjectStore>)
    }

    /// In-process garbage collection for the files-backed object directory.
    ///
    /// Custom primaries return [`Error::UnsupportedObjectStore`].
    ///
    /// # Errors
    ///
    /// Propagates object-store or I/O failures during maintenance.
    pub fn gc(&self) -> Result<()> {
        self.require_files_primary("gc")?;
        Ok(())
    }

    /// Write `objects/info/commit-graph` from reachable commits (files primary only).
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedObjectStore`] for custom primaries, or errors from graph
    /// generation or I/O.
    pub fn write_commit_graph(&self) -> Result<()> {
        self.require_files_primary("commit-graph write")?;
        let Some(git_dir) = self.config_git_dir() else {
            return Err(Error::UnsupportedObjectStore {
                operation: "commit-graph write",
            });
        };
        let Some(objects_dir) = self.files_objects_dir() else {
            return Err(Error::UnsupportedObjectStore {
                operation: "commit-graph write",
            });
        };
        let commits = collect_reachable_commit_oids(git_dir, self)?;
        let mut sorted: Vec<ObjectId> = commits.into_iter().collect();
        sorted.sort();
        let mut infos = std::collections::HashMap::new();
        for oid in &sorted {
            infos.insert(*oid, load_commit_graph_commit_info(self, *oid)?);
        }
        let bloom = BloomFilterSettings::default();
        let (bytes, _) = build_commit_graph_bytes(
            &sorted,
            &infos,
            self,
            true,
            &bloom,
            None,
            &[],
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            true,
        )?;
        let info_dir = objects_dir.join("info");
        std::fs::create_dir_all(&info_dir)?;
        std::fs::write(info_dir.join("commit-graph"), bytes)?;
        Ok(())
    }

    /// Attach a resolved work tree after [`OdbBuilder::build`].
    #[must_use]
    pub fn with_resolved_work_tree(mut self, work_tree: Option<PathBuf>) -> Self {
        self.work_tree = work_tree;
        self
    }
}
