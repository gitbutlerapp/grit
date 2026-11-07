//! Layer read-only stores, a commit-graph and alternates over a repository's object database.
//!
//! An [`OdbBuilder`] describes everything consulted *besides* the primary store. Pass it to
//! [`RepositoryOptions::with_odb_builder`](crate::environment::RepositoryOptions::with_odb_builder);
//! [`Repository::open`](crate::repo::Repository::open) resolves the `objects/` directory itself
//! and picks the primary store: the files layout under that directory, or the store given to
//! [`RepositoryOptions::with_object_store`](crate::environment::RepositoryOptions::with_object_store).
//!
//! # Example
//!
//! ```
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! use grit_lib::config::environment::RepositoryOptions;
//! use grit_lib::objects::odb::store::MemoryStore;
//! use grit_lib::objects::odb::OdbBuilder;
//! use grit_lib::objects::{HashAlgo, ObjectKind};
//! use grit_lib::refs::store::MemoryRefStore;
//! use grit_lib::repo::Repository;
//!
//! # fn main() -> grit_lib::error::Result<()> {
//! // A shared store the repository reads from but never writes to.
//! let shared = Arc::new(MemoryStore::new(HashAlgo::Sha1));
//! let upstream = Repository::open(
//!     Path::new("/nowhere/upstream.git"),
//!     &RepositoryOptions::empty()
//!         .with_object_store(shared.clone())
//!         .with_ref_store(Arc::new(MemoryRefStore::new())),
//! )?;
//! let oid = upstream.odb().write(ObjectKind::Blob, b"shared")?;
//!
//! let mirror = Repository::open(
//!     Path::new("/nowhere/mirror.git"),
//!     &RepositoryOptions::empty()
//!         .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
//!         .with_odb_builder(OdbBuilder::new().push_read_source(shared))
//!         .with_ref_store(Arc::new(MemoryRefStore::new())),
//! )?;
//! assert!(mirror.odb().contains(&oid)?);
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::commit_graph_file::CommitGraphChain;
use crate::commit_graph_write::collect_reachable_commit_infos;
use crate::config::ConfigSet;
use crate::error::Result;
use crate::fsck::reachability_roots;
use crate::gc::prune_loose_unreachable;
use crate::objects::HashAlgo;
use crate::odb::store::{ObjectStore, PrunableObjectStore, WritableObjectStore};
use crate::repo::error::FilesOnlyOperation;
use crate::repo::Repository;
use crate::repo::RepositoryError;

use super::{Odb, PrimaryStore};

/// Where a prune deletes unreachable objects from ([`Odb::prune_target`]).
pub(crate) enum PruneTarget<'a> {
    /// Loose object files of the files-backed primary.
    Files,
    /// A custom primary, through its prune capability.
    Store(&'a dyn PrunableObjectStore),
}

/// Read layers, an attached commit-graph and the alternates switch for a repository's
/// object database.
///
/// The builder holds no path and no primary store. [`Repository::open`](crate::repo::Repository::open)
/// resolves the `objects/` directory (including `GIT_OBJECT_DIRECTORY`) and takes the primary
/// from [`RepositoryOptions::with_object_store`](crate::environment::RepositoryOptions::with_object_store),
/// or the files layout when none is set. Lookups go primary, then each
/// [`Self::push_read_source`] store in order, then alternates.
#[derive(Debug, Clone, Default)]
pub struct OdbBuilder {
    read_sources: Vec<Arc<dyn ObjectStore>>,
    alternates: Option<bool>,
    commit_graph: Option<Arc<CommitGraphChain>>,
}

impl OdbBuilder {
    /// A builder with no read layers, no attached commit-graph, and the default alternates
    /// policy (see [`Self::alternates`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a read-only store consulted after the primary and before alternates.
    ///
    /// Its hash algorithm must match the repository's object format or
    /// [`Repository::open`](crate::repo::Repository::open) fails with
    /// [`RepositoryError::ObjectFormatMismatch`].
    #[must_use]
    pub fn push_read_source(mut self, store: Arc<dyn ObjectStore>) -> Self {
        self.read_sources.push(store);
        self
    }

    /// Attach a commit-graph for history walks to consult, for any primary store.
    ///
    /// Rev-list, merge-base, ancestry checks, rev-parse navigation, blame and path-limited log
    /// read parents, root trees, committer dates and generation numbers from `chain`, and test
    /// changed-path Bloom filters when it has them, instead of reading commit objects. Commits
    /// missing from the graph (written after it was built) are read from the store as usual,
    /// so a graph that covers only older history stays correct.
    ///
    /// History walks never read commit-graph files from `objects/` for a custom primary store:
    /// such a file describes the files layout, not that store. Build one with
    /// [`Maintenance::build_commit_graph`](crate::maintenance::Maintenance::build_commit_graph), persist its bytes in the store, and attach
    /// it here when opening the repository.
    ///
    /// An attached graph replaces any graph file under `objects/info/`. It is trusted, not
    /// checked against the store: every commit it lists must be in this database with exactly
    /// the parents and tree it records, which holds for a graph
    /// [`Maintenance::build_commit_graph`](crate::maintenance::Maintenance::build_commit_graph) built from this repository. Build it with
    /// [`CommitGraphChain::from_bytes`].
    ///
    /// The graph's [`CommitGraphChain::hash_algo`] must match the repository's object format,
    /// or [`Repository::open`](crate::repo::Repository::open) fails with
    /// [`RepositoryError::ObjectFormatMismatch`].
    #[must_use]
    pub fn commit_graph(mut self, chain: Arc<CommitGraphChain>) -> Self {
        self.commit_graph = Some(chain);
        self
    }

