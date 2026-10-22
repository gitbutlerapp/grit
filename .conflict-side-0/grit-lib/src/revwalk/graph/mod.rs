//! Commit graph layout for `log --graph` (lane topology and ASCII prefixes).
//!
//! [`GraphLayout`] consumes commits in display order (newest first, topological when
//! requested) with display parents and emits graph line prefixes. Parent lists should
//! already be rewritten so filtered-out commits are skipped while keeping connectivity.

mod layout;
mod parents;
mod walk;

pub use layout::{
    GraphCharset, GraphCommitInput, GraphLayout, GraphLineKind, GraphRenderedLine, GraphRowTopology,
};
pub use parents::{
    display_parents_for_graph, log_graph_materialize, rewrite_display_parents,
    rewrite_display_parents_map,
};
pub use walk::list_commits_for_log_graph;
