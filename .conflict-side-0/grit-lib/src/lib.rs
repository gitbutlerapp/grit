#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![deny(missing_docs)]
#![warn(rustdoc::unescaped_backticks)]

//! grit-lib — fast, linkable Git engine for Rust.
//!
//! Grit reimplements Git's on-disk formats and wire protocols in idiomatic Rust
//! so embedders can open repositories, walk history, diff, merge, fetch, and push
//! without shelling out to the `git` binary. The [`grit`](https://github.com/gitbutlerapp/grit)
//! CLI is a thin presentation layer over this crate; almost all behavior lives here.
//!
//! # Getting started
//!
//! [`Repository`](repo::Repository) is the primary entry point: discover or open a
//! work tree, then use methods and focused modules for objects, refs, the index,
//! config, remotes, and porcelain operations.
//!
//! ```
//! use grit_lib::config::environment::RepositoryOptions;
//! use grit_lib::repo::{InitOptions, Repository};
//! use grit_lib::state::resolve_head;
//!
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let head = resolve_head(repo.git_dir())?;
//! assert!(matches!(head, grit_lib::state::HeadState::Branch { short_name, oid: None, .. } if short_name == "main"));
//! let _ = Repository::open(
//!     repo.git_dir(),
//!     &RepositoryOptions::empty().with_work_tree(dir.path()),
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! - [`Repository`](repo::Repository) exposes typed handles for objects, refs, the
//!   index, config, revision walking, and diffs ([`objects::Objects`],
//!   [`refs::References`], [`index::IndexHandle`],
//!   [`config::ConfigHandle`], [`revwalk::RevWalk`],
//!   [`diff::Diff`]), plus [`repo_handles`] for remotes, worktrees, stash,
//!   notes, reflogs, and bundles.
//! - [`prelude`] re-exports the types most call sites need.
//! - [`porcelain`] holds user-facing operations that return structured result
//!   *models* (the CLI renders them).
//! - Browse the engine by domain: [`objects`], [`refs`], [`index`], [`revwalk`],
//!   [`diff`], [`config`], [`remote`], [`worktree`], [`stash`], [`notes`], and
//!   [`bundle`]. Legacy flat paths (`grit_lib::odb`, `grit_lib::fetch`, …) are
//!   private; [grit-scm.com/docs/library/api-layout](https://grit-scm.com/docs/library/api-layout)
//!   maps each old path to its canonical replacement.
//! - [`progress`] defines how long-running operations report progress and
//!   observe cancellation without touching the terminal.
//!
//! # API layout (canonical domains)
//!
//! | Flat path | Canonical path |
//! | --- | --- |
//! | `odb`, `pack`, `midx`, `hash`, `pack_bitmap`, `bitmap_walk`, … | [`objects`] |
//! | `reflog`, `refspec`, `reftable`, `check_ref_format`, … | [`refs`] |
//! | `sparse_checkout`, `split_index`, `untracked_cache`, `resolve_undo`, … | [`index`] |
//! | `rev_list`, `rev_parse`, `merge_base`, `commit_graph_file`, … | [`revwalk`] |
//! | `bloom`, `patch_ids`, `merge_trees`, `rerere`, … | [`diff`] |
//! | `dotfile`, `ident`, `init_filesystem`, … | [`config`] |
//! | `fetch`, `push`, `transport`, `credentials`, … | [`remote`] |
//! | `ignore`, `attributes`, `pathspec`, … | [`worktree`] |
//! | porcelain stash types | [`stash`] |
//! | `bundle_remote` | [`bundle`] |
//! | `parallel` | [`parallel`] (root) |
//! | `repo`, `error`, `porcelain`, `progress`, … | root (unchanged) |
//!
//! # Porcelain
//!
//! [`porcelain`] implements user-facing workflows (`status`, `log`, `merge`, …) as
//! pure functions that return structured result models. The library/CLI contract
//! is documented in [`porcelain`]: options in, models out, progress via
//! [`ProgressSink`](progress::ProgressSink) — no terminal I/O in the library.
//!
//! # Storage and refs
//!
//! - **Object database ([`objects`])** — loose objects plus pack files, pack indexes,
//!   multi-pack-index (MIDX), delta chains, and promisor/partial clone hooks.
//!   Backends are pluggable via the ODB abstraction on [`objects::Objects`].
//! - **References ([`refs`])** — loose files, `packed-refs`, and reftable stacks;
//!   reflog read/write and ref storage format selection live in the same module.
//!   [`RefStorageFormat`] selects the on-disk ref layout at init.
//!
//! # Network and bundles
//!
//! [`remote`] implements fetch, push, and ls-remote over local paths, git/ssh URLs,
//! and (with the optional HTTP client) smart HTTP. [`bundle`] reads and writes Git
//! bundle files and bundle-related transport helpers.
//!
//! # Features
//!
//! | Feature | Default | Description |
//! | ------- | ------- | ----------- |
//! | *(none)* | — | Core library: ODB, refs, index, diff, merge, local transport |
//! | `http-ureq` | disabled | [`ureq`](https://docs.rs/ureq) HTTP client for smart-HTTP fetch/push |
//!
//! Enable HTTP in `Cargo.toml`:
//!
//! ```toml
//! grit-lib = { version = "0.1", features = ["http-ureq"] }
//! ```
//!
//! # Errors and progress
//!
//! [`error`] defines typed failures with `thiserror`. Long-running work accepts a
//! [`ProgressSink`](progress::ProgressSink) and optional cancellation; see [`progress`].
//!
//! # Scope
//!
//! Modules tied to removed Git-compatible CLI surface are documented in
//! `docs/v1-scope.md`. Compatibility means **formats and protocols**, validated in
//! tests against the system `git` binary — not argv or message strings.

