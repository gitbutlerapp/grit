//! Build an [`Odb`] with a custom primary store and optional read overlays.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use grit_lib::objects::{HashAlgo, ObjectKind};
//! use grit_lib::objects::odb::store::{MemoryStore, WritableObjectStore};
//! use grit_lib::objects::odb::{Odb, OdbBuilder, WriteOptions};
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

use crate::commit_graph_file::CommitGraphChain;
use crate::commit_graph_write::collect_reachable_commit_oids;
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::fsck::reachability_roots;
use crate::gc::prune_loose_unreachable;
use crate::objects::HashAlgo;
use crate::odb::store::{ObjectStore, WritableObjectStore};
use crate::repo::Repository;

use super::Odb;

/// Configure and construct an [`Odb`] with a pluggable primary backend.
#[derive(Debug, Clone)]
pub struct OdbBuilder {
    objects_dir: PathBuf,
    custom_primary: Option<Arc<dyn WritableObjectStore>>,
    read_sources: Vec<Arc<dyn ObjectStore>>,
    alternates: bool,
    commit_graph: Option<Arc<CommitGraphChain>>,
}

impl OdbBuilder {
    /// Default files-backed layout rooted at `objects_dir` (the repository's `objects/` tree).
    ///
    /// When passed to [`RepositoryBuilder::odb_builder`](crate::repo::RepositoryBuilder::odb_builder),
    /// any spelling of the repository's `objects/` directory is accepted: a path through
    /// symlinks, or a relative path (resolved against the environment's current directory).
    /// With a custom [`Self::primary`] the directory need not exist, but it must still name
    /// the repository's `objects/` directory.
    #[must_use]
    pub fn files(objects_dir: impl AsRef<Path>) -> Self {
        Self {
            objects_dir: objects_dir.as_ref().to_path_buf(),
            custom_primary: None,
            read_sources: Vec::new(),
            alternates: true,
            commit_graph: None,
        }
    }

    /// Replace the default [`super::store::FilesSource`] primary with a custom store.
    ///
    /// Writes and primary reads go through this backend, including commits, staging
    /// ([`crate::porcelain::add::stage`]), merges, stash, notes, and bundle creation.
    /// [`Repository::fsck`](crate::repo::Repository::fsck) validates the objects this
    /// store lists. Received packs are written into it object by object:
    /// [`ingest_received_pack`](crate::index_pack::ingest_received_pack) and
    /// [`Bundle::unbundle`](crate::bundle::Bundle::unbundle) resolve every delta and call
    /// [`WritableObjectStore::write`] instead of installing a pack under `objects/pack/`.
    ///
    /// [`Remote::fetch`](crate::remote::Remote::fetch), [`Remote::push`](crate::remote::Remote::push),
    /// pull, bundles, and [`clone_into`](crate::clone::clone_into) read and write objects through
    /// the custom primary. Partial-clone filters (`operation: "partial fetch"`) and filesystem
    /// maintenance still refuse when they need on-disk pack layout:
    ///
    /// - maintenance: [`Odb::gc`], [`Odb::write_commit_graph`], repack, MIDX, and pack helpers
    ///
    /// History walks never read commit-graph files from `objects/` for a custom primary: such
    /// a file describes the files layout, not this store. To give walks a commit-graph, build
    /// one with [`crate::maintenance::build_commit_graph`], persist its bytes in the store, and
    /// attach it with [`Self::commit_graph`] when opening the repository.
    ///
    /// The store's [`ObjectStore::hash_algo`] must match the repository's object format:
    /// [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) fails with
    /// [`Error::ObjectFormatMismatch`] otherwise. [`Self::build`] does not check, since it
    /// has no repository config to compare against.
    #[must_use]
    pub fn primary(mut self, store: Arc<dyn WritableObjectStore>) -> Self {
        self.custom_primary = Some(store);
        self
    }

