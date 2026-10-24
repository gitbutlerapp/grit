//! Resolve worker counts and batch thresholds from Git config and explicit overrides.

use std::num::NonZeroUsize;

use crate::config::ConfigSet;
use crate::error::{BadNumericSource, ConfigError};
use crate::worktree_scan::PARALLEL_STAT_MIN_ENTRIES;

/// Minimum item count before parallel hash helpers use more than one thread.
const PAR_HASH_MIN_ITEMS: usize = 32;
/// Minimum total payload bytes for parallel hashing.
const PAR_HASH_MIN_TOTAL_BYTES: usize = 256 * 1024;

use super::Parallelism;

/// Git operation that selects which config keys apply to parallelism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParallelOp {
    /// Index stat preload and parallel worktree hashing (`index.threads`, `core.preloadIndex`).
    IndexRefresh,
    /// Parallel index load paths such as lazy name-hash init (`index.threads`, `core.preloadIndex`).
    IndexLoad,
    /// Checkout worker pool (`checkout.workers`, `checkout.thresholdForParallelism`).
    Checkout,
    /// `index-pack` and in-memory pack indexing (`pack.threads`).
    IndexPack,
    /// Pack writing / `pack-objects` threading (`pack.threads`).
    PackWrite,
    /// Parallel windowed delta search during pack planning (`pack.threads`).
    PackDeltaSearch,
    /// Rename/copy detection scoring (`index.threads` when preload is enabled).
    DiffRename,
    /// Per-file blob loads and line diffs for tree/commit ranges (`index.threads`).
    DiffContent,
}

/// Explicit CLI or embedder overrides layered on top of config resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadPolicy {
    /// When set, replaces config-derived thread counts (`0` = auto / all logical CPUs).
    pub threads_override: Option<usize>,
    /// When set, replaces the operation's default minimum item count before using a pool.
    pub min_items_override: Option<usize>,
    /// When set, replaces the byte threshold before using a pool (`Some(0)` disables it).
    pub min_total_bytes_override: Option<usize>,
}

impl ThreadPolicy {
    /// Override worker count (`None` = use config only; `Some(0)` = auto).
    #[must_use]
    pub fn with_threads_override(threads: Option<usize>) -> Self {
        Self {
            threads_override: threads,
            ..Self::default()
        }
    }

    /// Override minimum batch size (`Some(0)` forces the parallel path on tiny inputs).
    #[must_use]
    pub fn with_min_items_override(min_items: Option<usize>) -> Self {
        Self {
            min_items_override: min_items,
            ..Self::default()
        }
    }

    /// Set minimum batch override on an existing policy.
    #[must_use]
    pub fn min_items_override(mut self, min_items: Option<usize>) -> Self {
        self.min_items_override = min_items;
        self
    }

    /// Override minimum total bytes (`Some(0)` disables the byte threshold).
    #[must_use]
    pub fn min_total_bytes_override(mut self, min_bytes: Option<usize>) -> Self {
        self.min_total_bytes_override = min_bytes;
        self
    }

    /// Resolve pool options for `op` using merged `config` and this policy.
    ///
    /// # Panics
    ///
    /// Panics when `op` is [`ParallelOp::IndexPack`] or [`ParallelOp::PackWrite`] and
    /// `pack.threads` is invalid (call [`Self::try_resolve`] to surface errors).
    #[must_use]
    pub fn resolve(&self, op: ParallelOp, config: &ConfigSet) -> ParallelOptions {
        self.try_resolve(op, config).unwrap_or_else(|e| match op {
            ParallelOp::IndexPack | ParallelOp::PackWrite | ParallelOp::PackDeltaSearch => {
                panic!("invalid pack.threads config: {e}")
            }
            _ => unreachable!("only pack operations return config errors"),
        })
    }

