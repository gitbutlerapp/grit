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

## grit-lib: ODB backend (pluggable object store)

Criterion group **`odb_backend`** compares post-refactor [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) against the saved baseline **`odb-before`** (`cargo bench -p grit-lib --bench odb_backend -- --baseline odb-before`). Built-in layouts route hot reads through [`FilesSource`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.FilesSource.html) on the primary store and [`CompositeStore`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.CompositeStore.html) for alternates so behaviour matches the pre-refactor oracle tests while keeping a pluggable [`ObjectStore`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/trait.ObjectStore.html) boundary.

**Pluggable object store overhead (factory VM, 2026-10-09):** Re-run Criterion with `--baseline odb-before` after ODB changes; command-level **`grit-bench odb-backend`** baselines are in **`odb-backend-before.json`** (pre-refactor capture, scenario ids suffixed `-pre-refactor`) and **`odb-backend-after.json`**. Post-refactor Grit medians for cat-file batch/check are much lower than the pre-refactor capture on the same fixture; rev-list wall time is similar while Grit/Git **ratios** can drift when system `git` speeds up between runs.

Criterion measures [`Odb::read`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html), [`read_info`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.read_info), [`exists`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.exists) (hit and miss), and [`write`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.write) on four deterministic layouts built in `grit-lib/benches/fixture.rs`:

| Fixture | Shape |
| --- | --- |
| `loose_10k` | 10 000 loose blobs, no packs |
| `pack_100k` | Single repacked pack (~100 000 objects) |
| `midx_8` | Eight pack layers with multi-pack-index (`git prune-packed` so hits are pack-only, not loose) |
| `alternate_only` | Primary store empty; objects only in an alternate |

An additional benchmark, **`odb_backend/exists_miss_loop_10k`**, calls `exists` on 10 000 distinct missing OIDs against the 100k pack fixture (models add/status miss scans).

Command-level ODB scenarios on the repacked **100k-file / 1000-commit** synthetic repo live in **`grit-utils/baselines/odb-backend-before.json`** and **`grit-utils/baselines/odb-backend-after.json`** (`grit-bench odb-backend`): library drivers (`grit-bench drive …`) vs **`git cat-file --batch`**, **`git cat-file --batch-check`**, and **`git rev-list --objects --all`** on the same sorted OID stdin list where applicable.

```bash
cargo build --release -p grit-utils
./target/release/grit-bench odb-backend --format json --output grit-utils/baselines/odb-backend-after.json
cargo bench -p grit-lib --bench odb_backend -- --baseline odb-before
make docs
```

Run `grit-bench odb-backend` once before the Criterion command (or leave a populated `GRIT_BENCH_ODB_CACHE`) so the `pack_100k` fixture reuses the repacked 100k repo instead of rebuilding 100k commits inline.

**Recorded on the factory VM (2026-10-09, repacked 100k synthetic repo).** Pre-refactor numbers are in **`odb-backend-before.json`**; post-refactor in **`odb-backend-after.json`** (also drives the generated ODB rows below).

| Scenario | Git median | Grit median (before → after) | Grit / Git (before → after) |
| --- | ---: | ---: | ---: |
| `cat-file-batch-hot-path-100k` | ~144 ms | 1 117 ms → ~356 ms | **7.4× → ~2.5×** |
| `cat-file-batch-check-hot-path-100k` | ~52 ms | 1 261 ms → ~239 ms | **24.2× → ~5.2×** |
| `rev-list-objects-odb-backend-hot-path-100k` | ~65 ms | 6 916 ms → ~6 189 ms | **99.1× → ~96×** |

Rev-list **ratios** can rise when system **`git`** speeds up between captures even if Grit wall time improves; compare absolute Grit medians when judging regressions.

The rev-list gap is dominated by history/object enumeration in `grit-lib`, not bulk pack I/O; cat-file scenarios exercise ODB read and `read_info` paths directly.

## grit-bench: reachability bitmaps (`git.git`)

Scenario group **`bitmaps`** measures rev-list counting and a full-ref stateless **`upload-pack`** clone against a bare clone of upstream **`git/git`** at a pinned tag (`v2.47.0` by default), prepared with **`git repack -adb`** and **`git commit-graph write --reachable`**. Library workloads run through hidden **`grit-bench drive`** commands (`rev-list-count`, `rev-list-count-objects`, `serve-clone`).