    /// Whether to consult `info/alternates`, environment alternates, and submodule object dirs.
    ///
    /// Without this call alternates are off when the repository has a custom primary store
    /// ([`RepositoryOptions::with_object_store`](crate::environment::RepositoryOptions::with_object_store))
    /// and on for the files layout.
    #[must_use]
    pub fn alternates(mut self, enabled: bool) -> Self {
        self.alternates = Some(enabled);
        self
    }

    /// Hash algorithms of the injected read sources, then the attached commit-graph.
    pub(crate) fn injected_hash_algos(&self) -> impl Iterator<Item = HashAlgo> + '_ {
        self.read_sources
            .iter()
            .map(|store| store.hash_algo())
            .chain(self.commit_graph.iter().map(|chain| chain.hash_algo()))
    }

    /// An [`Odb`] rooted at `objects_dir` whose primary is `primary`, or the files layout
    /// under `objects_dir` when `None`, with this builder's layers applied.
    pub(crate) fn build(
        self,
        objects_dir: &Path,
        primary: Option<Arc<dyn WritableObjectStore>>,
    ) -> Odb {
        let mut odb = Odb::new(objects_dir);
        odb.alternates_enabled = self.alternates.unwrap_or(primary.is_none());
        if let Some(store) = primary {
            odb.primary = PrimaryStore::Custom(store);
        }
        odb.extra_read_sources = self.read_sources;
        odb.attached_commit_graph = self.commit_graph;
        odb
    }
}

impl Odb {
    /// Whether the primary backend is the default on-disk [`super::store::FilesSource`].
    #[must_use]
    pub fn uses_files_primary(&self) -> bool {
        matches!(self.primary, PrimaryStore::Files(_))
    }

    /// `objects/` path when the primary is files-backed; `None` for custom primaries.
    #[must_use]
    pub fn files_objects_dir(&self) -> Option<&Path> {
        match self.primary {
            PrimaryStore::Files(_) => Some(&self.objects_dir),
            PrimaryStore::Custom(_) => None,
        }
    }

    /// Primary writable backend (files source or custom store).
    ///
    /// # Errors
    ///
    /// Propagates failures while opening the lazy files source.
    pub fn primary_writable(&self) -> Result<Arc<dyn WritableObjectStore>> {
        match &self.primary {
            PrimaryStore::Custom(primary) => Ok(Arc::clone(primary)),
            PrimaryStore::Files(_) => Ok(self.primary()? as Arc<dyn WritableObjectStore>),
        }
    }

