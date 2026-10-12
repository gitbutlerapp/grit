//! Git index (staging area) read/write and related cache extensions.
mod core;
pub mod handle;

pub use core::*;
pub use handle::IndexHandle;

pub mod sparse_checkout {
    //! Canonical re-export of [`crate::sparse_checkout`].
    pub use crate::sparse_checkout::*;
}
pub mod split_index {
    //! Canonical re-export of [`crate::split_index`].
    pub use crate::split_index::*;
}
pub mod untracked_cache {
    //! Canonical re-export of [`crate::untracked_cache`].
    pub use crate::untracked_cache::*;
}
pub mod resolve_undo {
    //! Canonical re-export of [`crate::resolve_undo`].
    pub use crate::resolve_undo::*;
}
pub mod index_name_hash_lazy {
    //! Canonical re-export of [`crate::index_name_hash_lazy`].
    pub use crate::index_name_hash_lazy::*;
}
pub mod write_tree {
    //! Canonical re-export of [`crate::write_tree`].
    pub use crate::write_tree::*;
}
