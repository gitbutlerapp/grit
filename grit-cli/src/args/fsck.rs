//! Clap arguments for `grit fsck`.

use clap::Args;

/// Check refs, objects, and pack indexes for problems.
#[derive(Debug, Args)]
pub(crate) struct FsckArgs {
    /// Check connectivity from refs only (skip store-wide object validation).
    #[arg(long = "connectivity-only")]
    pub(crate) connectivity_only: bool,
    /// Report unreachable objects (with `--connectivity-only`, off unless this flag is set).
    #[arg(long)]
    pub(crate) dangling: bool,
    /// Do not list dangling objects (matches Git `--no-dangling`).
    #[arg(long = "no-dangling")]
    pub(crate) no_dangling: bool,
    /// Treat ref warnings as errors.
    #[arg(long)]
    pub(crate) strict: bool,
    /// Include info-level object diagnostics in human output (matches Git `--verbose`).
    #[arg(short, long)]
    pub(crate) verbose: bool,
}