    /// Where a prune deletes from: the files layout, or the custom primary's
    /// [`PrunableObjectStore`] capability.
    ///
    /// # Errors
    ///
    /// [`RepositoryError::UnsupportedObjectStore`] with [`FilesOnlyOperation::Prune`] for a
    /// custom primary whose [`WritableObjectStore::as_prunable`] is `None`.
    pub(crate) fn prune_target(&self) -> Result<PruneTarget<'_>> {
        let PrimaryStore::Custom(primary) = &self.primary else {
            return Ok(PruneTarget::Files);
        };
        primary
            .as_prunable()
            .map(PruneTarget::Store)
            .ok_or_else(|| {
                RepositoryError::UnsupportedObjectStore {
                    operation: FilesOnlyOperation::Prune,
                }
                .into()
            })
    }

    /// In-process garbage collection: delete every object unreachable from the repository's
    /// refs, reflogs, indexes and worktrees, with no grace period.
    ///
    /// A files-backed primary loses its unreachable loose objects (packs are left alone); a
    /// custom primary loses its unreachable objects through its [`PrunableObjectStore`]
    /// capability. Reachability uses the repository's own ref store, including one injected
    /// with [`RepositoryOptions::with_ref_store`](crate::environment::RepositoryOptions::with_ref_store),
    /// and the repository's [`Environment`](crate::environment::Environment), so config
    /// overrides passed there (such as `extensions.preciousObjects` through
    /// `GIT_CONFIG_COUNT`) apply.
    ///
    /// # Errors
    ///
    /// - [`RepositoryError::UnsupportedObjectStore`] with [`FilesOnlyOperation::Prune`] for a
    ///   custom primary without the prune capability, and with [`FilesOnlyOperation::Gc`] for
    ///   an `Odb` that does not belong to a repository;
    /// - [`RepositoryError::PreciousObjectsRepository`] when `extensions.preciousObjects` is set;
    /// - object-store or I/O failures during maintenance.
    pub fn gc(&self) -> Result<()> {
        self.prune_target()?;
        let Some(git_dir) = self.config_git_dir() else {
            return Err(RepositoryError::UnsupportedObjectStore {
                operation: FilesOnlyOperation::Gc,
            }
            .into());
        };
        // The repository's own environment carries config overrides (`GIT_CONFIG_COUNT`,
        // `-c`) that decide preciousObjects and the index location.
        let environment = self
            .shared_environment
            .as_deref()
            .cloned()
            .unwrap_or_else(crate::environment::Environment::empty);
        let mut opts = crate::environment::RepositoryOptions::with_environment(environment);
        if let PrimaryStore::Custom(primary) = &self.primary {
            let mut builder = OdbBuilder::new().alternates(self.alternates_enabled);
            for source in &self.extra_read_sources {
                builder = builder.push_read_source(Arc::clone(source));
            }
            opts = opts
                .with_object_store(Arc::clone(primary))
                .with_odb_builder(builder);
            if let Some(caches) = &self.shared_config_state {
                let ref_store = caches.open_ref_store(git_dir, &opts.environment)?;
                opts = opts.with_ref_store(ref_store);
            }
        }
        let repo = if git_dir.file_name().is_some_and(|n| n == ".git") {
            let wt = git_dir
                .parent()
                .ok_or(RepositoryError::UnsupportedObjectStore {
                    operation: FilesOnlyOperation::Gc,
                })?;
            Repository::open(git_dir, &opts.with_work_tree(wt))?
        } else {
            Repository::open(git_dir, &opts)?
        };
        crate::maintenance::refuse_precious_objects(&repo, "gc")?;
        let roots = reachability_roots(&repo)?;
        prune_loose_unreachable(self, &roots, None)?;
        Ok(())
    }

    /// Write `objects/info/commit-graph` from reachable commits (files primary only).
    ///
    /// A custom primary has no `objects/info/` to install a graph in: build it in memory with
    /// [`Maintenance::build_commit_graph`](crate::maintenance::Maintenance::build_commit_graph) and attach it with
    /// [`OdbBuilder::commit_graph`] instead.
    ///
    /// Writes nothing when there are no reachable commits (existing graph unchanged) and
    /// deletes any existing graph in a repository that rewrites parent lists (shallow,
    /// `info/grafts`, or replace refs unless `core.useReplaceRefs=false`), as
    /// [`Maintenance::write_commit_graph`](crate::maintenance::Maintenance::write_commit_graph) does.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::UnsupportedObjectStore`] for custom primaries, or errors from graph
    /// generation or I/O.
    pub fn write_commit_graph(&self) -> Result<()> {
        self.primary
            .require_files_primary(FilesOnlyOperation::CommitGraphWrite)?;
        let Some(git_dir) = self.config_git_dir() else {
            return Err(RepositoryError::UnsupportedObjectStore {
                operation: FilesOnlyOperation::CommitGraphWrite,
            }
            .into());
        };
        let Some(objects_dir) = self.files_objects_dir() else {
            return Err(RepositoryError::UnsupportedObjectStore {
                operation: FilesOnlyOperation::CommitGraphWrite,
            }
            .into());
        };
        let config = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(
                &crate::environment::Environment::empty(),
                Some(git_dir),
            ),
        )
        .unwrap_or_default();
        let skip = crate::maintenance::CommitGraphSkipReason::detect(git_dir, || {
            let enabled = config.bool_with_default("core.useReplaceRefs", true);
            Ok(enabled && !crate::refs::list_refs(git_dir, "refs/replace/")?.is_empty())
        })?;
        if skip.is_some() {
            crate::commit_graph_write::remove_commit_graph_files(objects_dir)?;
            return Ok(());
        }
        let commits = collect_reachable_commit_infos(git_dir, self)?;
        if commits.is_empty() {
            return Ok(());
        }
        let existing = CommitGraphChain::try_load(objects_dir).ok().flatten();
        let built = crate::maintenance::build_commit_graph_for_commits(
            self,
            &commits,
            &config,
            true,
            existing.as_ref(),
        )?;
        crate::commit_graph_write::atomic_replace_commit_graph(objects_dir, &built.bytes)?;
        let _ = crate::commit_graph_write::remove_commit_graph_chain_dir(objects_dir);
        Ok(())
    }

    /// Attach a resolved work tree to this handle, for resolving relative alternate paths.
    #[must_use]
    pub fn with_resolved_work_tree(mut self, work_tree: Option<PathBuf>) -> Self {
        self.work_tree = work_tree;
        self
    }
}