// --- Curated public API: domain roots and cross-cutting modules only.
// Flat implementation modules are `pub(crate)`; use canonical domain paths. ---
pub mod porcelain;
pub mod progress;

/// The types most callers need, re-exported for `use grit_lib::prelude::*;`.
///
/// Library guide: [embedding](https://grit-scm.com/docs/library/embedding)
///
/// ```rust
/// use grit_lib::prelude::*;
/// use grit_lib::repo::InitOptions;
///
/// # fn main() -> Result<()> {
/// let dir = tempfile::tempdir()?;
/// let repo = Repository::init(dir.path(), &InitOptions::default())?;
/// let _cfg = repo.config()?;
/// assert!(repo.refs().read_raw("HEAD")?.is_some());
/// # Ok(())
/// # }
/// ```
pub mod prelude {
    pub use crate::command_runner::{CommandRunner, RecordingRunner, SystemCommandRunner};
    pub use crate::config::ConfigSet;
    pub use crate::diagnostics::{
        CollectingDiagnostics, DiagnosticSink, NullDiagnostics, Trace, Warning,
    };
    pub use crate::environment::{Environment, RepositoryOptions};
    pub use crate::error::{Error, MidxError, Result};
    pub use crate::index::Index;
    pub use crate::objects::odb::Odb;
    pub use crate::objects::{Object, ObjectId, ObjectInfo, ObjectKind};
    pub use crate::repo::{DiscoveryInfo, InitOptions, Repository, RepositoryBuilder};
}

pub mod revwalk;
pub mod stash;

pub mod apply;
pub(crate) mod attributes;
pub mod blame;
pub(crate) mod bloom;
pub(crate) mod branch_tracking;
pub mod bundle;
pub(crate) mod bundle_remote;
pub(crate) mod check_ref_format;
pub(crate) mod clone;
pub(crate) mod combined_diff_patch;
pub(crate) mod combined_tree_diff;
pub mod command_runner;
pub mod commit;
pub(crate) mod commit_encoding;
pub(crate) mod commit_graph_file;
pub(crate) mod commit_graph_write;
pub(crate) mod commit_pretty;
pub(crate) mod commit_trailers;
pub mod config;
pub(crate) mod connectivity;
pub(crate) mod credentials;
pub(crate) mod crlf;
pub(crate) mod delta_encode;
pub(crate) mod delta_islands;
pub mod diagnostics;
pub mod diff;
pub(crate) mod diff_indent_heuristic;
mod diff_rename_parallel;
pub(crate) mod diffstat;
pub(crate) mod dotfile;
pub(crate) mod environment;
pub mod error;
mod ewah_bitmap;
pub(crate) mod pack_roll_delta;

