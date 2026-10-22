---
title: Parallelism
summary: Thread policy, Git config keys, and ordered parallel maps in grit-lib.
---

Grit centralizes worker counts and batch thresholds in [`grit_lib::parallel`](https://docs.rs/grit-lib/latest/grit_lib/parallel/index.html) so embedders and the CLI share one policy.

## Thread policy

[`ThreadPolicy`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html) holds optional overrides:

- **`threads_override`** — explicit worker count (`0` = auto / all logical CPUs, `1` = serial). The `grit` CLI sets this from global [`--threads`](../global-options.md).
- **`min_items_override`** — minimum input length before spawning a pool. Setting it to **`0`** forces the parallel path on tiny inputs (integration tests use this).
- **`min_total_bytes_override`** — optional byte threshold for hashing-style maps (`Some(0)` disables the byte gate).

Call [`ThreadPolicy::resolve`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html#method.resolve) with a [`ParallelOp`](https://docs.rs/grit-lib/latest/grit_lib/parallel/enum.ParallelOp.html) and a merged [`ConfigSet`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html) to obtain [`ParallelOptions`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ParallelOptions.html) (threads + batch thresholds).

| `ParallelOp` | Config keys | Default when unset (vs Git) |
| --- | --- | --- |
| `IndexRefresh` | `core.preloadIndex`, `index.threads` | `core.preloadIndex` defaults **on** (same as Git). `index.threads` unset → auto (all logical CPUs) for hash-style refresh; serial when preload is off or `index.threads` is `1` / `false`. Batch: **≥32** entries and **≥256 KiB** total payload before pooling. |
| `IndexLoad` | `core.preloadIndex`, `index.threads` | Same thread semantics as refresh. Batch: **≥4000** index entries (`2 × LAZY_THREAD_COST`, Git `name-hash.c`) before lazy name-hash threading. |
| `Checkout` | `checkout.workers`, `checkout.thresholdForParallelism` | **`checkout.workers` unset → 1 worker** (Git serial checkout until configured). Threshold **100** entries (Git default) — parallel file creation only when `\|entries\| ≥ threshold`. |
| `IndexPack` | `pack.threads` | Unset / `0` → auto; invalid values error via [`try_resolve`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html#method.try_resolve). No item-count floor (`min_items = 0`). |
| `PackWrite` | `pack.threads` | Same as `IndexPack`. Parallel delta encoding + zlib; on-wire order matches the single-threaded writer. |
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

Use [`Index::parse_with_algo_and_config`](https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html#method.parse_with_algo_and_config) with [`IndexParseContext`](https://docs.rs/grit-lib/latest/grit_lib/index/struct.IndexParseContext.html) (thread policy from [`Repository::thread_policy`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html#method.thread_policy)) and the repository config. Invalid EOIE data falls back to the serial entry parse. Writes go through [`Index::write_with_config_and_policy`](https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html#method.write_with_config_and_policy) so split indexes and sparse indexes keep working.

### `pack.threads` (index-pack / pack-objects)

Unset or `0` selects auto (all logical CPUs). Positive integers cap worker count. Git rejects booleans, non-numeric strings, and negative values with a config error; use [`ThreadPolicy::try_resolve`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html#method.try_resolve) for [`ParallelOp::IndexPack`](https://docs.rs/grit-lib/latest/grit_lib/parallel/enum.ParallelOp.html) to surface those errors. Fetch and [`clone`](https://docs.rs/grit-lib/latest/grit_lib/clone/fn.clone.html) pass [`CloneOptions::thread_policy`](https://docs.rs/grit-lib/latest/grit_lib/clone/struct.CloneOptions.html) (and the repository [`Environment`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html)) into received-pack indexing so global `--threads` overrides `pack.threads` and command-scope `GIT_CONFIG_*` / `-c pack.threads=N` still applies when no override is set.

Stat preload during status/diff uses Git's **~500 entries per thread** rule with a **20**-thread cap ([`ThreadPolicy::index_stat_preload_threads`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html#method.index_stat_preload_threads)), still honoring `core.preloadIndex`, `index.threads`, and the global thread override.

The untracked walk in status (no pathspecs) uses the `IndexRefresh` worker count and runs one task per directory entry. With at least 1000 index entries it first splits directories that only recurse (they hold tracked files and are not a gitlink or nested submodule) into their children, until each worker has about eight directories, so a work tree whose files sit under a single top-level directory still walks in parallel. Splitting changes neither the reported paths nor their order.

Optional `pack.windowMemory` caps sliding-window retention during delta search (Git `pack.windowMemory`, default unlimited).

## Global `--threads` (CLI)

[`grit --threads`](../global-options.md) sets [`RepositoryOptions::thread_policy`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html) for the whole process:

| Value | Effect |
| --- | --- |
| **`0`** | Auto — all logical CPUs for ops that honor unset config (`index.threads`, `pack.threads`, etc.). Does **not** turn on checkout parallelism by itself: unset **`checkout.workers`** still means one worker until you set **`checkout.workers`** or pass **`--threads` ≥ 2**. |
| **`1`** | Force serial pools (`threads = 1`) regardless of `index.threads` / `pack.threads` / `checkout.workers`. |
| **`N > 1`** | Cap workers at **`min(N, available CPUs)`** for all parallel operations (including checkout when the entry threshold is met). |

Embedders pass the same semantics via [`ThreadPolicy::with_threads_override`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html#method.with_threads_override).

## Determinism

Parallelism must not change Git-compatible **results**, only latency:

- [`try_par_map`](https://docs.rs/grit-lib/latest/grit_lib/parallel/fn.try_par_map.html) / [`par_map`](https://docs.rs/grit-lib/latest/grit_lib/parallel/fn.par_map.html) return outputs **in input order** even when workers finish out of order.
- Pack writing (`PackWrite`) emits the same bytes as the serial writer (header order follows the pack plan).
- Windowed delta search (`PackDeltaSearch`) partitions work by fixed segments so the chosen delta base for each object is independent of thread count.
- Index-pack builds the same object ids and offsets as the serial path; only scheduling differs.
- Checkout records the same index entries and file contents; parallel creation is ordered before post-processing.

There is **no global thread pool** and **no environment reads** inside `grit_lib::parallel`; pass policy explicitly (via [`RepositoryOptions::thread_policy`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html) or a local [`ThreadPolicy`](https://docs.rs/grit-lib/latest/grit_lib/parallel/struct.ThreadPolicy.html)).

## Ordered parallel maps

[`try_par_map_force`](https://docs.rs/grit-lib/latest/grit_lib/parallel/fn.try_par_map_force.html) skips batch thresholds (used for index-pack). Hash-specific helpers in [`grit_lib::objects::hash`](https://docs.rs/grit-lib/latest/grit_lib/hash/index.html) remain thin wrappers over these APIs.

## Tests

- Policy unit tests live in `grit-lib/src/parallel/policy.rs`.
- Public API coverage: `grit-lib/tests/parallel_api.rs`.
- Hash wrappers: `grit-lib/tests/hash_parallel_api.rs` (unchanged behavior).
- `grit_test_support::forced_parallel_policy` sets `threads = 4` and `min_items = 0` for deterministic parallel paths in integration tests (see the `grit-test-support` crate in the workspace).
- CI **`test-threaded`** runs `cargo test --release --workspace` with `index.threads`, `pack.threads`, `checkout.workers`, and `checkout.thresholdForParallelism=0` forced via `GIT_CONFIG_COUNT` (see [TESTING.md](../../TESTING.md)).
