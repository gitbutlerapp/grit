//! Working-tree status for one repository ([`Status`] from [`Repository::status`](crate::repo::Repository::status)).

mod handle;

pub use handle::{Status, UntrackedCachePolicy, UntrackedOptions};

pub use crate::porcelain::status::{
    format_porcelain_v2, short_status_lines, ConflictGroup, IgnoredMode, InterruptedRewrite,
    RenameDetection, StatusAction, StatusGuidance, StatusModel, StatusOptions,
    UntrackedCollectMeta, UntrackedMode,
};