pub mod bench_fixture;
pub(crate) mod bitmap_walk;
pub(crate) mod fetch;
pub(crate) mod fetch_head;
pub(crate) mod fetch_negotiator;
pub(crate) mod filter_process;
pub(crate) mod fmt_merge_msg;
pub(crate) mod fsck;
pub(crate) mod fsck_standalone;
pub mod fsmonitor;
pub(crate) mod gc;
pub(crate) mod git_binary_base85;
pub(crate) mod git_date;
pub(crate) mod git_path;
pub(crate) mod gitmodules;
pub(crate) mod hash;
pub(crate) mod hide_refs;
pub(crate) mod hooks;
pub(crate) mod ident;
pub(crate) mod ident_config;
pub(crate) mod ident_resolve;
pub(crate) mod ignore;
pub mod index;
mod index_eoie_ieot;
pub(crate) mod index_name_hash_lazy;
pub(crate) mod index_pack;
pub(crate) mod init_filesystem;
pub(crate) mod interpret_trailers;
pub(crate) mod local_object_copy;
pub(crate) mod log_walk;
pub(crate) mod mailmap;
pub mod maintenance;
pub(crate) mod merge_base;
pub(crate) mod merge_diff;
pub(crate) mod merge_file;
pub(crate) mod merge_trees;
pub(crate) mod midx;
pub(crate) mod midx_error;
pub(crate) mod net_trace;
pub mod notes;
pub mod object_inspect;
pub mod objects;
pub(crate) mod odb;
pub(crate) mod pack;
pub(crate) mod pack_bitmap;
mod pack_delta;
mod pack_fsck_verify;
mod pack_index;
pub(crate) mod pack_index_build;
mod pack_map;
pub(crate) mod pack_name_hash;
pub(crate) mod pack_object_select;
pub(crate) mod pack_objects;
pub(crate) mod pack_receive;
pub mod pack_reuse;
pub(crate) mod pack_rev;
pub(crate) mod pack_store;
pub mod pack_write;
pub(crate) mod pack_zlib;
pub mod parallel;
pub(crate) mod patch_ids;
pub(crate) mod path_icase;
pub(crate) mod pathspec;
pub(crate) mod pkt_line;
pub(crate) mod precompose_config;
pub(crate) mod promisor;
pub(crate) mod promisor_remote;
pub(crate) mod protocol;
pub(crate) mod protocol_v2;
pub(crate) mod prune_packed;
pub(crate) mod push;
pub(crate) mod push_cert;
pub(crate) mod push_report;
pub mod quote_path;
mod read_dir_kind;
pub(crate) mod receive_pack;
pub(crate) mod receive_quarantine;
pub(crate) mod ref_exclusions;
pub(crate) mod ref_namespace;
pub(crate) mod ref_storage;
pub(crate) mod reflog;
pub mod refs;
pub(crate) mod refs_fsck;
pub(crate) mod refspec;
pub(crate) mod reftable;
pub mod remote;
pub mod repo;
pub(crate) mod repo_caches;
pub mod repo_handles;
pub(crate) mod rerere;
pub(crate) mod resolve_undo;
pub(crate) mod rev_list;
pub(crate) mod rev_list_error;
pub(crate) mod rev_parse;
pub(crate) mod rev_parse_error;
pub(crate) mod serve;
pub(crate) mod shallow;
pub(crate) mod shared_repo;
pub(crate) mod sideband_lines;
pub(crate) mod signing;
pub(crate) mod sparse_checkout;
pub(crate) mod split_index;
pub mod state;
pub(crate) mod stripspace;
pub(crate) mod submodule_gitdir;
pub(crate) mod terminal;
pub(crate) mod textconv_cache;
mod tracked_path_tree;
pub(crate) mod transfer;
pub(crate) mod transport;
pub(crate) mod transport_path;
pub(crate) mod unicode_normalization;
pub(crate) mod unpack_objects;
pub(crate) mod untracked_cache;
pub(crate) mod upload_filter;
pub(crate) mod url_rewrite;
pub(crate) mod userdiff;
pub(crate) mod wildmatch;
pub mod worktree;
mod worktree_batch;
pub(crate) mod worktree_cwd;
pub(crate) mod worktree_ref;
pub(crate) mod worktree_rules;
mod worktree_rules_tests;
mod worktree_scan;
pub use worktree_scan::PARALLEL_STAT_MIN_ENTRIES;
#[doc(hidden)]
pub use worktree_scan::{reset_worktree_metadata_probe, worktree_metadata_probe_count};
pub(crate) mod write_tree;
pub(crate) mod ws;
pub(crate) mod zlib_inflate;

pub use ref_storage::RefStorageFormat;

#[cfg(test)]
pub(crate) mod hot_path_test_metrics;

#[cfg(test)]
mod object_write_regression;
#[cfg(test)]
mod repository_config_snapshot_tests;
