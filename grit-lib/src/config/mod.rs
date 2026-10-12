//! Configuration cascade, identity, and repository bootstrap helpers.
mod core;
pub mod handle;

pub use core::*;
pub use handle::ConfigHandle;

pub mod dotfile {
    //! Canonical re-export of [`crate::dotfile`].
    pub use crate::dotfile::*;
}
pub mod environment {
    //! Canonical re-export of [`crate::environment`].
    pub use crate::environment::*;
}
pub mod gitmodules {
    //! Canonical re-export of [`crate::gitmodules`].
    pub use crate::gitmodules::*;
}
pub mod ident {
    //! Canonical re-export of [`crate::ident`].
    pub use crate::ident::*;
}
pub mod ident_config {
    //! Canonical re-export of [`crate::ident_config`].
    pub use crate::ident_config::*;
}
pub mod ident_resolve {
    //! Canonical re-export of [`crate::ident_resolve`].
    pub use crate::ident_resolve::*;
}
pub mod init_filesystem {
    //! Canonical re-export of [`crate::init_filesystem`].
    pub use crate::init_filesystem::*;
}
pub mod precompose_config {
    //! Canonical re-export of [`crate::precompose_config`].
    pub use crate::precompose_config::*;
}
