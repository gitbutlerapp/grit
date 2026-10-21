//! Integration coverage for [`grit_lib::parallel`] public helpers.

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::parallel::{
    index_pack_threads_for_ingest, par_map, ParallelOp, Parallelism, ThreadPolicy,
};
use grit_test_support::forced_parallel_policy;
use std::num::NonZeroUsize;
use std::path::Path;

fn config_from(text: &str) -> ConfigSet {
    let file = ConfigFile::parse(Path::new("test"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

#[test]
fn forced_parallel_policy_enables_tiny_batches() {
    let policy = forced_parallel_policy();
    let opts = policy.resolve(ParallelOp::IndexPack, &ConfigSet::new());
    assert_eq!(opts.min_items, 0);
    assert!(opts.threads.get() >= 1);
}

#[test]
fn par_map_public_api_preserves_order_with_forced_policy() {
    let policy = forced_parallel_policy();
    let opts = policy.resolve(ParallelOp::IndexRefresh, &ConfigSet::new());
    let items: Vec<usize> = (0..128).collect();
    let got = par_map(&items, opts, 0, |&x| x * 3);
    let expect: Vec<_> = items.iter().map(|x| x * 3).collect();
    assert_eq!(got, expect);
}

#[test]
fn index_threads_false_is_serial_for_refresh() {
    let cfg = config_from("[index]\n\tthreads = false\n");
    let opts = ThreadPolicy::default().resolve(ParallelOp::IndexRefresh, &cfg);
    assert_eq!(opts.threads, NonZeroUsize::MIN);
}

#[test]
fn index_pack_threads_respects_cli_policy_not_config_alone() {
    let cfg = config_from("[pack]\n\tthreads = 2\n");
    assert_eq!(
        index_pack_threads_for_ingest(ThreadPolicy::default(), &cfg, None).expect("ingest"),
        None
    );
    let policy = ThreadPolicy::with_threads_override(Some(5));
    assert_eq!(
        index_pack_threads_for_ingest(policy, &cfg, None).expect("ingest"),
        Some(
            5.min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            )
        )
    );
}

#[test]
fn fetch_ingest_sees_git_config_count_pack_threads() {
    use grit_lib::config::environment::Environment;
    use std::ffi::OsString;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("objects")).expect("objects");
    let env = Environment::from_vars(
        [
            (OsString::from("GIT_CONFIG_COUNT"), OsString::from("1")),
            (
                OsString::from("GIT_CONFIG_KEY_0"),
                OsString::from("pack.threads"),
            ),
            (OsString::from("GIT_CONFIG_VALUE_0"), OsString::from("4")),
        ],
        dir.path().to_path_buf(),
    );
    let cfg = ConfigSet::load(&env, Some(dir.path()), true).expect("load config");
    let opts = ThreadPolicy::default()
        .try_resolve(ParallelOp::IndexPack, &cfg)
        .expect("resolve pack threads from GIT_CONFIG_COUNT");
    assert_eq!(opts.threads.get(), 4);
}

#[test]
fn parallelism_resolve_matches_documented_auto() {
    let avail = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    assert_eq!(Parallelism::resolve(Some(0)).threads().get(), avail);
}
