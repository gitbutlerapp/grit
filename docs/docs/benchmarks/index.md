# Benchmarks

> Grit vs Git on realistic workloads, from the committed grit-bench baseline.

Performance work is measured on the factory VM and in CI against the **system** `git` binary. Micro-benchmarks live in `grit-lib` (Criterion) and command-level scenarios in `grit-bench` (`grit-utils`).

Command-level numbers below come from the committed **grit-bench** JSON baselines listed in `content/docs/site.toml`. Each scenario runs **hyperfine** against the system **`git`** binary and a release **`grit`** build on the same machine; times are wall-clock milliseconds (mean and spread from timed runs).

## grit-lib: parallel object hashing

Criterion group `hash/batch_parallel` (`cargo bench -p grit-lib --bench objects hash/batch_parallel`) hashes **20 000** Git blob objects of **16 KiB** each using [`hash_objects_parallel`](https://docs.rs/grit-lib/latest/grit_lib/hash/fn.hash_objects_parallel.html) (SHA-1, release build).

| Threads | Time (median) | Throughput |
| ------- | ------------- | ---------- |
| 1       | 173 ms        | 1.76 GiB/s |
| 8       | 44.5 ms       | 6.86 GiB/s |

**Speedup (8 vs 1 thread): ~3.9×** on the factory VM (2026-10-07).

Parallel hashing falls back to a serial loop when there are fewer than [`PAR_HASH_MIN_ITEMS`](https://docs.rs/grit-lib/latest/grit_lib/hash/constant.PAR_HASH_MIN_ITEMS.html) (32) objects or less than [`PAR_HASH_MIN_TOTAL_BYTES`](https://docs.rs/grit-lib/latest/grit_lib/hash/constant.PAR_HASH_MIN_TOTAL_BYTES.html) (256 KiB) of payload, so small batches avoid thread overhead.

## grit-lib: ODB backend (pre-refactor baseline)

Criterion group **`odb_backend`** (`cargo bench -p grit-lib --bench odb_backend -- --save-baseline odb-before`) measures [`Odb::read`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html), [`read_info`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.read_info), [`exists`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.exists) (hit and miss), and [`write`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.write) on four deterministic layouts built in `grit-lib/benches/fixture.rs`:

| Fixture | Shape |
| --- | --- |
| `loose_10k` | 10 000 loose blobs, no packs |
| `pack_100k` | Single repacked pack (~100 000 objects) |
| `midx_8` | Eight pack layers with multi-pack-index (`git prune-packed` so hits are pack-only, not loose) |
| `alternate_only` | Primary store empty; objects only in an alternate |

An additional benchmark, **`odb_backend/exists_miss_loop_10k`**, calls `exists` on 10 000 distinct missing OIDs against the 100k pack fixture (models add/status miss scans).

Command-level ODB scenarios on the repacked **100k-file / 1000-commit** synthetic repo live in **`grit-utils/baselines/odb-backend-before.json`** (`grit-bench odb-backend`): library drivers (`grit-bench drive …`) vs **`git cat-file --batch`**, **`git cat-file --batch-check`**, and **`git rev-list --objects --all`** on the same sorted OID stdin list where applicable.

```bash
cargo build --release -p grit-utils
./target/release/grit-bench odb-backend --format json --output grit-utils/baselines/odb-backend-before.json
cargo bench -p grit-lib --bench odb_backend -- --save-baseline odb-before
make docs
```

Run `grit-bench odb-backend` once before the Criterion command (or leave a populated `GRIT_BENCH_ODB_CACHE`) so the `pack_100k` fixture reuses the repacked 100k repo instead of rebuilding 100k commits inline.

**Recorded on the factory VM (2026-10-09, `odb-backend-before.json`, repacked 100k synthetic repo):**

| Scenario | Git median | Grit median | Grit / Git |
| --- | ---: | ---: | ---: |
| `cat-file-batch-hot-path-100k` | 151 ms | 1 117 ms | **7.38×** |
| `cat-file-batch-check-hot-path-100k` | 52 ms | 1 261 ms | **24.2×** |
| `rev-list-objects-odb-backend-hot-path-100k` | 70 ms | 6 916 ms | **99.1×** |

The rev-list gap is dominated by history/object enumeration in `grit-lib`, not bulk pack I/O; cat-file scenarios exercise ODB read and `read_info` paths directly.

## grit-lib: parallel index-pack (in-memory)

Hyperfine on a **~90 000-object** depth-50 pack (`git fast-import` + `git repack -adf --depth=50`, factory VM 2026-10-07). Grit uses [`pack_index_records_with_threads`](https://docs.rs/grit-lib/latest/grit_lib/unpack_objects/fn.pack_index_records_with_threads.html) via `cargo run --release -p grit-lib --example index_pack_bench` (see `GRIT_INDEX_PACK_BENCH_PACK` / `GRIT_INDEX_PACK_THREADS`).

| Command | Mean (8 runs) |
| ------- | ------------- |
| `git index-pack --threads=8` | 640 ms |
| grit index-pack, 1 thread | 1.43 s |
| grit index-pack, 8 threads | 781 ms |

**Speedup (8 vs 1 thread): ~1.83×** on this fixture. Multi-threaded builds discover object boundaries with a discard decompress pass, then parallel workers inflate and hash whole objects; delta chains inflate, apply, and hash in parallel rounds. Grit does not yet beat `git index-pack --threads=8` on this pack (Git also writes the on-disk `.idx`).

Integration coverage uses a **≥ 50 000** object fixture built with `git fast-import` and shared across tests via `OnceLock` (`grit-lib/tests/index_pack_parallel.rs`); the default release suite completes in about **41 s** on the factory VM.

## Hashing

Raw digest throughput uses [`grit_lib::hash::ObjectHasher`](https://docs.rs/grit-lib/latest/grit_lib/hash/enum.ObjectHasher.html) (release build, Criterion). OpenSSL numbers are from `openssl speed -evp sha1 -bytes 16384` and `openssl speed -evp sha256` (16 KiB blocks where shown). Git blob hashing uses `git hash-object` on a 256 MiB file vs the `gritx-hash-file` example (same canonical blob object id, no object-database write).

**Environment (2026-10-07):** Intel Xeon (SHA-NI present), Linux, `git` 2.43.0, OpenSSL 3.0.13, Rust stable. `hash-info --json` reports `{"sha1":"x86_sha_ni","sha256":"x86_sha_ni"}`.

| Workload | grit-lib | Reference | grit / reference |
| -------- | -------- | --------- | ---------------- |
| SHA-1, 1 MiB buffer | 1.98 GiB/s | OpenSSL SHA-1 @ 16 KiB: 1.98 GiB/s | **100%** |
| SHA-256, 1 MiB buffer | 1.80 GiB/s | OpenSSL SHA-256 @ 16 KiB: 1.78 GiB/s | **101%** |
| SHA-1 blob, 256 MiB file | 0.31 s | `git hash-object`: 0.48 s | **1.55× faster** |

SHA-1 at 1 MiB and above meets the project bar (≥ 95% of OpenSSL on the same CPU). SHA-256 meets ≥ 90% of OpenSSL. End-to-end blob id for a large file beats stock Git 2.43, which still uses sha1dc for object hashing.

### Commands

```bash
# Backends selected on this CPU
cargo run -p grit-examples --bin hash-info -- --json

# Criterion throughput (64 B … 64 MiB)
cargo bench -p grit-lib --bench hash

# OpenSSL + hyperfine vs git (creates a 256 MiB temp file)
./scripts/bench-hash.sh
```

## grit-bench vs Git

### Methodology

- **Fixtures** use synthetic repos at large file counts and deep history, built deterministically in a scratch directory.
- **Drivers** are CLI invocations of `grit` and `git` with equivalent semantics (not argv-for-argv compatibility).
- **Ratios** are **Grit median ÷ Git median** (stored in each scenario’s `ratio` field; hyperfine also records mean and spread). Values below `1.00×` mean Grit is faster; values above `2.00×` are highlighted as regressions worth investigating.
- **Spread** shows Grit’s standard deviation across timed runs (hyperfine `--min-runs` / warmup settings from the baseline capture).

### Reproduce locally

Build release binaries, install [hyperfine](https://github.com/sharkdp/hyperfine), then run matching grit-bench scenarios from the repo root (see `grit-utils/README.md` for the current subcommands):

```bash
cargo build --release -p grit-cli -p grit-utils

# Example: hot-path scenarios at L/H tiers (switch, pick, merge)
./target/release/grit-bench hot-paths --sizes 10000,100000 --format json --output /tmp/grit-bench-hot-paths.json
```

When roadmap item 2 adds more baseline JSON files on `main`, list their paths under `[benchmarks].baseline` in `content/docs/site.toml` and regenerate the site.

Compare a fresh run against the committed baseline (exit non-zero if any scenario ratio drifts beyond tolerance):

```bash
./target/release/grit-bench compare grit-utils/baselines/hot-paths-before.json /tmp/grit-bench-hot-paths.json --tolerance 0.10
```

### Hot-path acceptance (maintenance plan step 386)

Recorded on the factory VM after stacking pick/merge/stash perf work on `origin/main`. **Before** numbers are in `grit-utils/baselines/hot-paths-before.json` (2026-10-07); **after** in `grit-utils/baselines/hot-paths-after.json` (2026-10-08). Config isolation (`GIT_CONFIG_NOSYSTEM`, empty global config) applies to hot-path, suite `commit`, and related scenarios. FSMN variants: `grit-utils/baselines/hot-paths-after-fsmn.json`.

**Machine (2026-10-08 acceptance):** Intel Xeon (factory VM), **4** physical / **4** logical CPUs, **16 GiB** RAM, Linux **6.12.94+**, scratch filesystem **ext4**, `rustc` **1.99.0**, `git` **2.43.0**, release `grit` **0.5.1**, hyperfine **2.x** (see `machine` in each baseline JSON).

| Scenario | L before × | L after × | H before × | H after × | Notes |
| --- | ---: | ---: | ---: | ---: | --- |
| `add` | — | **2.52** | — | **4.51** | `suite-after-LH.json` (`add-{N}`) |
| `commit` | — | **0.84** | — | **1.16** | `commit-after-LH.json` — `grit commit` vs `git add -A && git commit` |
| `switch` | 3.29 | 3.44 | 4.89 | 5.63 | Still &gt;2× at L; H/L ≈ **1.64×** (&gt;1.25× bar) |
| `switch-wide` | 3.70 | 2.29 | 26.5 | 2.98 | L still &gt;2×; H/L ≈ **1.30×** |
| `pick` | 1.41 | 1.14 | 5.50 | 4.04 | L within 2×; H/L ≈ **3.54×** |
| `merge` | 2.61 | 2.18 | 3.97 | 3.83 | L still above 2×; H/L ≈ **1.76×** |
| `pick-series` | 12.9 | 11.2 | 21.7 | 17.7 | L &gt;2×; H/L ≈ **1.58×**; 20× sequential `grit pick` vs one `git cherry-pick` range |

**Suite at L/H** (`grit-utils/baselines/suite-after-LH.json`): status scenarios remain far above 2× (dirty/clean at 10k and 100k files).

**H/L acceptance (1.25× bar):** after-run medians fail the scaling check for **`switch`**, **`switch-wide`**, **`pick`**, **`merge`**, and **`pick-series`** (not only switch/pick/merge). **`commit`** and **`add`** at H are within 2× of Git but **`add`** at L/H both exceed 2× at 10k and 100k files.

Two back-to-back hot-path after runs (`hot-paths-after-run1.json` vs `run2.json`) agree within **10% relative** on every scenario except `merge-100000` and `pick-100000` (~**13–14%** relative spread). The default `grit-bench compare` tolerance is an **absolute** 0.10 on the ratio, which is tighter than ±10% relative for large ratios (e.g. pick-series).

### Follow-up tracking (step 386)

No GitHub issues were filed from this factory run (`file_bug_report` is dogfooding-only). Track these against roadmap item 4:

- **L &gt;2×:** `switch`, `switch-wide`, `merge`, `pick-series`, `add` (10k and 100k), status at L/H.
- **H/L &gt;1.25×:** `switch`, `switch-wide`, `pick`, `merge`, `pick-series`.
- **Repeatability:** second hot-path after-run drift on `merge-100000` and `pick-100000` (~13–14% relative).
- **CLI gaps:** `grep`, `rebase`, `reset`, `stash push` (deferrals documented above).

**Criterion** (`cargo bench -p grit-lib --bench hot_paths`, release): see factory run log — `apply_stash_L` ~52 ms median; `pick_2000_paths_L` ~621 ms (library merge/checkout path, not CLI).

### Roadmap commands without a `grit` engine yet

| Git command | Grit CLI | How the plan covers it |
| --- | --- | --- |
| `grep` | none | Deferred — no porcelain driver in scope |
| `rebase` | none | `pick-series` grit-bench scenario (20× `grit pick`) as upper-bound proxy |
| `reset` | none | Deferred |
| `stash push` | none | Criterion `apply_stash` / library `apply_stash` bench; no `grit stash push` timing |

### Object reads

The **`grit-bench odb`** suite (library drivers in `grit-bench drive …` vs system **`git`**) measures cat-file batch reads, `rev-list --objects --all`, and `log -p` on a cached bare **git.git** clone and a repacked **100k-file / 1000-commit** synthetic repo. JSON reports include **peak RSS** per scenario (each workload measured in a fresh `grit-bench measure-rss` helper process via `getrusage(RUSAGE_CHILDREN)` on that run only) alongside wall time.

Reproduce:

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench odb --format json --output grit-utils/baselines/odb-read.json
```

**Profiling notes (2026-10-08 factory VM, baseline `grit-utils/baselines/odb-read.json`):**

- **Cat-file batch** on git.git (pack order): grit ~15.8 s / ~543 MiB peak vs git ~10.5 s / ~477 MiB — mostly ODB pack reads; grit is ~1.5× slower on wall time.
- **`rev-list --objects --all`** on git.git: grit ~1 612 s vs git ~3.0 s — **not ODB-bound**; time is in `rev_list` history/object enumeration (tree walks and object listing), not bulk cat-file batch I/O.
- **`log -p -2000`** on git.git: grit ~8.8 s vs git ~1.5 s — **mostly revwalk + tree diff/patch formatting** (`rev_list` then per-commit `diff_trees` / unified diff); ODB reads happen per diff hunk but dominate less than walk/diff work at this commit count.

Follow-up optimization for the super-linear rev-list gap belongs in **revwalk / object listing**, not pack index or mmap ODB work.

### Results



| | |
| --- | --- |
| Git | git version 2.43.0 |
| Grit | grit 0.5.1 |
| CPU | Intel(R) Xeon(R) Processor |
| OS | linux (Linux 6.12.94+) |
| Recorded | 2026-10-08 06:51:40.138623969 |

## Summary by operation

| Operation | Scenarios | Median Grit / Git | Worst Grit / Git |
| --- | ---: | ---: | ---: |
| add | 2 | 3.49× | 4.50× |
| commit | 2 | 1.00× | 1.16× |
| merge | 2 | 3.00× | 3.80× |
| object_reads | 8 | 234.49× | 4569.02× |
| odb_backend | 3 | 22.88× | 98.20× |
| pick | 4 | 6.96× | 16.49× |
| status | 4 | 9.02× | 46.96× |
| switch | 4 | 3.25× | 5.36× |

### add

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `add-10000` | synthetic-10000 | 19.7 | 49.0 | 2.49× | ±2.49 ms |
| `add-100000` | synthetic-100000 | 198 | 891 | 4.50× | ±65.8 ms |

### commit

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `commit-10000` | synthetic-10000 | 176 | 148 | 0.84× | ±7.32 ms |
| `commit-100000` | synthetic-100000 | 1,929 | 2,243 | 1.16× | ±93.4 ms |

### merge

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `merge-10000` | synthetic-10000 | 116 | 256 | 2.21× | ±38.0 ms |
| `merge-100000` | synthetic-100000 | 246 | 934 | 3.80× | ±26.5 ms |

### object_reads

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `cat-file-batch-sorted-git.git` | git.git | 32,433 | 46,558 | 1.44× | ±253 ms |
| `cat-file-batch-unordered-git.git` | git.git | 10,655 | 15,384 | 1.44× | ±62.4 ms |
| `log-patch-2000-git.git` | git.git | 1,518 | 699,640 | 460.95× | ±15,497 ms |
| `rev-list-objects-git.git` | git.git | 3,036 | 1,590,307 | 523.82× | ±2,433 ms |
| `cat-file-batch-sorted-hot-path-100k` | hot-path-100k | 119 | 922 | 7.76× | ±2.48 ms |
| `cat-file-batch-unordered-hot-path-100k` | hot-path-100k | 117 | 937 | 8.03× | ±28.4 ms |
| `log-patch-2000-hot-path-100k` | hot-path-100k | 400 | 1,827,664 | 4569.02× | ±12,342 ms |
| `rev-list-objects-hot-path-100k` | hot-path-100k | 55.4 | 34,028 | 614.25× | ±43.8 ms |

### odb_backend

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `cat-file-batch-check-hot-path-100k` | hot-path-100k | 56.7 | 1,298 | 22.88× | ±196 ms |
| `cat-file-batch-hot-path-100k` | hot-path-100k | 162 | 1,117 | 6.91× | ±19.5 ms |
| `rev-list-objects-odb-backend-hot-path-100k` | hot-path-100k | 71.9 | 7,062 | 98.20× | ±446 ms |

### pick

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `pick-10000` | synthetic-10000 | 126 | 170 | 1.35× | ±53.6 ms |
| `pick-series-10000` | synthetic-10000 | 137 | 1,354 | 9.86× | ±53.6 ms |
| `pick-100000` | synthetic-100000 | 204 | 828 | 4.06× | ±25.0 ms |
| `pick-series-100000` | synthetic-100000 | 996 | 16,428 | 16.49× | ±315 ms |

### status

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `status-clean-10000` | synthetic-10000 | 8.25 | 37.8 | 4.58× | ±2.78 ms |
| `status-dirty-10000` | synthetic-10000 | 10.3 | 103 | 10.03× | ±5.66 ms |
| `status-clean-100000` | synthetic-100000 | 89.0 | 712 | 8.01× | ±13.3 ms |
| `status-dirty-100000` | synthetic-100000 | 113 | 5,328 | 46.96× | ±95.1 ms |

### switch

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `switch-10000` | synthetic-10000 | 25.4 | 90.3 | 3.55× | ±15.4 ms |
| `switch-wide-10000` | synthetic-10000 | 49.8 | 111 | 2.24× | ±17.3 ms |
| `switch-100000` | synthetic-100000 | 183 | 979 | 5.36× | ±88.7 ms |
| `switch-wide-100000` | synthetic-100000 | 418 | 1,235 | 2.96× | ±30.3 ms |


To refresh this page after updating a baseline JSON file, regenerate the static site:

```bash
make docs
```

The docs `--check` step fails if baseline numbers change without re-running `make docs`.

For other library surfaces under test, see the [grit-lib API on docs.rs](https://docs.rs/grit-lib).
