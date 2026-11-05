//! Commit-graph chain helpers for integration tests.

use crate::commit_graph_file::{CommitGraphChain, CommitGraphLayer};

/// Build a commit-graph chain from tip-first layers (test-only).
pub fn chain_from_layers(layers: Vec<CommitGraphLayer>) -> CommitGraphChain {
    crate::commit_graph_file::commit_graph_chain_from_layers(layers)
}
