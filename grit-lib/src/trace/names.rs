//! Stable span names for tests and CLI trace output (must match `trace_span!` literals).

#![allow(dead_code)]

/// [`Repository::open`](crate::repo::Repository::open) and discovery setup.
pub const REPOSITORY_OPEN: &str = "grit.repository.open";
/// Config session refresh and file load.
pub const CONFIG_LOAD: &str = "grit.config.load";
/// Index read from disk (including split-index merge).
pub const INDEX_READ: &str = "grit.index.read";
/// Index write to disk.
pub const INDEX_WRITE: &str = "grit.index.write";
/// Working-tree status scan ([`crate::porcelain::status::status`]).
pub const STATUS_SCAN: &str = "grit.status.scan";
/// Rev-list / rev-walk setup ([`crate::rev_list::rev_list`]).
pub const REVWALK_SETUP: &str = "grit.revwalk.setup";
/// Tree diff ([`crate::diff::diff_trees`]).
pub const DIFF_TREE: &str = "grit.diff.tree";
/// Blob content comparison ([`crate::diff::diffcore_count_changes`]).
pub const DIFF_BLOB: &str = "grit.diff.blob";
/// Checkout plan construction.
pub const CHECKOUT_PLAN: &str = "grit.checkout.plan";
/// Checkout worktree / index apply.
pub const CHECKOUT_WRITE: &str = "grit.checkout.write";
/// Pack `.idx` open and parse.
pub const PACK_INDEX_OPEN: &str = "grit.pack.index.open";
/// Pack index build with delta resolution batches.
pub const PACK_DELTA_RESOLVE: &str = "grit.pack.delta.resolve";
/// Fetch upload-pack negotiation.
pub const FETCH_NEGOTIATE: &str = "grit.fetch.negotiate";
/// Side-band pack receive.
pub const PACK_RECEIVE: &str = "grit.pack.receive";
/// Received pack install / index-pack.
pub const PACK_INDEX: &str = "grit.pack.index";
/// Push over git-receive-pack.
pub const PUSH: &str = "grit.push";
