//! Git smart HTTP serving for grit.
//!
//! - [`smart_http`] answers the four smart HTTP endpoints (`info/refs` discovery
//!   and the `git-upload-pack` / `git-receive-pack` RPCs) for one repository,
//!   running upload-pack and receive-pack in-process through [`grit_protocol`].
//! - [`store`] keeps users (email addresses with passwords), web sessions,
//!   access tokens and per-repository read/write grants in a SQLite database.
//! - [`server`] is the multi-repository server behind `grit serve`: it
//!   authenticates Git requests with tokens from the [`store`] and serves fetch
//!   (protocol v0, v1 and v2) and push for each repository it finds.
//! - [`web`] renders the web interface: sign-in, repository pages, and user
//!   management for admins.
//! - [`login`] tracks `grit auth <url>` sign-ins waiting for the user to
//!   approve them in the browser.
//! - [`bundles`] builds each repository's clone bundle in the background and
//!   offers it through `bundle-uri`, so clones can download it resumably.
//! - [`checkouts`] serves snapshot checkouts: a repository's target branch,
//!   cut down to some paths, as one commit (`grit checkout`).
//! - [`uploads`] accepts resumable pushes: bundles uploaded over TUS to the
//!   endpoint receive-pack advertises as `pushBundle`.

pub mod bundles;
pub mod checkouts;
pub mod login;
pub mod server;
pub mod smart_http;
pub mod store;
pub mod uploads;
pub mod web;
