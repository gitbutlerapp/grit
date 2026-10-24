//! Git smart HTTP serving for grit.
//!
//! - [`smart_http`] answers the four smart HTTP endpoints (`info/refs` discovery
//!   and the `git-upload-pack` / `git-receive-pack` RPCs) for one repository,
//!   running upload-pack and receive-pack in-process through [`grit_protocol`].
//! - [`store`] keeps users, access tokens and per-repository read/write grants
//!   in a SQLite database.
//! - [`server`] is the multi-repository server behind `grit serve`: it
//!   authenticates every request against the [`store`], serves fetch (protocol
//!   v0, v1 and v2) and push for each repository it finds, and renders a small
//!   web interface listing those repositories.

pub mod server;
pub mod smart_http;
pub mod store;
