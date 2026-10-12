#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! grit-lib — fast, linkable Git engine for Rust.
//!
//! # Architecture
//!
//! All Git-compatible logic lives here; the `grit` binary is a thin CLI shim
//! that parses arguments and delegates to types exposed from this crate.
//! Modules that only served out-of-scope Git commands or the removed `grit-git`
//! CLI are not part of the crate; see `docs/v1-scope.md` (Removed from grit-lib).
//!
//! ## Where to start
//!
//! - [`Repository`](repo::Repository) is the central handle — open or discover
//!   one, then reach the object store, index, refs, and config through it or the
//!   typed handles ([`objects::Objects`](objects::Objects), [`refs::References`](refs::References),
//!   [`index::IndexHandle`](index::IndexHandle), [`config::ConfigHandle`](config::ConfigHandle),
//!   [`revwalk::RevWalk`](revwalk::RevWalk), [`diff::Diff`](diff::Diff)).
//! - [`prelude`] re-exports the handful of types most call sites need.
//! - [`porcelain`] holds user-facing operations that return structured result
//!   *models* (the CLI renders them). The flat module list below remains the
//!   low-level escape hatch and keeps every existing path valid.
//! - Browse the engine by domain: [`objects`], [`refs`], [`index`], [`revwalk`],
//!   [`diff`], [`config`], [`remote`], [`worktree`], [`stash`], [`notes`], and
//!   [`bundle`]. The mapping table on [grit-scm.com/docs/library/api-layout](https://grit-scm.com/docs/library/api-layout)
//!   lists every flat path.
//! - [`progress`] defines how long-running operations report progress and
//!   observe cancellation without touching the terminal.
//!
//! ## API layout (canonical domains)
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
//! ## Core modules
//!
//! - [`error`] — shared error types using `thiserror`
//! - [`objects`] — object ID, object kinds, and in-memory representations
//! - [`odb`] — loose object store (read/write zlib-compressed objects)
//! - [`repo`] — repository discovery and handle
//! - [`index`] — Git index (staging area) read/write
//! - [`ignore`] — ignore/exclude pattern matching for check-ignore
//! - [`refs`] — reference storage (files backend)

// --- Curated public API (additive; the flat `pub mod` list below is unchanged
// so every existing `grit_lib::<module>::…` path keeps resolving) ---
pub mod porcelain;
pub mod progress;

/// The types most callers need, re-exported for `use grit_lib::prelude::*;`.
pub mod prelude {
    pub use crate::command_runner::{CommandRunner, RecordingRunner, SystemCommandRunner};
    pub use crate::config::ConfigSet;
    pub use crate::diagnostics::{
        CollectingDiagnostics, DiagnosticSink, NullDiagnostics, Trace, Warning,
    };
    pub use crate::environment::{Environment, RepositoryOptions};
    pub use crate::error::{Error, MidxError, Result};
    pub use crate::index::Index;
    pub use crate::objects::{Object, ObjectId, ObjectInfo, ObjectKind};
    pub use crate::odb::Odb;
    pub use crate::repo::Repository;
}

pub mod revwalk;
pub mod stash;

pub mod apply;
pub mod attributes;
pub mod blame;
pub mod bloom;
pub mod branch_tracking;
pub mod bundle;
pub mod bundle_remote;
pub mod check_ref_format;
pub mod clone;
pub mod combined_diff_patch;
pub mod combined_tree_diff;
pub mod command_runner;
pub mod commit;
pub mod commit_encoding;
pub mod commit_graph_file;
pub mod commit_graph_write;
pub mod commit_pretty;
pub mod commit_trailers;
pub mod config;
pub mod connectivity;
pub mod credentials;
pub mod crlf;
pub mod delta_encode;
pub mod delta_islands;
pub mod diagnostics;
pub mod diff;
pub mod diff_indent_heuristic;
mod diff_rename_parallel;
pub mod diffstat;
pub mod dotfile;
pub mod environment;
pub mod error;
mod ewah_bitmap;

#[doc(hidden)]
pub mod ewah_bench {
    include!("ewah_bench_api.rs");
}
pub mod bitmap_walk;
pub mod fetch;
pub mod fetch_head;
pub mod fetch_negotiator;
pub mod filter_process;
pub mod fmt_merge_msg;
pub mod fsck_standalone;
pub mod gc;
pub mod git_binary_base85;
pub mod git_date;
pub mod git_path;
pub mod gitmodules;
pub mod hash;
pub mod hide_refs;
pub mod hooks;
pub mod ident;
pub mod ident_config;
pub mod ident_resolve;
pub mod ignore;
pub mod index;
mod index_eoie_ieot;
pub mod index_name_hash_lazy;
pub mod index_pack;
pub mod init_filesystem;
pub mod interpret_trailers;
pub(crate) mod local_object_copy;
pub mod mailmap;
pub mod merge_base;
pub mod merge_diff;
pub mod merge_file;
pub mod merge_trees;
pub mod midx;
pub mod midx_error;
pub mod net_trace;
pub mod notes;
pub mod objects;
pub mod odb;
pub mod pack;
pub mod pack_bitmap;
mod pack_delta;
mod pack_index;
pub(crate) mod pack_index_build;
mod pack_map;
pub mod pack_name_hash;
pub mod pack_object_select;
pub mod pack_objects;
pub mod pack_receive;
pub mod pack_rev;
pub mod pack_store;
pub(crate) mod pack_zlib;
pub mod parallel;
pub mod patch_ids;
pub mod path;
pub mod path_icase;
pub mod pathspec;
pub mod pkt_line;
pub mod precompose_config;
pub mod promisor;
pub mod promisor_remote;
pub mod protocol;
pub mod protocol_v2;
pub mod prune_packed;
pub mod push;
pub mod push_cert;
pub mod push_report;
pub mod quote_path;
pub mod receive_pack;
pub mod receive_quarantine;
pub mod ref_exclusions;
pub mod ref_namespace;
pub mod ref_storage;
pub mod reflog;
pub mod refs;
pub mod refs_fsck;
pub mod refspec;
pub mod reftable;
pub mod remote;
pub mod repo;
pub mod repo_caches;
pub mod rerere;
pub mod resolve_undo;
pub mod rev_list;
pub mod rev_list_error;
pub mod rev_parse;
pub mod rev_parse_error;
pub mod serve;
pub mod shallow;
pub mod shared_repo;
pub mod sideband_lines;
pub mod signing;
pub mod sparse_checkout;
pub mod split_index;
pub mod state;
pub mod stripspace;
pub mod submodule_active;
pub mod submodule_config;
pub mod submodule_gitdir;
pub mod terminal;
pub mod textconv_cache;
pub mod trace;
mod tracked_path_tree;
pub mod transfer;
pub mod transport;
pub mod transport_path;
pub mod tree_path_follow;
pub mod unicode_normalization;
pub mod unpack_objects;
pub mod untracked_cache;
pub mod upload_filter;
pub mod url_rewrite;
pub mod userdiff;
pub mod whitespace_rule;
pub mod wildmatch;
pub mod worktree;
mod worktree_batch;
pub mod worktree_cwd;
pub mod worktree_ref;
pub mod worktree_rules;
mod worktree_rules_tests;
mod worktree_scan;
pub use worktree_scan::PARALLEL_STAT_MIN_ENTRIES;
pub mod write_tree;
pub mod ws;
#[doc(hidden)]
pub mod zlib_inflate;

pub use ref_storage::RefStorageFormat;

#[doc(hidden)]
pub mod hot_path_test_metrics;
#[cfg(test)]
mod object_write_regression;
#[cfg(test)]
mod repository_config_snapshot_tests;
