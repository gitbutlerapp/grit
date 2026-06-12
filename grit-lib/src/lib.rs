//! Gust library — core Git-compatible engine.
//!
//! # Architecture
//!
//! All Git-compatible logic lives here; the `grit` binary is a thin CLI shim
//! that parses arguments and delegates to types exposed from this crate.
//!
//! ## Where to start
//!
//! - [`Repository`](repo::Repository) is the central handle — open or discover
//!   one, then reach the object store, index, refs, and config through it.
//! - [`prelude`] re-exports the handful of types most call sites need.
//! - [`porcelain`] holds user-facing operations that return structured result
//!   *models* (the CLI renders them); [`plumbing`] holds the low-level,
//!   machine-stable building blocks. The flat module list below remains the
//!   low-level escape hatch and keeps every existing path valid.
//! - Browse the engine by subsystem via the domain views: [`object_store`],
//!   [`references`], [`worktree_index`], [`revision`], [`diffing`], [`merging`],
//!   and [`configuration`].
//! - [`progress`] defines how long-running operations report progress and
//!   observe cancellation without touching the terminal.
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
pub mod plumbing;
pub mod porcelain;
pub mod progress;

/// The types most callers need, re-exported for `use grit_lib::prelude::*;`.
pub mod prelude {
    pub use crate::config::ConfigSet;
    pub use crate::error::{Error, Result};
    pub use crate::index::Index;
    pub use crate::objects::{Object, ObjectId, ObjectKind};
    pub use crate::odb::Odb;
    pub use crate::repo::Repository;
}

// Domain-grouped views of the engine. Each is a curated table of contents over
// the flat module list below — pure module re-exports, so `object_store::objects`
// and the original `objects` path both resolve. A module appears in at most one
// domain; cross-cutting and operation-level modules stay in the flat list.

/// Object storage: ids/kinds, the object database, packs, multi-pack index, deltas.
pub mod object_store {
    pub use crate::{
        delta_encode, delta_islands, midx, objects, odb, pack, pack_geometry, pack_name_hash,
        pack_rev, promisor, promisor_remote, prune_packed, unpack_objects,
    };
}

/// References: the refs backends, reflog, refspecs, name validation, namespaces.
pub mod references {
    pub use crate::{
        branch_ref_format, branch_tracking, check_ref_format, hide_refs, ref_exclusions,
        ref_namespace, reflog, refs, refspec, reftable,
    };
}

/// Index and working tree: the index, sparse checkout, attributes, ignore, CRLF.
pub mod worktree_index {
    pub use crate::{
        attributes, crlf, ignore, index, index_name_hash_lazy, path_walk, resolve_undo,
        sparse_checkout, split_index, untracked_cache, worktree, worktree_cwd, worktree_ref,
        write_tree,
    };
}

/// Revision machinery: rev-parse, rev-list, name-rev, commit-graph.
pub mod revision {
    pub use crate::{commit_graph_file, commit_graph_write, name_rev, rev_list, rev_parse};
}

/// Diffing: tree/content diff, rename detection, diffstat, line-log, pickaxe bloom.
pub mod diffing {
    pub use crate::{
        bloom, combined_diff_patch, combined_tree_diff, diff, diff_indent_heuristic, diff_moved,
        diffstat, difftool, line_log, patch_ids, userdiff,
    };
}

/// Merging: merge-base, tree/file merges, rerere, merge-message formatting.
pub mod merging {
    pub use crate::{
        fmt_merge_msg, merge_base, merge_diff, merge_file, merge_tree_trivial, merge_trees,
        mergetool_vimdiff, rerere,
    };
}

/// Configuration and identity: config cascade, .gitmodules, author/committer idents.
pub mod configuration {
    pub use crate::{
        config, dotfile, gitmodules, ident, ident_config, ident_resolve, precompose_config,
        url_rewrite,
    };
}

pub mod am;
pub mod apply;
pub mod attributes;
pub mod blame;
pub mod bloom;
pub mod branch_ref_format;
pub mod branch_tracking;
pub mod check_ref_format;
pub mod combined_diff_patch;
pub mod combined_tree_diff;
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
pub mod diff;
pub mod diff_indent_heuristic;
pub mod diff_moved;
pub mod diffstat;
pub mod difftool;
pub mod dotfile;
pub mod error;
mod ewah_bitmap;
pub mod fast_export;
pub mod fast_import;
pub mod fetch;
pub mod fetch_head;
pub mod fetch_negotiator;
pub mod fetch_submodules;
pub mod filter_process;
pub mod fmt_merge_msg;
pub mod fsck_standalone;
pub mod gc;
pub mod git_binary_base85;
pub mod git_column;
pub mod git_date;
pub mod git_path;
pub mod gitmodules;
pub mod hide_refs;
pub mod hooks;
pub mod ident;
pub mod ident_config;
pub mod ident_resolve;
pub mod ignore;
pub mod index;
pub mod index_name_hash_lazy;
pub mod interpret_trailers;
pub mod line_log;
pub mod ls_remote;
pub mod mailinfo;
pub mod mailmap;
pub mod merge_base;
pub mod merge_diff;
pub mod merge_file;
pub mod merge_tree_trivial;
pub mod merge_trees;
pub mod mergetool_vimdiff;
pub mod midx;
pub mod name_rev;
pub mod net_trace;
pub mod notes;
pub mod objects;
pub mod odb;
pub mod pack;
pub mod pack_geometry;
pub mod pack_name_hash;
pub mod pack_rev;
#[cfg(feature = "test-tools")]
pub mod parse_options_test_tool;
pub mod patch_ids;
pub mod path_walk;
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
pub mod push_submodules;
pub mod quote_path;
pub mod receive_pack;
pub mod ref_exclusions;
pub mod ref_namespace;
pub mod reflog;
pub mod refs;
pub mod refs_fsck;
pub mod refspec;
pub mod reftable;
pub mod repo;
pub mod rerere;
pub mod resolve_undo;
pub mod rev_list;
pub mod rev_parse;
pub mod shallow;
pub mod shared_repo;
pub mod signing;
#[cfg(unix)]
pub mod simple_ipc;
pub mod sparse_checkout;
pub mod split_index;
pub mod unicode_normalization;
pub mod untracked_cache;
pub mod upload_filter;
#[cfg(not(unix))]
pub mod simple_ipc {
    /// Whether simple IPC is supported on this platform.
    #[must_use]
    pub fn supports_simple_ipc() -> bool {
        false
    }

    /// Stub for non-Unix targets.
    pub fn run_simple_ipc_tool(_args: &[String]) -> i32 {
        eprintln!("simple IPC not available on this platform");
        1
    }
}
pub mod state;
pub mod stripspace;
pub mod submodule_active;
pub mod submodule_config;
pub mod submodule_config_cache;
pub mod submodule_gitdir;
pub mod tab_expand;
#[cfg(feature = "test-tools")]
pub mod test_tool_progress;
pub mod textconv_cache;
pub mod transfer;
pub mod transport;
pub mod transport_path;
pub mod tree_path_follow;
#[cfg(unix)]
pub mod unix_process;
pub mod unpack_objects;
pub mod url_rewrite;
pub mod userdiff;
pub mod whitespace_rule;
pub mod wildmatch;
pub mod worktree;
pub mod worktree_cwd;
pub mod worktree_ref;
pub mod write_tree;
pub mod ws;
