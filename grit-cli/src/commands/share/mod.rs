//! `grit share` — work on a branch together with other users of a grit
//! hosting server such as grithub.
//!
//! A shared branch is in every member's space, and each member has their
//! own head of it: only you move yours, so nobody's push can overwrite
//! anyone else's work, and pushing your head after an amend or rebase needs
//! no `--force`. The other members' heads are fetched beside yours as
//! `<remote>/shared/<branch>/<user>`.
//!
//! - `grit share add <branch> [<user>...] [--link]` shares a branch of your
//!   space (pushing it first when it isn't there yet), adds people, or makes
//!   a share link anyone signed in can accept.
//! - `grit share list [<branch>]` shows your shares: who is on each, where
//!   their heads are, and how each compares with your branch here.
//! - `grit share accept <link>` accepts a link and fetches the branch (or
//!   clones your copy when you aren't in a clone of the project).
//! - `grit share remove <branch> [<user>...]` removes people, or ends the
//!   share; `grit share leave <branch>` leaves it; `grit share revoke
//!   <branch> <link-id>` revokes a link.

pub use crate::render::share::*;

pub mod divergence;
pub mod integrated;
pub mod ops;

pub(crate) use divergence::{auto_combine, compare, explain};
pub(crate) use integrated::integrated_on;
pub use ops::{accept, add, leave, list, remove, resolve, revoke};

mod dispatch;
pub use dispatch::dispatch;
