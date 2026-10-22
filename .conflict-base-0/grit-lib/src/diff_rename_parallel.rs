//! Parallel blob loads and rename/copy score-matrix rows for [`crate::diff`].

use std::num::NonZeroUsize;

use crate::config::ConfigSet;
use crate::objects::pack_store::PackReadContextGuard;
use crate::odb::Odb;
use crate::parallel::{
    try_par_map_with_local, ParallelHashError, ParallelOp, ParallelOptions, ThreadPolicy,
};
use std::sync::Arc;

/// Git's default `diff.renameLimit` when unset.
pub(crate) const DEFAULT_RENAME_LIMIT: usize = 1000;

/// Parsed `diff.renameLimit` (`None` = unlimited, including explicit `0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenameLimit {
    max: Option<NonZeroUsize>,
}

impl RenameLimit {}

/// Resolve `diff.renameLimit` the way Git does for inexact rename/copy matrices.
#[must_use]
pub(crate) fn parse_rename_limit(config: &ConfigSet) -> RenameLimit {
    let raw = config
        .get_i64("diff.renameLimit")
        .or_else(|| config.get_i64("diff.renamelimit"))
        .and_then(|r| r.ok());
    match raw {
        Some(n) if n <= 0 => RenameLimit { max: None },
        Some(n) => usize::try_from(n)
            .ok()
            .and_then(NonZeroUsize::new)
            .map(|nz| RenameLimit { max: Some(nz) })
            .unwrap_or(RenameLimit { max: None }),
        None => RenameLimit {
            max: NonZeroUsize::new(DEFAULT_RENAME_LIMIT),
        },
    }
}

/// Return `true` when Git would skip **inexact** rename detection for this matrix shape.
#[must_use]
pub(crate) fn inexact_matrix_exceeds_limit(
    num_destinations: usize,
    num_sources: usize,
    limit: RenameLimit,
) -> bool {
    let Some(max) = limit.max else {
        return false;
    };
    let cap = max.get().saturating_mul(max.get());
    num_destinations.saturating_mul(num_sources) > cap
}

/// Resolve pool options for rename/copy similarity work.
#[must_use]
pub(crate) fn diff_rename_parallel_options(
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> ParallelOptions {
    thread_policy.resolve(ParallelOp::DiffRename, config)
}

/// Load `count` blob payloads in parallel when policy thresholds allow.
pub(crate) fn parallel_load_optional_blobs<F>(
    odb: &Odb,
    count: usize,
    options: ParallelOptions,
    total_bytes_hint: usize,
    load_index: F,
) -> Vec<Option<Vec<u8>>>
where
    F: Fn(usize) -> Option<Vec<u8>> + Sync,
{
    if count == 0 {
        return Vec::new();
    }
    let indices: Vec<usize> = (0..count).collect();
    let store = odb.pack_store_for(odb.objects_dir());
    match try_par_map_with_local(
        &indices,
        options,
        total_bytes_hint,
        || PackReadContextGuard::install(Arc::clone(&store)),
        |_ctx, &i| Ok(load_index(i)),
    ) {
        Ok(rows) => rows,
        Err(ParallelHashError::Task(err)) => err,
    }
}

/// Map `0..len` with the rename/copy parallel policy inside a pack read context.
pub(crate) fn parallel_map_indices<R, E, F>(
    odb: &Odb,
    len: usize,
    options: ParallelOptions,
    total_bytes_hint: usize,
    f: F,
) -> Result<Vec<R>, E>
where
    R: Send,
    E: Send + 'static,
    F: Fn(usize) -> Result<R, E> + Sync,
{
    if len == 0 {
        return Ok(Vec::new());
    }
    let indices: Vec<usize> = (0..len).collect();
    let store = odb.pack_store_for(odb.objects_dir());
    try_par_map_with_local(
        &indices,
        options,
        total_bytes_hint,
        || PackReadContextGuard::install(Arc::clone(&store)),
        |_ctx, &i| f(i),
    )
    .map_err(|ParallelHashError::Task(err)| err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};
    use std::path::Path;

    fn config_from(text: &str) -> ConfigSet {
        let file = ConfigFile::parse(Path::new("test"), text, ConfigScope::Local).expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn rename_limit_defaults_to_git_value() {
        let cfg = ConfigSet::new();
        assert_eq!(
            parse_rename_limit(&cfg).max.map(NonZeroUsize::get),
            Some(DEFAULT_RENAME_LIMIT)
        );
    }

    #[test]
    fn rename_limit_zero_means_unlimited() {
        let cfg = config_from("[diff]\n\trenameLimit = 0\n");
        assert_eq!(parse_rename_limit(&cfg).max, None);
        assert!(!inexact_matrix_exceeds_limit(
            10_000,
            10_000,
            parse_rename_limit(&cfg)
        ));
    }

    #[test]
    fn inexact_matrix_uses_limit_squared() {
        let limit = RenameLimit {
            max: NonZeroUsize::new(1000),
        };
        assert!(!inexact_matrix_exceeds_limit(1000, 1000, limit));
        assert!(inexact_matrix_exceeds_limit(1001, 1000, limit));
        assert!(inexact_matrix_exceeds_limit(1000, 1001, limit));
    }
}
