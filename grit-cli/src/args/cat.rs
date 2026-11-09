//! Clap arguments for `grit cat`.

use clap::Args;

/// Show blob, tree, commit, or tag contents for a revision spec.
#[derive(Debug, Args)]
pub(crate) struct CatArgs {
    /// Object or revision spec (`HEAD`, `main:path`, `v1^{commit}`, …).
    pub(crate) spec: String,
}