```bash
cargo build --release -p grit-utils -p grit-cli
./target/release/grit-bench bitmaps --format json --output grit-utils/baselines/bitmaps-before.json
```

Baseline **`bitmaps-before.json`** (factory VM, 2026-10-10): **`rev-list-count-git.git`** — git ~63 ms vs grit ~207 ms; **`rev-list-count-objects-git.git`** — git ~24 ms vs grit ~546 s (no pack bitmap reader yet); **`serve-clone-git.git`** — git ~785 ms, grit **failed** (`memory_cap` / OOM under the serve time and RSS caps). Re-run with **`--repo`** after the cached fixture exists under **`GRIT_BENCH_ODB_CACHE`**.

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

**Machine (2026-10-09 refresh):** same factory VM (Intel Xeon, **4** / **4** CPUs, **16 GiB** RAM, Linux **6.12.94+**, ext4 scratch), `rustc` **1.99.0**, `git` **2.43.0**, release `grit` **0.5.3** at **`01a254dc`**, hyperfine **2.0.0**. Committed baselines were regenerated with `./target/release/grit-bench {hot-paths,commit,all} --sizes 10000,100000` (default warmup / min-runs).

| Scenario | L before × | L 2026-10-08 × | L 2026-10-09 × | H before × | H 2026-10-08 × | H 2026-10-09 × | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `add` | — | **2.52** | **2.23** | — | **4.51** | **2.69** | `suite-after-LH.json` — improved at H |
| `commit` | — | **0.84** | **0.48** | — | **1.16** | **0.70** | `commit-after-LH.json` — faster vs Git at L/H |
| `switch` | 3.29 | 3.44 | **2.94** | 4.89 | 5.63 | **4.06** | H improved; L still &gt;2× |
| `switch-wide` | 3.70 | 2.29 | **3.42** | 26.5 | 2.98 | **22.54** | **H regression** — see refresh notes |
| `pick` | 1.41 | 1.14 | **1.48** | 5.50 | 4.04 | **5.22** | L slightly worse vs 2026-10-08 |
| `merge` | 2.61 | 2.18 | **2.86** | 3.97 | 3.83 | **3.74** | L ~32% worse ratio vs 2026-10-08 |
| `pick-series` | 12.9 | 11.2 | **11.94** | 21.7 | 17.7 | **18.56** | Still &gt;2×; 20× `grit pick` vs one `git cherry-pick` range |

**2026-10-09 refresh vs 2026-10-08 baselines:** `grit-bench compare` reports ratio drift beyond the default **0.10** absolute tolerance on most scenarios (many improved). Regressions **more than 20% worse** on the Grit/Git ratio: **`switch-wide-100000`** (2.98× → **22.54×**), **`switch-wide-10000`** (2.29× → 3.42×), **`merge-10000`**, **`pick-10000`**, **`pick-100000`**. **`status-dirty-100000`** remains ~**53×** (unchanged). Suite **status** and **add** ratios improved at L/H except dirty-100k.

| Scenario | Git ms | Grit ms | Old ratio | New ratio |
| --- | ---: | ---: | ---: | ---: |
| `status-dirty-10000` | 11.1 | 80.1 | 10.19× | 7.21× |
| `status-dirty-100000` | 113.0 | 5962.1 | 52.66× | 52.77× |
| `status-clean-10000` | 9.0 | 21.2 | 4.58× | 2.34× |
| `status-clean-100000` | 82.7 | 513.8 | 9.67× | 6.21× |
| `add-10000` | 21.4 | 47.8 | 2.52× | 2.23× |
| `add-100000` | 224.8 | 603.6 | 4.51× | 2.69× |
| `commit-10000` | 165.6 | 79.1 | 0.84× | 0.48× |
| `commit-100000` | 1665.7 | 1159.3 | 1.16× | 0.70× |
| `switch-10000` | 23.9 | 70.3 | 3.44× | 2.94× |
| `switch-100000` | 160.6 | 652.4 | 5.63× | 4.06× |
| `switch-wide-10000` | 45.3 | 155.3 | 2.29× | 3.42× |
| `switch-wide-100000` | 386.4 | 8711.8 | 2.98× | 22.54× |
| `pick-10000` | 115.9 | 171.5 | 1.14× | 1.48× |
| `pick-100000` | 194.0 | 1011.6 | 4.04× | 5.22× |
| `merge-10000` | 104.9 | 300.5 | 2.18× | 2.86× |
| `merge-100000` | 283.4 | 1060.6 | 3.83× | 3.74× |
| `pick-series-10000` | 129.4 | 1544.3 | 11.18× | 11.94× |
| `pick-series-100000` | 961.6 | 17843.4 | 17.65× | 18.56× |

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

