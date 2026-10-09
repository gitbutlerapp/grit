---
title: Benchmarks
summary: Grit vs Git on realistic workloads, from the committed grit-bench baseline.
---

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

**Profiling notes (2026-10-09 factory VM, after rebasing onto `origin/main` and step 480 tuning):**

- **Cat-file batch (unordered)** on git.git: grit is typically **≤1.2×** git after pack read context reuse and offset-order batch reads (see refreshed `grit-utils/baselines/odb-read.json`).
- **Cat-file on hot-path-100k** (repacked): acceptance target is ≤**1.2×** git wall time and ≤**1.5×** peak RSS; see the baseline JSON for current medians.
- **`rev-list --objects --all`**: acceptance target is ≤**1.2×** git on **both** git.git and hot-path-100k. System git uses pack reachability bitmaps when present; grit still walks trees with parent pruning and commit-graph parent lookup — see baseline ratios.
- **`log -p -2000`**: documented exception path only when revwalk/tree-diff dominates; see baseline `log-patch-2000-*` scenarios.

Refresh tables with `./target/release/grit-bench odb --format json --output grit-utils/baselines/odb-read.json` (hyperfine **≥5** runs; run twice and compare drift before committing).

### Results

<!-- grit:benchmark-tables -->

To refresh this page after updating a baseline JSON file, regenerate the static site:

```bash
make docs
```

The docs `--check` step fails if baseline numbers change without re-running `make docs`.

For other library surfaces under test, see the [grit-lib API on docs.rs](https://docs.rs/grit-lib).
