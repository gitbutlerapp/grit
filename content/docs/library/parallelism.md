---
title: Parallelism
summary: Thread policy, Git config keys, and ordered parallel maps in grit-lib.
---

Grit centralizes worker counts and batch thresholds in [`grit_lib::parallel`](rustdoc:grit_lib::parallel) so embedders and the CLI share one policy.

## Thread policy

[`ThreadPolicy`](rustdoc:grit_lib::parallel::ThreadPolicy) holds optional overrides:

- **`threads_override`** — explicit worker count (`0` = auto / all logical CPUs, `1` = serial). The `grit` CLI sets this from global [`--threads`](../global-options.md).
- **`min_items_override`** — minimum input length before spawning a pool. Setting it to **`0`** forces the parallel path on tiny inputs (integration tests use this).
- **`min_total_bytes_override`** — optional byte threshold for hashing-style maps (`Some(0)` disables the byte gate).

Call [`ThreadPolicy::resolve`](rustdoc:grit_lib::parallel::ThreadPolicy::resolve) with a [`ParallelOp`](rustdoc:grit_lib::parallel::ParallelOp) and a merged [`ConfigSet`](rustdoc:grit_lib::config::ConfigSet) to obtain [`ParallelOptions`](rustdoc:grit_lib::parallel::ParallelOptions) (threads + batch thresholds).