**Measured outcomes:** the Object reads table below is generated from committed `grit-utils/baselines/odb-read.json` (hyperfine **≥5** runs per scenario, grit vs system git medians and peak RSS). Refresh with two invocations and `grit-bench compare run1.json run2.json --tolerance 0.10` before updating the baseline file and running `make docs`.

## grit-bench: network (clone, fetch, push, ls-remote)

The **`grit-bench network`** suite compares **`grit`** and system **`git`** on the same remote URLs. Cached fixtures live under **`GRIT_BENCH_NETWORK_CACHE`** (default `/tmp/grit-bench-network-cache`):

| Fixture | Shape |
| --- | --- |
| **deep-history** | **≥50 000** commits and **≥20 000** tracked files (built once via `git fast-import`) |
| **many-refs** | deep-history plus **≥10 000** `refs/heads/bench-ref-*` branches |
| **large-blobs** | a small tree with multi‑MiB blobs for pack-heavy transfer |

Scenarios include **clone** over **`file://`**, **git http-backend** (local CGI), and **grit-http-server** smart HTTP; **incremental fetch** and **no-op fetch** over **`file://`**; **push** over **`file://`**; **`git ls-remote`** vs **`grit remote refs`** on the many-refs fixture; and a **server-side** comparison that times system **`git clone`** against **git http-backend** vs **grit-http-server** on the same published bare repo. Hermetic git config matches other suites (`GIT_CONFIG_NOSYSTEM`, empty global config).

```bash
cargo build --release -p grit-cli -p grit-http-server -p grit-utils
./target/release/grit-bench network --format json --output grit-utils/baselines/network.json
./target/release/grit-bench compare grit-utils/baselines/network.json /tmp/network-rerun.json --tolerance 0.10
make docs
```

Integration smoke (tiny fixtures, eight scenarios; requires **hyperfine** on `PATH`): `cargo test -p grit-utils network_scenario_smoke_end_to_end`.

Acceptance bars for the network performance pass are **≤1.2×** git wall time on client-side scenarios (clone, fetch, push, ls-remote over `file://` and smart HTTP). Compare the generated table ratios to that bar.

**Server-side clone (`server-clone-http-compare-*`):** system **`git clone`** against **grit-http-server** is expected to stay above **1.2×** until the upload-pack path uses **reachability bitmaps** during negotiation (same dependency as Git’s `upload-pack` on large, pack-served repos). Client-side grit and git both pay similar pack transfer cost; the gap is dominated by grit’s in-process upload-pack enumeration without bitmaps, not HTTP keep-alive or side-band framing. Track under the MIDX/bitmap roadmap item rather than masking with compatibility shortcuts.

Regenerate release binaries with **`cargo build --release -p grit-lib`** before **`grit-bench network`** so the timed **`grit`** binary includes library changes.

### Results



| | |
| --- | --- |
| Git | git version 2.43.0 |
| Grit | grit 0.5.3 |
| CPU | Intel(R) Xeon(R) Processor |
| OS | linux (Linux 6.12.94+) |
| Recorded | 2026-10-09 07:45:00.292685959 |

## Summary by operation

| Operation | Scenarios | Median Grit / Git | Worst Grit / Git |
| --- | ---: | ---: | ---: |
| add | 2 | 2.56× | 2.59× |
| bitmaps | 3 | 3.42× | 22454.26× |
| commit | 2 | 0.58× | 0.68× |
| delta_encode | 2 | — | — |
| merge | 2 | 3.40× | 3.84× |
| network-clone | 3 | 2.49× | 2.53× |
| network-fetch | 2 | 8.72× | 16.73× |
| network-ls-remote | 1 | 1.70× | 1.70× |
| network-push | 1 | 21.43× | 21.43× |
| network-server | 1 | 79.40× | 79.40× |
| object_reads | 8 | 234.49× | 4569.02× |
| odb_backend | 6 | 14.90× | 98.20× |
| pick | 4 | 8.63× | 17.09× |
| status | 4 | 6.17× | 48.09× |
| switch | 4 | 3.61× | 22.50× |