    /// Append a read-only store consulted after the primary and before alternates.
    ///
    /// Like [`Self::primary`], its hash algorithm must match the repository's object format
    /// or [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) fails with
    /// [`Error::ObjectFormatMismatch`].
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
    /// An attached graph replaces any graph file under `objects/info/`. It is trusted, not
    /// checked against the store: every commit it lists must be in this database with exactly
    /// the parents and tree it records, which holds for a graph
    /// [`crate::maintenance::build_commit_graph`] built from this repository. Build it with
    /// [`CommitGraphChain::from_bytes`].
    ///
    /// The graph's [`CommitGraphChain::hash_algo`] must match the repository's object format,
    /// or [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) fails with
    /// [`Error::ObjectFormatMismatch`].
    #[must_use]
    pub fn commit_graph(mut self, chain: Arc<CommitGraphChain>) -> Self {
        self.commit_graph = Some(chain);
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

    pub(crate) fn set_objects_dir(&mut self, objects_dir: PathBuf) {
        self.objects_dir = objects_dir;
    }

    /// Whether [`Self::primary`] replaced the files-backed primary store.
    pub(crate) fn has_custom_primary(&self) -> bool {
        self.custom_primary.is_some()
    }

    /// Hash algorithms of the injected stores: the custom primary, then each read source,
    /// then the attached commit-graph.
    pub(crate) fn injected_hash_algos(&self) -> impl Iterator<Item = HashAlgo> + '_ {
        self.custom_primary
            .iter()
            .map(|store| store.hash_algo())
            .chain(self.read_sources.iter().map(|store| store.hash_algo()))
            .chain(self.commit_graph.iter().map(|chain| chain.hash_algo()))
    }

    /// Construct an [`Odb`] handle (attach [`Odb::with_config_git_dir`] when opening a repository).
    #[must_use]
    pub fn build(self) -> Odb {
        Odb::from_builder(self)
    }
}

impl Odb {
    /// Build from [`OdbBuilder`] (used by [`RepositoryBuilder`](crate::repo::RepositoryBuilder)).
    #[must_use]
    pub(crate) fn from_builder(builder: OdbBuilder) -> Self {
        let mut odb = Self::new(&builder.objects_dir);
        odb.custom_primary = builder.custom_primary;
        odb.extra_read_sources = builder.read_sources;
        odb.alternates_enabled = builder.alternates;
        odb.attached_commit_graph = builder.commit_graph;
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
        let Some(git_dir) = self.config_git_dir() else {
            return Err(Error::UnsupportedObjectStore { operation: "gc" });
        };
        let opts = crate::environment::RepositoryOptions::empty();
        let repo = if git_dir.file_name().is_some_and(|n| n == ".git") {
            let wt = git_dir
                .parent()
                .ok_or(Error::UnsupportedObjectStore { operation: "gc" })?;
            Repository::open(git_dir, &opts.with_work_tree(wt))?
        } else {
            Repository::open(git_dir, &opts)?
        };
        let roots = reachability_roots(&repo)?;
        prune_loose_unreachable(self, &roots, None)?;
        Ok(())
    }

    /// Write `objects/info/commit-graph` from reachable commits (files primary only).
    ///
    /// A custom primary has no `objects/info/` to install a graph in: build it in memory with
    /// [`crate::maintenance::build_commit_graph`] and attach it with
    /// [`OdbBuilder::commit_graph`] instead.
    ///
    /// Writes nothing and deletes any existing graph in a repository that rewrites parent
    /// lists (shallow, `info/grafts`, or replace refs unless `core.useReplaceRefs=false`), as
    /// [`crate::maintenance::write_commit_graph`] does.
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
        let config = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(git_dir),
            true,
        )
        .unwrap_or_default();
        let skip = crate::maintenance::CommitGraphSkipReason::detect(git_dir, || {
            let enabled = config
                .get_bool("core.useReplaceRefs")
                .and_then(|r| r.ok())
                .unwrap_or(true);
            Ok(enabled && !crate::refs::list_refs(git_dir, "refs/replace/")?.is_empty())
        })?;
        if skip.is_some() {
            crate::commit_graph_write::remove_commit_graph_files(objects_dir)?;
            return Ok(());
        }
        let commits = collect_reachable_commit_oids(git_dir, self)?;
        let existing = CommitGraphChain::try_load(objects_dir).ok().flatten();
        let built = crate::maintenance::build_commit_graph_for_commits(
            self,
            commits,
            &config,
            true,
            existing.as_ref(),
        )?;
        crate::commit_graph_write::atomic_replace_commit_graph(objects_dir, &built.bytes)?;
        let _ = crate::commit_graph_write::remove_commit_graph_chain_dir(objects_dir);
        Ok(())
    }

    /// Attach a resolved work tree after [`OdbBuilder::build`].
    #[must_use]
    pub fn with_resolved_work_tree(mut self, work_tree: Option<PathBuf>) -> Self {
        self.work_tree = work_tree;
        self
    }
}
