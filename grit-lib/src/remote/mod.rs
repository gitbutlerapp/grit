//! Remotes, fetch/push, transport, and credential helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/network)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.remotes();
//!
//! # Ok(())
//! # }
//! ```
mod core;
pub mod push_plan;

pub use core::*;

pub mod fetch {
    //! fetch API (re-exported at this path).
    pub use crate::fetch::*;
}
pub mod fetch_request {
    //! Explicit fetch refspec resolution.
    pub use crate::fetch_request::*;
}
pub mod push {
    //! push API (re-exported at this path).
    pub use crate::push::*;
}
pub mod credentials {
    //! credentials API (re-exported at this path).
    pub use crate::credentials::*;
}
pub mod github_auth {
    //! github_auth API (re-exported at this path).
    pub use crate::github_auth::*;
}
pub mod url_rewrite {
    //! url_rewrite API (re-exported at this path).
    pub use crate::url_rewrite::*;
}
pub mod transfer {
    //! transfer API (re-exported at this path).
    pub use crate::transfer::*;
}
pub mod transport {
    //! transport API (re-exported at this path).
    pub use crate::transport::*;
}
pub mod transport_path {
    //! transport_path API (re-exported at this path).
    pub use crate::transport_path::*;
}
pub mod protocol {
    //! protocol API (re-exported at this path).
    pub use crate::protocol::*;
}
pub mod protocol_v2 {
    //! protocol_v2 API (re-exported at this path).
    pub use crate::protocol_v2::*;
}
pub mod shallow {
    //! shallow API (re-exported at this path).
    pub use crate::shallow::*;
}
pub mod fetch_head {
    //! fetch_head API (re-exported at this path).
    pub use crate::fetch_head::*;
}
pub mod fetch_negotiator {
    //! fetch_negotiator API (re-exported at this path).
    pub use crate::fetch_negotiator::*;
}
pub mod receive_pack {
    //! receive_pack API (re-exported at this path).
    pub use crate::receive_pack::*;
}
pub mod push_report {
    //! push_report API (re-exported at this path).
    pub use crate::push_report::*;
}
pub mod clone {
    //! clone API (re-exported at this path).
    pub use crate::clone::*;
}
pub mod partial_clone {
    //! Partial-clone filter specs and promisor-remote config.
    pub use crate::partial_clone::*;
}
pub mod serve {
    //! serve API (re-exported at this path).
    pub use crate::serve::receive_pack;
    pub use crate::serve::upload_pack;
    pub use crate::serve::*;
}
pub mod pkt_line {
    //! pkt_line API (re-exported at this path).
    pub use crate::pkt_line::*;
}
pub mod sideband_lines {
    //! sideband_lines API (re-exported at this path).
    pub use crate::sideband_lines::*;
}
