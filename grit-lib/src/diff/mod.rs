//! Tree and content diffs, rename detection, merge diffs, and related helpers.
mod core;
pub mod handle;

pub use core::*;
pub use handle::Diff;

pub mod bloom {
    //! Canonical re-export of [`crate::bloom`].
    pub use crate::bloom::*;
}
pub mod combined_diff_patch {
    //! Canonical re-export of [`crate::combined_diff_patch`].
    pub use crate::combined_diff_patch::*;
}
pub mod combined_tree_diff {
    //! Canonical re-export of [`crate::combined_tree_diff`].
    pub use crate::combined_tree_diff::*;
}
pub mod diffstat {
    //! Canonical re-export of [`crate::diffstat`].
    pub use crate::diffstat::*;
}
pub mod patch_ids {
    //! Canonical re-export of [`crate::patch_ids`].
    pub use crate::patch_ids::*;
}
pub mod userdiff {
    //! Canonical re-export of [`crate::userdiff`].
    pub use crate::userdiff::*;
}
pub mod merge_diff {
    //! Canonical re-export of [`crate::merge_diff`].
    pub use crate::merge_diff::*;
}
pub mod merge_file {
    //! Canonical re-export of [`crate::merge_file`].
    pub use crate::merge_file::*;
}
pub mod merge_trees {
    //! Canonical re-export of [`crate::merge_trees`].
    pub use crate::merge_trees::*;
}
pub mod rerere {
    //! Canonical re-export of [`crate::rerere`].
    pub use crate::rerere::*;
}
pub mod fmt_merge_msg {
    //! Canonical re-export of [`crate::fmt_merge_msg`].
    pub use crate::fmt_merge_msg::*;
}
