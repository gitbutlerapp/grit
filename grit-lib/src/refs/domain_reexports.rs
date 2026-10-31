//! Canonical nested paths for ref-adjacent modules (flat [`crate::reflog`] paths remain valid).

pub mod reflog {
    //! reflog API (re-exported at this path).
    pub use crate::reflog::*;
}
pub mod refspec {
    //! refspec API (re-exported at this path).
    pub use crate::refspec::*;
}
pub mod reftable {
    //! reftable API (re-exported at this path).
    pub use crate::reftable::*;
}
pub mod check_ref_format {
    //! check_ref_format API (re-exported at this path).
    pub use crate::check_ref_format::*;
}
pub mod ref_namespace {
    //! ref_namespace API (re-exported at this path).
    pub use crate::ref_namespace::*;
}
pub mod hide_refs {
    //! hide_refs API (re-exported at this path).
    pub use crate::hide_refs::*;
}
pub mod ref_storage {
    //! ref_storage API (re-exported at this path).
    pub use crate::ref_storage::*;
}
pub mod branch_tracking {
    //! branch_tracking API (re-exported at this path).
    pub use crate::branch_tracking::*;
}
pub mod refs_fsck {
    //! refs_fsck API (re-exported at this path).
    pub use crate::refs_fsck::*;
}