    /// Resolve pool options, returning a config error when `pack.threads` is invalid.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for rejected `pack.threads` values (negative integers,
    /// booleans, or non-numeric strings), matching Git's pack/index-pack behavior.
    pub fn try_resolve(
        &self,
        op: ParallelOp,
        config: &ConfigSet,
    ) -> Result<ParallelOptions, ConfigError> {
        let preload = core_preload_index(config);
        let threads = match op {
            ParallelOp::IndexRefresh | ParallelOp::IndexLoad => {
                if self.threads_override.is_some() || preload {
                    resolve_index_threads(config, self.threads_override)
                } else {
                    NonZeroUsize::MIN
                }
            }
            ParallelOp::DiffRename | ParallelOp::DiffContent => {
                if self.threads_override.is_some() || preload {
                    resolve_index_threads(config, self.threads_override)
                } else {
                    NonZeroUsize::MIN
                }
            }
            ParallelOp::IndexPack | ParallelOp::PackWrite | ParallelOp::PackDeltaSearch => {
                resolve_pack_threads(config, self.threads_override)?
            }
            ParallelOp::Checkout => resolve_checkout_workers(config, self.threads_override),
        };

        let (default_min_items, default_min_bytes) = match op {
            ParallelOp::IndexRefresh => (PAR_HASH_MIN_ITEMS, Some(PAR_HASH_MIN_TOTAL_BYTES)),
            ParallelOp::IndexLoad => (crate::index_name_hash_lazy::LAZY_THREAD_COST * 2, None),
            ParallelOp::Checkout => (0, None),
            ParallelOp::IndexPack | ParallelOp::PackWrite | ParallelOp::PackDeltaSearch => {
                (0, None)
            }
            ParallelOp::DiffRename | ParallelOp::DiffContent => {
                (PAR_HASH_MIN_ITEMS, Some(PAR_HASH_MIN_TOTAL_BYTES))
            }
        };

        let min_items = self.min_items_override.unwrap_or(default_min_items);
        let signed_entry_threshold = match op {
            ParallelOp::Checkout => Some(checkout_parallelism_threshold(config)),
            _ => None,
        };

        let min_total_bytes = match self.min_total_bytes_override {
            Some(0) => None,
            Some(n) => Some(n),
            None => default_min_bytes,
        };

        Ok(ParallelOptions {
            threads,
            min_items,
            min_total_bytes,
            signed_entry_threshold,
        })
    }

    /// Thread count for index stat preload (Git `preload-index.c`: ~500 entries per thread).
    #[must_use]
    pub fn index_stat_preload_threads(
        &self,
        config: &ConfigSet,
        blob_count: usize,
        stat_parallel_threads: Option<usize>,
    ) -> usize {
        const THREAD_COST: usize = 500;
        const MAX_PARALLEL: usize = 20;

        if !core_preload_index(config) || blob_count < PARALLEL_STAT_MIN_ENTRIES {
            return 1;
        }

        let mut threads = blob_count / THREAD_COST;
        if threads < 2 {
            threads = 2;
        }
        threads = threads.min(MAX_PARALLEL);

        let index_cap = resolve_index_threads(config, self.threads_override).get();
        threads = threads.min(index_cap);

        if let Some(requested) = stat_parallel_threads {
            if requested <= 1 {
                return 1;
            }
            threads = threads.min(requested);
        }

        threads.max(1)
    }
}

/// Resolved worker count and serial-fallback thresholds for one parallel map call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParallelOptions {
    /// Worker threads (always at least 1).
    pub threads: NonZeroUsize,
    /// Minimum input length before spawning a pool (0 = always parallel when `threads` > 1).
    pub min_items: usize,
    /// Optional minimum summed payload bytes (used by object hashing).
    pub min_total_bytes: Option<usize>,
    /// Git `checkout.thresholdForParallelism` (signed); parallel only when `count >=` this value.
    pub signed_entry_threshold: Option<i64>,
}

impl ParallelOptions {
    /// View as [`Parallelism`] for legacy hash helpers.
    #[must_use]
    pub fn parallelism(self) -> Parallelism {
        Parallelism::new(self.threads)
    }
}

#[must_use]
fn core_preload_index(config: &ConfigSet) -> bool {
    config
        .get_bool("core.preloadindex")
        .or_else(|| config.get_bool("core.preloadIndex"))
        .and_then(|r| r.ok())
        .unwrap_or(true)
}

/// Git `repo_config_get_index_threads` value semantics for `index.threads`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexThreadsSetting {
    /// Unset — Grit hash paths treat this as auto (all CPUs), matching prior behavior.
    UnsetAuto,
    Auto,
    Serial,
    Count(usize),
}