| `ParallelOp` | Config keys | Default when unset (vs Git) |
| --- | --- | --- |
| `IndexRefresh` | `core.preloadIndex`, `index.threads` | `core.preloadIndex` defaults **on** (same as Git). `index.threads` unset → auto (all logical CPUs) for hash-style refresh; serial when preload is off or `index.threads` is `1` / `false`. Batch: **≥32** entries and **≥256 KiB** total payload before pooling. |
| `IndexLoad` | `core.preloadIndex`, `index.threads` | Same thread semantics as refresh. Batch: **≥4000** index entries (`2 × LAZY_THREAD_COST`, Git `name-hash.c`) before lazy name-hash threading. |
| `Checkout` | `checkout.workers`, `checkout.thresholdForParallelism` | **Windows:** `checkout.workers` **unset** → **1 worker**; explicit **`0` or any value `< 1`** → auto (all logical CPUs), same as Git; explicit **`N ≥ 1`** → `N`. Global `--threads` applies only when `checkout.workers` is explicitly set. **Other platforms:** unset or `< 1` → auto unless `--threads` caps the process. Threshold **100** entries (Git default) — parallel file creation only when `\|entries\| ≥ threshold`. [`grit switch`](../commands/switch.md) uses the repository threshold (not a forced zero). Switch prepare stat checks stay **serial on Windows** even when `index.threads` is auto (issue #1613). |
| `IndexPack` | `pack.threads` | Unset / `0` → auto; invalid values error via [`try_resolve`](rustdoc:grit_lib::parallel::ThreadPolicy::try_resolve). No item-count floor (`min_items = 0`). |
| `PackWrite` | `pack.threads` | Same as `IndexPack`. Parallel delta encoding + zlib in small batches on long-lived workers, streamed to the writer in plan order; on-wire bytes match the single-threaded writer. |
| `PackDeltaSearch` | `pack.threads` | Same as `IndexPack`. Segments are fixed from object count and `pack.window` so chosen delta bases do **not** depend on worker count. |
| `DiffRename` | `index.threads` with `core.preloadIndex` | Same threads as `IndexRefresh` when preload is on; same **32** / **256 KiB** batch gates. |

### Git value semantics for `index.threads`

Matches Git's `repo_config_get_index_threads`:

- unset — auto (all logical CPUs for hash-style refresh paths)
- `true` or `0` — auto
- `false` or `1` — serial (one worker)
- integer `n > 1` — up to `n` workers (clamped to available CPUs)
- other integers (for example `-3`) — preserved as serial (Git only parallelizes when `nr_threads > 1` after treating `0` as auto)

When `core.preloadIndex` is false, index refresh/load operations stay serial regardless of `index.threads`.

### EOIE / IEOT (parallel index read and write)

When `index.threads` is explicitly set to more than one worker, Git may also write **EOIE** (end-of-index-entries) and **IEOT** (index entry offset table) extensions so entry blocks can be parsed on worker threads while extension payloads load in parallel. Grit reads and writes these extensions when the same config keys are enabled:

- `index.recordEndOfIndexEntries` / `index.recordendofindexentries`
- `index.recordOffsetTable` / `index.recordoffsettable`

If those booleans are unset, they default to **on** when `index.threads` is explicitly configured and not serial (`1` / `false`).

Use [`Index::parse_with_algo_and_config`](rustdoc:grit_lib::index::Index::parse_with_algo_and_config) with [`IndexParseContext`](rustdoc:grit_lib::index::IndexParseContext) (thread policy from [`Repository::thread_policy`](rustdoc:grit_lib::repo::Repository::thread_policy)) and the repository config. Invalid EOIE data falls back to the serial entry parse. Writes go through [`Index::write_with_config_and_policy`](rustdoc:grit_lib::index::Index::write_with_config_and_policy) so split indexes and sparse indexes keep working.

### `pack.threads` (index-pack / pack-objects)

Unset or `0` selects auto (all logical CPUs). Positive integers cap worker count. Git rejects booleans, non-numeric strings, and negative values with a config error; use [`ThreadPolicy::try_resolve`](rustdoc:grit_lib::parallel::ThreadPolicy::try_resolve) for [`ParallelOp::IndexPack`](rustdoc:grit_lib::parallel::ParallelOp) to surface those errors. Fetch and [`clone`](rustdoc:grit_lib::remote::clone::clone) pass [`CloneOptions::thread_policy`](rustdoc:grit_lib::remote::clone::CloneOptions) (and the repository [`Environment`](rustdoc:grit_lib::config::environment::Environment)) into received-pack indexing so global `--threads` overrides `pack.threads` and command-scope `GIT_CONFIG_*` / `-c pack.threads=N` still applies when no override is set.

Stat preload during status/diff uses Git's **~500 entries per thread** rule with a **20**-thread cap ([`ThreadPolicy::index_stat_preload_threads`](rustdoc:grit_lib::parallel::ThreadPolicy::index_stat_preload_threads)), still honoring `core.preloadIndex`, `index.threads`, and the global thread override.

The untracked walk in status (no pathspecs) uses the `IndexRefresh` worker count and runs one task per directory entry. With at least 1000 index entries it first splits directories that only recurse (they hold tracked files and are not a gitlink or nested submodule) into their children, until each worker has about eight directories, so a work tree whose files sit under a single top-level directory still walks in parallel. Splitting changes neither the reported paths nor their order.

Optional `pack.windowMemory` caps sliding-window retention during delta search (Git `pack.windowMemory`, default unlimited).

## Global `--threads` (CLI)

[`grit --threads`](../global-options.md) sets [`RepositoryOptions::thread_policy`](rustdoc:grit_lib::config::environment::RepositoryOptions) for the whole process:

| Value | Effect |
| --- | --- |
| **`0`** | Auto — all logical CPUs for ops that honor unset config (`index.threads`, `pack.threads`, etc.). For **checkout on Windows**, unset **`checkout.workers`** stays serial; set **`checkout.workers = 0`** (or any value `< 1`) for auto. On other platforms, unset **`checkout.workers`** also uses auto unless you pass **`--threads` ≥ 2** to cap checkout workers. |
| **`1`** | Force serial pools (`threads = 1`) regardless of `index.threads` / `pack.threads` / `checkout.workers` (non-Windows checkout). |
| **`N > 1`** | Cap workers at **`min(N, available CPUs)`** for index/pack/diff operations. Checkout honors the cap when the entry threshold is met; on **Windows**, parallel checkout requires an explicit **`checkout.workers`** (including **`0`** for auto), not an unset key. |

Embedders pass the same semantics via [`ThreadPolicy::with_threads_override`](rustdoc:grit_lib::parallel::ThreadPolicy::with_threads_override).

## Determinism

Parallelism must not change Git-compatible **results**, only latency:

- [`try_par_map`](rustdoc:grit_lib::parallel::try_par_map) / [`par_map`](rustdoc:grit_lib::parallel::par_map) return outputs **in input order** even when workers finish out of order.
- Pack writing (`PackWrite`) emits the same bytes as the serial writer (header order follows the pack plan).
- Windowed delta search (`PackDeltaSearch`) partitions work by fixed segments so the chosen delta base for each object is independent of thread count.
- Index-pack builds the same object ids and offsets as the serial path; only scheduling differs. Each worker resolves the whole delta tree under one base at a time, depth-first, so memory follows the deepest chain (capped per worker) rather than the number of deltas, and adding threads adds only one chain's worth of memory each.
- Checkout records the same index entries and file contents; parallel creation is ordered before post-processing.

There is **no global thread pool** and **no environment reads** inside `grit_lib::parallel`; pass policy explicitly (via [`RepositoryOptions::thread_policy`](rustdoc:grit_lib::config::environment::RepositoryOptions) or a local [`ThreadPolicy`](rustdoc:grit_lib::parallel::ThreadPolicy)).

## Ordered parallel maps

[`try_par_map_force`](rustdoc:grit_lib::parallel::try_par_map_force) skips batch thresholds (used for index-pack). Hash-specific helpers in [`grit_lib::objects::hash`](rustdoc:grit_lib::objects::hash) remain thin wrappers over these APIs.

## Progress and cancellation

Long-running library work reports through [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink) and accepts a separate [`Cancel`](rustdoc:grit_lib::progress::Cancel) signal. Call [`poll_cancel`](rustdoc:grit_lib::progress::poll_cancel) at loop boundaries (or rely on built-in checks in fetch pack demux and status scans); either the cancel handle or [`ProgressSink::should_cancel`](rustdoc:grit_lib::progress::ProgressSink::should_cancel) stops the operation with [`Error::Cancelled`](rustdoc:grit_lib::error::Error::Cancelled). Pass [`NeverCancel`](rustdoc:grit_lib::progress::NeverCancel) when interruption is not needed.

Network and long-running entry points that accept [`Cancel`](rustdoc:grit_lib::progress::Cancel) today include [`fetch_remote`](rustdoc:grit_lib::remote::fetch::fetch_remote), [`fetch_local_with_progress`](rustdoc:grit_lib::remote::transfer::fetch_local_with_progress), [`Remote::fetch`](rustdoc:grit_lib::remote::Remote::fetch) / [`Remote::push`](rustdoc:grit_lib::remote::Remote::push), [`RepositoryRemotes::fetch_all`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::fetch_all), [`clone`](rustdoc:grit_lib::remote::clone::clone), [`pull`](rustdoc:grit_lib::porcelain::pull::pull), [`push_local`](rustdoc:grit_lib::remote::transfer::push_local), [`status`](rustdoc:grit_lib::porcelain::status::status), bundle fetch/unbundle ([`fetch_from_bundle_with_progress`](rustdoc:grit_lib::bundle::bundle_remote::fetch_from_bundle_with_progress), [`Bundle::unbundle_with_progress`](rustdoc:grit_lib::bundle::Bundle::unbundle_with_progress)), and received-pack indexing ([`install_pack_path_with_progress`](rustdoc:grit_lib::objects::index_pack::install_pack_path_with_progress) / [`ingest_received_pack_path_with_progress`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack_path_with_progress)). The coordinator thread calls [`poll_cancel`](rustdoc:grit_lib::progress::poll_cancel) at ingest phase boundaries (before/after parallel object hashing, between idx write, verify, and rename); index-pack worker threads observe only an [`AtomicBool`](https://doc.rust-lang.org/std/sync/atomic/struct.AtomicBool.html) snapshot, not [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink). **Not yet cancellable mid-flight:** thin-pack repair during fetch, a single ordered delta-resolution pool batch, and v2 idx sorting inside [`write_v2_pack_index_with_trailer`](rustdoc:grit_lib::objects::pack::write_v2_pack_index_with_trailer) — cancel is checked before/after those steps. Sideband demux during fetch uses the same cancel handle as the transport. Pass [`NeverCancel`](rustdoc:grit_lib::progress::NeverCancel) when interruption is not needed.

## Tests

- Policy unit tests live in `grit-lib/src/parallel/policy.rs`.
- Public API coverage: `grit-lib/tests/parallel_api.rs`.
- Hash wrappers: `grit-lib/tests/hash_parallel_api.rs` (unchanged behavior).
- `grit_test_support::forced_parallel_policy` sets `threads = 4` and `min_items = 0` for deterministic parallel paths in integration tests (see the `grit-test-support` crate in the workspace).
- CI **`test-threaded`** runs `cargo test --release --workspace` with `index.threads`, `pack.threads`, `checkout.workers`, and `checkout.thresholdForParallelism=0` forced via `GIT_CONFIG_COUNT` (see [TESTING.md](../../TESTING.md)).