### add

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `add-10000` | synthetic-10000 | 21.8 | 55.2 | 2.53× | ±32.9 ms |
| `add-100000` | synthetic-100000 | 234 | 608 | 2.59× | ±45.7 ms |

### bitmaps

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `rev-list-count-git.git` | git.git | 63.7 | 218 | 3.42× | ±35.7 ms |
| `rev-list-count-objects-git.git` | git.git | 24.3 | 546,442 | 22454.26× | ±5,375 ms |
| `serve-clone-git.git` | git.git | 819 | 0.00 | 0.00× | ±0.00 ms |

### merge

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `merge-10000` | synthetic-10000 | 105 | 310 | 2.95× | ±23.6 ms |
| `merge-100000` | synthetic-100000 | 289 | 1,110 | 3.84× | ±93.2 ms |

### network-clone

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `clone-file-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 602 | 1,521 | 2.53× | ±27.1 ms |
| `clone-git-http-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 671 | 1,669 | 2.49× | ±68.9 ms |
| `clone-grit-http-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 52,259 | 56,669 | 1.08× | ±2,437 ms |

### network-fetch

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `fetch-incr-file-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 44.8 | 749 | 16.73× | ±20.0 ms |
| `fetch-noop-file-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 8.93 | 6.41 | 0.72× | ±0.48 ms |

### network-ls-remote

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `ls-remote-file-prod` | /tmp/grit-bench-network-cache/many-refs-prod.git | 44.1 | 75.1 | 1.70× | ±7.50 ms |

### delta_encode

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `push-file-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 33.4 | 717 | 21.43× | ±9.04 ms |

### merge

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `server-clone-http-compare-prod` | /tmp/grit-bench-network-cache/deep-history-prod.git | 688 | 54,617 | 79.40× | ±2,671 ms |

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
| `cat-file-batch-check-hot-path-100k` | hot-path-100k | 46.8 | 246 | 5.26× | ±17.0 ms |
| `cat-file-batch-check-hot-path-100k-pre-refactor` | hot-path-100k | 56.7 | 1,298 | 22.88× | ±196 ms |
| `cat-file-batch-hot-path-100k` | hot-path-100k | 150 | 354 | 2.36× | ±9.24 ms |
| `cat-file-batch-hot-path-100k-pre-refactor` | hot-path-100k | 162 | 1,117 | 6.91× | ±19.5 ms |
| `rev-list-objects-odb-backend-hot-path-100k` | hot-path-100k | 66.2 | 6,242 | 94.23× | ±121 ms |
| `rev-list-objects-odb-backend-hot-path-100k-pre-refactor` | hot-path-100k | 71.9 | 7,062 | 98.20× | ±446 ms |

### pick

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `pick-10000` | synthetic-10000 | 117 | 171 | 1.46× | ±4.25 ms |
| `pick-series-10000` | synthetic-10000 | 129 | 1,551 | 11.98× | ±64.2 ms |
| `pick-100000` | synthetic-100000 | 194 | 1,021 | 5.27× | ±48.8 ms |
| `pick-series-100000` | synthetic-100000 | 1,045 | 17,861 | 17.09× | ±426 ms |

### status

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `status-clean-10000` | synthetic-10000 | 9.79 | 21.7 | 2.22× | ±2.13 ms |
| `status-dirty-10000` | synthetic-10000 | 11.5 | 81.1 | 7.04× | ±5.29 ms |
| `status-clean-100000` | synthetic-100000 | 95.6 | 506 | 5.29× | ±26.6 ms |
| `status-dirty-100000` | synthetic-100000 | 125 | 6,011 | 48.09× | ±274 ms |

### switch

| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |
| --- | --- | ---: | ---: | ---: | --- |
| `switch-10000` | synthetic-10000 | 24.6 | 70.5 | 2.86× | ±2.28 ms |
| `switch-wide-10000` | synthetic-10000 | 46.6 | 158 | 3.38× | ±6.96 ms |
| `switch-100000` | synthetic-100000 | 172 | 658 | 3.83× | ±20.5 ms |
| `switch-wide-100000` | synthetic-100000 | 389 | 8,763 | 22.50× | ±179 ms |


To refresh this page after updating a baseline JSON file, regenerate the static site:

```bash
make docs
```

The docs `--check` step fails if baseline numbers change without re-running `make docs`.

For other library surfaces under test, see the [grit-lib API on docs.rs](https://docs.rs/grit-lib).
