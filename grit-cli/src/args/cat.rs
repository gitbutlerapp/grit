//! Clap arguments for `grit cat`.

use crate::branch_list_args;
use crate::clap_value_args;
use crate::diff_args;
use crate::diff_format;
use crate::global_args;
use crate::limit_args;
use crate::merge_strategy_args;
use crate::message_input;
use crate::pathspec_args;
use crate::quiet_args;
use crate::rev_select;
use clap::Args;

/// Show blob, tree, commit, or tag contents for a revision spec.
#[derive(Debug, Args)]
pub(crate) struct CatArgs {
    /// Object or revision spec (`HEAD`, `main:path`, `v1^{commit}`, …).
    pub(crate) spec: String,
}