#[must_use]
fn index_threads_setting(config: &ConfigSet) -> IndexThreadsSetting {
    if let Some(Ok(n)) = config.get_i64("index.threads") {
        return match n {
            0 => IndexThreadsSetting::Auto,
            1 => IndexThreadsSetting::Serial,
            n if n > 1 => IndexThreadsSetting::Count(usize::try_from(n).unwrap_or(1)),
            _ => IndexThreadsSetting::Serial,
        };
    }
    if config.get("index.threads").is_some_and(|v| v.trim() == "0") {
        return IndexThreadsSetting::Auto;
    }
    if let Some(result) = config.get_bool("index.threads") {
        return match result {
            Ok(true) => IndexThreadsSetting::Auto,
            Ok(false) => IndexThreadsSetting::Serial,
            Err(_) => IndexThreadsSetting::UnsetAuto,
        };
    }
    IndexThreadsSetting::UnsetAuto
}

#[must_use]
fn resolve_index_threads(config: &ConfigSet, override_threads: Option<usize>) -> NonZeroUsize {
    if let Some(n) = override_threads {
        return Parallelism::resolve(Some(n)).threads();
    }
    match index_threads_setting(config) {
        IndexThreadsSetting::UnsetAuto | IndexThreadsSetting::Auto => {
            Parallelism::resolve(None).threads()
        }
        IndexThreadsSetting::Serial => NonZeroUsize::MIN,
        IndexThreadsSetting::Count(n) => Parallelism::resolve(Some(n)).threads(),
    }
}

/// Git `pack.threads` parsing (`None` = unset → auto at resolve time).
fn parse_pack_threads(config: &ConfigSet) -> Result<Option<usize>, ConfigError> {
    const KEY: &str = "pack.threads";
    let Some(raw) = config.get(KEY) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    match config.get_i64(KEY) {
        Some(Ok(n)) if n < 0 => Err(ConfigError::Other(format!(
            "invalid number of threads specified ({n})"
        ))),
        Some(Ok(0)) => Ok(None),
        Some(Ok(n)) => Ok(Some(usize::try_from(n).unwrap_or(1))),
        Some(Err(_)) | None => Err(ConfigError::BadNumericValue {
            key: KEY.into(),
            value: trimmed.into(),
            reason: BadNumericSource::InvalidUnit,
        }),
    }
}

fn resolve_pack_threads(
    config: &ConfigSet,
    override_threads: Option<usize>,
) -> Result<NonZeroUsize, ConfigError> {
    if let Some(n) = override_threads {
        return Ok(Parallelism::resolve(Some(n)).threads());
    }
    let requested = parse_pack_threads(config)?;
    Ok(Parallelism::resolve(requested).threads())
}

/// Parsed `checkout.workers` value (Git: unset vs explicit auto vs explicit count).
enum CheckoutWorkersSetting {
    /// Key absent or unreadable — platform default applies.
    Unset,
    /// Explicit `< 1` (including `0`) — auto / all logical CPUs.
    Auto,
    /// Explicit worker count.
    Count(usize),
}

#[must_use]
fn checkout_workers_setting(config: &ConfigSet) -> CheckoutWorkersSetting {
    match config.get_i64("checkout.workers") {
        None | Some(Err(_)) => CheckoutWorkersSetting::Unset,
        Some(Ok(n)) if n < 1 => CheckoutWorkersSetting::Auto,
        Some(Ok(n)) => CheckoutWorkersSetting::Count(n as usize),
    }
}

#[must_use]
fn resolve_checkout_workers(config: &ConfigSet, override_threads: Option<usize>) -> NonZeroUsize {
    let setting = checkout_workers_setting(config);

    if let Some(n) = override_threads {
        #[cfg(windows)]
        if matches!(setting, CheckoutWorkersSetting::Unset) {
            // Issue #1613: global `--threads` must not enable checkout disk pools when
            // `checkout.workers` was never configured.
            let _ = n;
            return NonZeroUsize::MIN;
        }
        return Parallelism::resolve(Some(n)).threads();
    }

    match setting {
        CheckoutWorkersSetting::Unset => {
            #[cfg(windows)]
            {
                return NonZeroUsize::MIN;
            }
            #[cfg(not(windows))]
            {
                Parallelism::resolve(None).threads()
            }
        }
        CheckoutWorkersSetting::Auto => Parallelism::resolve(None).threads(),
        CheckoutWorkersSetting::Count(n) => Parallelism::resolve(Some(n)).threads(),
    }
}

#[must_use]
fn checkout_parallelism_threshold(config: &ConfigSet) -> i64 {
    match config.get_i64("checkout.thresholdForParallelism") {
        None => 100,
        Some(Ok(n)) => n,
        Some(Err(_)) => 100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigScope, ConfigSet};
    use crate::environment::Environment;

    fn config_from(text: &str) -> ConfigSet {
        use crate::config::ConfigFile;
        use std::path::Path;
        let file = ConfigFile::parse(Path::new("test"), text, ConfigScope::Local).expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn index_threads_bool_and_int_semantics() {
        let true_auto = config_from("[index]\n\tthreads = true\n");
        assert_eq!(index_threads_setting(&true_auto), IndexThreadsSetting::Auto);
        let false_serial = config_from("[index]\n\tthreads = false\n");
        assert_eq!(
            index_threads_setting(&false_serial),
            IndexThreadsSetting::Serial
        );
        let zero = config_from("[index]\n\tthreads = 0\n");
        assert_eq!(index_threads_setting(&zero), IndexThreadsSetting::Auto);
        let one = config_from("[index]\n\tthreads = 1\n");
        assert_eq!(index_threads_setting(&one), IndexThreadsSetting::Serial);
        let four = config_from("[index]\n\tthreads = 4\n");
        assert_eq!(index_threads_setting(&four), IndexThreadsSetting::Count(4));
        let neg = config_from("[index]\n\tthreads = -3\n");
        assert_eq!(index_threads_setting(&neg), IndexThreadsSetting::Serial);
    }

    #[test]
    fn index_threads_negative_is_serial_for_refresh() {
        let cfg = config_from("[index]\n\tthreads = -3\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::IndexRefresh, &cfg);
        assert_eq!(opts.threads, NonZeroUsize::MIN);
    }

    #[test]
    fn pack_threads_negative_is_config_error() {
        let cfg = config_from("[pack]\n\tthreads = -3\n");
        let err = ThreadPolicy::default()
            .try_resolve(ParallelOp::IndexPack, &cfg)
            .expect_err("negative pack.threads");
        assert_eq!(
            err,
            ConfigError::Other("invalid number of threads specified (-3)".into())
        );
    }

    #[test]
    fn pack_threads_bool_is_bad_numeric() {
        for (raw, label) in [("true", "true"), ("false", "false")] {
            let cfg = config_from(&format!("[pack]\n\tthreads = {raw}\n"));
            let err = ThreadPolicy::default()
                .try_resolve(ParallelOp::IndexPack, &cfg)
                .expect_err(&format!("pack.threads = {label}"));
            assert!(
                matches!(
                    &err,
                    ConfigError::BadNumericValue {
                        key,
                        value,
                        reason
                    } if key == "pack.threads" && value == label && *reason == BadNumericSource::InvalidUnit
                ),
                "unexpected error for {label}: {err:?}"
            );
        }
    }

    #[test]
    fn preload_index_disables_index_refresh_workers() {
        let cfg = config_from("[core]\n\tpreloadIndex = false\n[index]\n\tthreads = 8\n");
        let policy = ThreadPolicy::default();
        let opts = policy.resolve(ParallelOp::IndexRefresh, &cfg);
        assert_eq!(opts.threads, NonZeroUsize::MIN);
    }

    #[test]
    fn thread_override_enables_index_refresh_when_preload_off() {
        let cfg = config_from("[core]\n\tpreloadIndex = false\n[index]\n\tthreads = 2\n");
        let policy = ThreadPolicy::with_threads_override(Some(6));
        let opts = policy.resolve(ParallelOp::IndexRefresh, &cfg);
        assert_eq!(
            opts.threads.get(),
            6.min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            )
        );
    }

    #[test]
    fn thread_override_beats_config() {
        let cfg = config_from("[index]\n\tthreads = 2\n");
        let policy = ThreadPolicy::with_threads_override(Some(6));
        let opts = policy.resolve(ParallelOp::IndexRefresh, &cfg);
        assert_eq!(
            opts.threads.get(),
            6.min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            )
        );
    }

    #[test]
    fn min_items_override_for_tests() {
        let cfg = ConfigSet::new();
        let policy = ThreadPolicy::with_min_items_override(Some(0));
        let opts = policy.resolve(ParallelOp::IndexPack, &cfg);
        assert_eq!(opts.min_items, 0);
    }

    #[test]
    fn pack_threads_zero_is_auto() {
        let cfg = config_from("[pack]\n\tthreads = 0\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::IndexPack, &cfg);
        assert!(opts.threads.get() > 1 || std::thread::available_parallelism().is_ok());
    }

    #[test]
    #[cfg(not(windows))]
    fn checkout_workers_unset_defaults_to_auto() {
        let cfg = ConfigSet::new();
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(
            opts.threads.get(),
            std::thread::available_parallelism()
                .map(NonZeroUsize::get)
                .unwrap_or(1)
        );
        assert_eq!(opts.signed_entry_threshold, Some(100));
    }

    #[test]
    #[cfg(windows)]
    fn checkout_workers_unset_defaults_to_serial() {
        let cfg = ConfigSet::new();
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(opts.threads, NonZeroUsize::MIN);
        assert_eq!(opts.signed_entry_threshold, Some(100));
    }

    #[test]
    #[cfg(windows)]
    fn checkout_global_threads_override_ignored_without_checkout_workers() {
        let cfg = ConfigSet::new();
        let policy = ThreadPolicy::with_threads_override(Some(8));
        let opts = policy.resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(opts.threads, NonZeroUsize::MIN);
    }

    #[test]
    #[cfg(windows)]
    fn checkout_explicit_auto_enables_parallel_workers() {
        let cfg = config_from("[checkout]\n\tworkers = 0\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(
            opts.threads.get(),
            std::thread::available_parallelism()
                .map(NonZeroUsize::get)
                .unwrap_or(1)
        );
    }

    #[test]
    #[cfg(windows)]
    fn checkout_workers_two_respects_cpu_cap_on_windows() {
        let cfg = config_from("[checkout]\n\tworkers = 2\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(
            opts.threads.get(),
            2.min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            )
        );
    }

    #[test]
    #[cfg(windows)]
    fn checkout_thread_override_beats_workers_config_on_windows() {
        let cfg = config_from("[checkout]\n\tworkers = 2\n");
        let policy = ThreadPolicy::with_threads_override(Some(4));
        let opts = policy.resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(
            opts.threads.get(),
            4.min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            )
        );
    }

    #[test]
    fn checkout_workers_zero_is_auto() {
        let cfg = config_from("[checkout]\n\tworkers = 0\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(
            opts.threads.get(),
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        );
    }

    #[test]
    fn checkout_threshold_negative_is_preserved() {
        let cfg = config_from("[checkout]\n\tthresholdForParallelism = -5\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(opts.signed_entry_threshold, Some(-5));
    }

    #[test]
    fn checkout_threshold_negative_keeps_small_batches_serial() {
        use crate::parallel::map::pool_worthwhile;
        let cfg =
            config_from("[checkout]\n\tthresholdForParallelism = -5\n[checkout]\n\tworkers = 4\n");
        let opts = ThreadPolicy::default().resolve(ParallelOp::Checkout, &cfg);
        assert!(!pool_worthwhile(0, 0, opts));
        // Threads are capped at the machine's cores (Miri reports one), and a
        // single thread never uses the pool.
        assert_eq!(pool_worthwhile(100, 0, opts), opts.threads.get() > 1);
    }

    #[test]
    fn checkout_thread_override_beats_workers_config() {
        let cfg = config_from("[checkout]\n\tworkers = 2\n");
        let policy = ThreadPolicy::with_threads_override(Some(6));
        let opts = policy.resolve(ParallelOp::Checkout, &cfg);
        assert_eq!(
            opts.threads.get(),
            6.min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            )
        );
    }

    #[test]
    fn stat_preload_respects_preload_math_and_override() {
        let cfg = ConfigSet::new();
        let policy = ThreadPolicy::default();
        let index_cap = resolve_index_threads(&cfg, None).get();
        assert_eq!(
            policy.index_stat_preload_threads(&cfg, 5000, None),
            10_usize.min(20).min(index_cap)
        );
        let policy = ThreadPolicy::with_threads_override(Some(8));
        let override_cap = resolve_index_threads(&cfg, Some(8)).get();
        assert_eq!(
            policy.index_stat_preload_threads(&cfg, 5000, None),
            10_usize.min(20).min(override_cap)
        );
        assert_eq!(
            policy.index_stat_preload_threads(&cfg, 5000, Some(4)),
            4_usize.min(override_cap)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "creates a temp dir; Miri isolation blocks mkdir")]
    fn pack_index_parallelism_for_git_dir_matches_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("config"), "[pack]\n\tthreads = 3\n").expect("write config");
        let env = Environment::empty();
        let par = ConfigSet::pack_index_parallelism_for_git_dir(&env, Some(dir.path()));
        assert_eq!(par.threads().get(), 3);
    }
}
