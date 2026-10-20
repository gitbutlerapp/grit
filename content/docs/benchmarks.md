---
title: Benchmarks
summary: Grit vs Git on realistic workloads, from the committed grit-bench baseline.
---

Performance work is measured on the factory VM and in CI against the **system** `git` binary. Micro-benchmarks live in `grit-lib` (Criterion) and command-level scenarios in `grit-bench` (`grit-utils`).

For a map of **grit-lib** modules, **grit** commands, common workflows, and every **`grit-bench daily`** scenario id, see the functionality matrix in [`grit-utils/MATRIX.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/MATRIX.md) (rendered on the performance overview page in a later roadmap step).

Command-level numbers below come from the committed **grit-bench** JSON baselines listed in `content/docs/site.toml`. Each scenario runs **hyperfine** against the system **`git`** binary and a release **`grit`** build on the same machine; times are wall-clock milliseconds (mean and spread from timed runs).

## grit-bench fixture cache

Scenario suites reuse prepared repositories under **`GRIT_BENCH_CACHE`** (default `$XDG_CACHE_HOME/grit-bench`). **`grit-bench fixtures prepare`** clones upstream **git.git** at tag **`v2.47.0`** (bare, verified `HEAD` oid) and generates synthetic tiers **S**, **M**, and **L** with the grit-lib generator. **`grit-bench fixtures list`** prints identity (name, tier, `HEAD`, object/ref/file counts). Mutating runs take a **`git clone --local`** working copy so the cache stays read-only. JSON reports may include a top-level **`fixtures`** array with the same identity fields.

```bash
./target/release/grit-bench fixtures prepare --format json
./target/release/grit-bench fixtures list --format markdown
```

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

## grit-lib: pack write (100k reuse vs fresh)

Criterion group **`pack`** (`cargo bench -p grit-lib --bench pack_write`) rebuilds the full closure of the repacked **`pack_100k`** fixture (~100 000 objects) via [`build_pack_with_parallel_threads`](https://docs.rs/grit-lib/latest/grit_lib/objects/pack_objects/fn.build_pack_with_parallel_threads.html):

| Benchmark | What it measures |
| --- | --- |
| **`write_100k_reuse`** | Delta planning with on-disk delta and whole-object reuse (`PackBuildOptions::for_local_copy`, bitmaps off) |
| **`write_100k_fresh`** | Same window/depth but **`reuse_objects`** and **`reuse_deltas`** disabled (recompress and re-delta everything) |

Throughput is reported in objects per second using the **emitted** pack header count from a probe rebuild of the full closure from the fixture **history tip** (cached repo `HEAD` or the generated repo tip), not the midpoint `hit` OID used for ODB read micro-benchmarks. Each iteration asserts that count matches the probe. Populate **`GRIT_BENCH_ODB_CACHE`** (or run **`grit-bench odb-backend`** once) so the fixture reuses the cached repacked repo instead of rebuilding 100k commits inline.

```bash
cargo bench -p grit-lib --bench pack_write -- write_100k
```

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

## grit-lib: parallel packed ODB reads

Criterion group **`odb_read_parallel/packed_blobs`** (`cargo bench -p grit-lib --bench odb_read_parallel`) measures throughput when worker threads share one [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) and hold [`Odb::with_pack_read_context`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html#method.with_pack_read_context) for the worker lifetime. Each iteration reads a partition of **50 000** whole-object pack blobs (**3072-byte** payload; loose copies are removed after packing so hits are pack-only). Baseline **`odb-read-parallel.json`** records Criterion central estimates on the factory VM (**4 logical CPUs**, Rust **1.99** stable).

| Threads | Median time (50k reads) | Throughput |
| ------- | ----------------------: | ---------- |
| 1 | ~149 ms | ~336 Kelem/s |
| 4 | ~51 ms | ~983 Kelem/s |

**Speedup (4 vs 1 thread): ~2.9×** on the factory VM (2026-10-11). This exercises the explicit batch read-context API and shared [`PackStore`](https://docs.rs/grit-lib/latest/grit_lib/pack_store/struct.PackStore.html) catalog, not ad-hoc per-read context installation.

```bash
cargo bench -p grit-lib --bench odb_read_parallel -- packed_blobs
```

## grit-bench: reachability bitmaps (`git.git`)

Scenario group **`bitmaps`** measures three acceptance metrics on upstream **`git/git`**: commit counting, object counting with bitmap index, and stateless **`upload-pack`** clone serving. The fixture is a bare **single-branch tag clone** at **`v2.47.0`** (override with **`GRIT_BENCH_GIT_GIT_TAG`**) under **`GRIT_BENCH_ODB_CACHE`**, prepared once with **Git-written** pack bitmaps (`git repack -adb`, `git commit-graph write --reachable`) and separately with **grit-written** pack bitmaps (`git repack -ad`, commit-graph, then `PackBitmapWriter` — see `ensure_bitmaps_grit_repo` in `grit-utils/src/bitmap_fixture.rs`). Library workloads use hidden **`grit-bench drive`** commands (`rev-list-count`, `rev-list-count-objects`, `serve-clone`); Git runs the equivalent **`git rev-list`** / **`git upload-pack`** invocations with the same isolated config as other grit-bench suites.

**Methodology.** Hyperfine warmups and minimum runs match other factory captures (see each JSON `machine` block). Ratios are **Grit median ÷ Git median**. **Peak RSS** is recorded per side via `grit-bench measure-rss` / hyperfine wrappers. **`serve-clone`** runs under **`GRIT_BENCH_SERVE_TIMEOUT_SECS`** (default **120**) and **`GRIT_BENCH_SERVE_MAX_RSS_GIB`** (default **12**). Reproducibility: run the suite twice back-to-back on the same fixture; medians should agree within **±10% relative** (`grit-bench compare … --tolerance 0.10` uses an absolute ratio tolerance — interpret large ratios accordingly).

```bash
cargo build --release -p grit-utils -p grit-cli
./target/release/grit-bench bitmaps --format json --output grit-utils/baselines/bitmaps-after.json
# Optional grit-written pack bitmap sidecar (separate JSON; default fixture stays Git-prepared):
GRIT_BENCH_BITMAP_FIXTURE=grit ./target/release/grit-bench bitmaps --format json --output grit-utils/baselines/bitmaps-grit-written.json
```

**Before (`bitmaps-before.json`, 2026-10-10, pre-reader/writer stack):** bitmap reader missing — object count ~**23 000×** vs Git; serve-clone hit **memory_cap**.

**After acceptance (`bitmaps-after.json`, factory VM 2026-10-16, release `grit` on the pack-reuse + bitmap stack):**

| Scenario | Git median | Grit median | Ratio | Grit peak RSS |
| --- | ---: | ---: | ---: | ---: |
| `rev-list-count-git.git` | 57.3 ms | 1033.2 ms | **18.0×** | ~285 MiB |
| `rev-list-count-objects-git.git` | 16.8 ms | 1064.6 ms | **63.2×** | ~286 MiB |
| `serve-clone-git.git` | 691.6 ms | *(120 s cap)* | — | timeout (`exit 124`) |

Two back-to-back runs on the git-prepared fixture (2026-10-16) agreed within **~6–8% relative** on Grit medians for the rev-list scenarios (`971 ms` vs `1033 ms` count; `984 ms` vs `1065 ms` object count).

**Grit-written bitmap fixture:** set **`GRIT_BENCH_BITMAP_FIXTURE=grit`** when running `grit-bench bitmaps` to prepare the same pinned **`git.git`** clone with **`git repack -ad`**, commit-graph, and a grit-lib **`PackBitmapWriter`** sidecar (`ensure_bitmaps_grit_repo`). The committed **`bitmaps-after.json`** uses the **Git-prepared** fixture (default); capture a separate JSON for grit-written bitmaps when comparing interoperability (see factory notes for the grit-fixture run ratios).

**Acceptance bar (roadmap item 14):** each ratio **≤ 1.2×** (and serve-clone within the same bar). The after run **passes compatibility** (bitmaps read/write, serve packs validate with `git index-pack --strict` after the thin-pack reuse fix) but **misses the 1.2× performance bar** on all three command-level metrics on this tag-heavy **`git.git`** fixture (~954 tag refs). Follow-ups (not retargeting the bar):

- **Multi-tip bitmap counting:** optimize `rev-list --count --all` / `--objects --use-bitmap-index` when hundreds of tag tips are present (Git ~60 ms vs grit ~1 s on the same fixture).
- **Serve-clone wall time:** upload-pack on the full closure must complete under the factory serve cap without timing out; profile pack reuse + enumeration on `git.git`.
- **Re-run `bitmaps-after.json`** after those land; keep **`bitmaps-before.json`** for regression context.

Intermediate capture on the same VM before the final writer stack (**2026-10-10**) remains in **`bitmaps-before.json`** for the generated tables (`rev-list-count` **~3.3×**, object count unusable, serve **memory_cap**).

## grit-lib: parallel index-pack (in-memory)

Release [`index_pack_bench`](https://github.com/gitbutlerapp/grit/tree/main/grit-lib/examples/index_pack_bench.rs) on a **~112 500-object** depth-50 pack (`git fast-import` + `git repack -adf --depth=50`, factory VM 2026-10-12). Grit uses [`pack_index_records_with_threads`](https://docs.rs/grit-lib/latest/grit_lib/unpack_objects/fn.pack_index_records_with_threads.html) (`GRIT_INDEX_PACK_BENCH_PACK` / `GRIT_INDEX_PACK_THREADS`); timings cover in-memory record building only (no `.idx` write). System **`git index-pack`** rows are end-to-end on a fresh temp directory each run (writes `.idx`).

| Command | Median (5 runs) |
| ------- | ---------------- |
| grit index-pack (records only), 1 thread | 1.21 s |
| grit index-pack (records only), 4 threads | 0.49 s |
| `git index-pack --threads=1` (end-to-end) | 1.25 s |
| `git index-pack --threads=4` (end-to-end) | 0.43 s |

**Speedup (4 vs 1 thread, grit records): ~2.47×.** Multi-threaded builds overlap pack scanning with batched parallel inflate+hash (ordered work pool; pack order preserved in the public record list). The single-thread path scans once and inflates+hashes serially—no redundant second skip pass. Delta chains still inflate, apply, and hash in parallel rounds.

Integration coverage uses a **≥ 50 000** object fixture built with `git fast-import` and shared across tests via `OnceLock` (`grit-lib/tests/index_pack_parallel.rs`); the default release suite completes in about **41 s** on the factory VM.

## Hashing

Raw digest throughput uses [`grit_lib::objects::hash::ObjectHasher`](https://docs.rs/grit-lib/latest/grit_lib/hash/enum.ObjectHasher.html) (release build, Criterion). OpenSSL numbers are from `openssl speed -evp sha1 -bytes 16384` and `openssl speed -evp sha256` (16 KiB blocks where shown). Git blob hashing uses `git hash-object` on a 256 MiB file vs the `gritx-hash-file` example (same canonical blob object id, no object-database write).

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

### Maintenance (`grit-bench maintenance`)

Baseline **`grit-utils/baselines/maintenance-medium.json`** compares full **`grit gc`**, **`grit gc repack --bitmaps`**, and **`grit gc commit-graph`** against **`git gc`**, **`git repack -adb`**, and **`git commit-graph write --reachable --changed-paths`** on the deep-history network fixture with a worktree seeded with extra loose objects. Each hyperfine run uses **`prepare-maintenance`** to restore that worktree so timings are not skewed by mutating the same checkout across iterations.

Recorded on the factory VM when the baseline was captured (see `machine` / `tools` in the JSON). Roadmap item **15** targets **≤ 1.1×** Git on large-repo scenarios; maintenance work is tracked separately until repack and commit-graph close the gap:

| Scenario | Grit ÷ Git (committed baseline) | vs 1.1× bar | Follow-up |
| --- | ---: | --- | --- |
| `gc-medium` | **7.31×** | miss | [GitHub #1026](https://github.com/gitbutlerapp/grit/issues/1026) — full in-process gc orchestration vs `git gc` |
| `repack-medium` | **2.58×** | miss | [#1026](https://github.com/gitbutlerapp/grit/issues/1026) — pack build / bitmap sidecars |
| `commit-graph-write` | **1.38×** | miss | [#1026](https://github.com/gitbutlerapp/grit/issues/1026) — changed-path Bloom generation |

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench maintenance --format json --output /tmp/maintenance-medium.json
```

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

**2026-10-11 status index lookups:** Replacing quadratic index scans during untracked/ignore classification dropped **`status-dirty-100000`** from ~**53×** to ~**2.5×** Git median time and **`status-dirty-10000`** from ~**7×** to ~**2×** (see refreshed `suite-after-LH.json`). A follow-up **size-mismatch** fast path on the status diff (Git `compare_stat_data`: different size ⇒ modified without re-reading blobs) brings **`status-dirty-10000`** to ~**1.94×** on the committed baseline.

**Suite at L/H** (`grit-utils/baselines/suite-after-LH.json`): **`status-dirty-10000`** is below the **2×** bar; dirty status at 100k remains ~**2.2×** (within **3×**); clean status at L/H is still above **2×**.

**H/L acceptance (1.25× bar):** after-run medians fail the scaling check for **`switch`**, **`switch-wide`**, **`pick`**, **`merge`**, and **`pick-series`** (not only switch/pick/merge). **`commit`** and **`add`** at H are within 2× of Git but **`add`** at L/H both exceed 2× at 10k and 100k files.

Two back-to-back hot-path after runs (`hot-paths-after-run1.json` vs `run2.json`) agree within **10% relative** on every scenario except `merge-100000` and `pick-100000` (~**13–14%** relative spread). The default `grit-bench compare` tolerance is an **absolute** 0.10 on the ratio, which is tighter than ±10% relative for large ratios (e.g. pick-series).

### Follow-up tracking (step 386)

No GitHub issues were filed from this factory run (`file_bug_report` is dogfooding-only). Track these against roadmap item 4:

- **L &gt;2×:** `switch`, `switch-wide`, `merge`, `pick-series`, `add` (10k and 100k), status at L/H.
- **H/L &gt;1.25×:** `switch`, `switch-wide`, `pick`, `merge`, `pick-series`.
- **Repeatability:** second hot-path after-run drift on `merge-100000` and `pick-100000` (~13–14% relative).
- **CLI gaps:** `grep`, `rebase`, `reset` (deferrals documented above).

**Criterion** (`cargo bench -p grit-lib --bench hot_paths`, release): see factory run log — `apply_stash_L` ~52 ms median; `pick_2000_paths_L` ~621 ms (library merge/checkout path, not CLI).

### Roadmap commands without a `grit` engine yet

| Git command | Grit CLI | How the plan covers it |
| --- | --- | --- |
| `grep` | none | Deferred — no porcelain driver in scope |
| `rebase` | none | `pick-series` grit-bench scenario (20× `grit pick`) as upper-bound proxy |
| `reset` | none | Deferred |
| `stash push` | `stash-push-pop` grit-bench scenario | `grit stash` + `grit stash pop` vs `git stash push` + `git stash pop` (see `stash-push-pop-LH.json`) |

### Refs

The **`grit-bench refs`** suite compares library drivers (`grit-bench refs-driver …`) to **`git for-each-ref`** and **`git update-ref --stdin`** on bare repositories with **100 000** branch refs. Fixtures are built once with system git and cached under the bench scratch directory: fully **packed** refs, **loose** refs only, and (when system git is **≥ 2.45**) a **reftable** layout. Scenarios list all refs, list the **`refs/heads/b05*`** glob (~1 000 refs), batch-**create** 100 000 refs into an empty-refs working copy, **update** 1 000 existing refs, and **delete** 1 000 refs; a prepare hook restores the fixture before each timed iteration. Initial packed/loose listing baselines are in **`grit-utils/baselines/refs-before.json`**; files-backend listing and batch **`update-ref`** optimizations on the 100k-ref fixtures are in **`grit-utils/baselines/refs-after.json`**; reftable timings (Git **≥ 2.45**) are in **`grit-utils/baselines/refs-after-reftable.json`**. The **`grit-utils/baselines/refs-after-994.json`** snapshot captures [`gc::update_refs`](https://docs.rs/grit-lib/latest/grit_lib/gc/fn.update_refs.html) batching through [`commit_ref_store_batch`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.commit_ref_store_batch.html) with a reused packed-refs cache instead of reparsing `packed-refs` per update.

On the **100k packed** fixture, **update 1 000 refs** (`update-ref-update-1k-refs-100k-packed`) dropped from **~184 s** Grit mean (4 485× git) to **~1.5 s** (**~36×** git at ~41 ms); the regression was repeated full `packed-refs` parses per update in the batch path, not transaction semantics.

Criterion micro-benchmarks for the reftable backend: **`cargo bench -p grit-lib --bench refs_reftable`** (`for_each_ref_100k`, `prefix_seek`, `transaction_create_10k`).

```bash
cargo build --release -p grit-cli -p grit-utils
GRIT_BENCH_BRANCH=factory/refs-bench-774 ./target/release/grit-bench refs --format json --output grit-utils/baselines/refs-before.json
GRIT_BENCH_BRANCH=factory/files-refs-perf-782 ./target/release/grit-bench refs --format json --output grit-utils/baselines/refs-after.json
GRIT_BENCH_BRANCH=factory/reftable-refs-perf-783 GRIT_BENCH_REFS_ONLY=reftable ./target/release/grit-bench refs --git "$(command -v git)" --format json --output grit-utils/baselines/refs-after-reftable.json
```

Use a **≥ 2.45** system **`git`** (or an explicit `--git` path) when refreshing the reftable baseline; older git skips the reftable fixture.

Set `GRIT_BENCH_BRANCH` to the GitButler branch you are benchmarking so `grit_commit` in the JSON matches that branch tip (required on `gitbutler/workspace` checkouts).

Driver **`for-each-ref`** output is checked against git byte-for-byte in `grit-utils` integration tests (1 000-ref fixture).

### Object reads

The **`grit-bench odb`** suite (library drivers in `grit-bench drive …` vs system **`git`**) measures cat-file batch reads, `rev-list --objects --all`, and `log -p` on a cached bare **git.git** clone and a repacked **100k-file / 1000-commit** synthetic repo. JSON reports include **peak RSS** per scenario (each workload measured in a fresh `grit-bench measure-rss` helper process via `getrusage(RUSAGE_CHILDREN)` on that run only) alongside wall time.

Reproduce:

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench odb --format json --output grit-utils/baselines/odb-read.json
```

**Measured outcomes:** the Object reads table below is generated from committed `grit-utils/baselines/odb-read.json` (hyperfine **≥5** runs per scenario, grit vs system git medians and peak RSS). Refresh with two invocations and `grit-bench compare run1.json run2.json --tolerance 0.10` before updating the baseline file and running `make docs`.

## grit-bench: deep-leaf worktree (issue #1017)

**`grit-bench deep-leaf`** builds the dogfood layout used on Windows release checks: many deep leaf directories with a few CRLF **`*.txt`** files each (via **`.gitattributes`**), a large **`large.bin`**, and tracked **`*.rs`** sources. Scenarios time clean **status**, dirty **status** after modifying **100** Rust files, and **diff** against system **`git`**. Use **`--smoke`** for a few hundred leaf dirs in CI; omit it for the full **~50k**-path production fixture.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench deep-leaf --smoke --format json --output /tmp/deep-leaf-smoke.json
```

## grit-bench: dense-leaf worktree (issue #1041)

**`grit-bench dense-leaf`** builds the Windows dogfood layout with **many small files per leaf directory** (~**50** **`*.txt`** files under each of **~1000** deep prefixes, plus a few extra-deep chains). Scenarios time clean **status**, dirty **status** after modifying **100** text files, **diff**, and **add** against system **`git`**. The **add** scenario uses a hyperfine **`--prepare`** hook that runs **`git reset -q HEAD`** and re-dirties the same **100** files before each timed run. Use **`--smoke`** (~**4k** tracked paths) in CI; omit it for the full **~50k**-path production fixture. Release checks on Windows should compare medians to **`git`** and expect ratios within **3×** on warm-cache runs.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench dense-leaf --smoke --format json --output /tmp/dense-leaf-smoke.json
```

## grit-bench: parallelism (threads=1 vs auto)

The **`grit-bench parallel`** suite measures how wall time scales when worker threads are pinned to **1** versus **auto** (unset `index.threads`, `checkout.workers`, and `pack.threads`, so system **git** and **grit** use all logical CPUs). Each scenario runs twice in the JSON baseline: ids end with **`-t1`** or **`-tauto`**. Hermetic git config matches other suites (`GIT_CONFIG_NOSYSTEM`, empty global config, `gc.auto=0`), with extra keys injected through **`GIT_CONFIG_COUNT`** for the one-thread runs. When the **`grit`** binary exposes a global **`--threads`** flag, the suite passes **`--threads=1`** or **`--threads=0`** (auto) in addition to those config keys.

Fixtures reuse the synthetic **100 000**-file repo (same shape as **`synthetic-100000`** / hot-path **H** tier). Scenarios:

| Workload | Git command / config | Grit |
| --- | --- | --- |
| Status dirty / clean | `git status -s` | `grit status` |
| Fresh checkout | `git checkout -f HEAD` after `git clone --no-checkout` | `grit-bench drive checkout-branch` (library checkout) |
| `file://` clone | `git clone` | `grit clone` |
| Wide switch | `git switch` out and back (~10% path delta) | `grit switch` |
| Index-pack | `git index-pack --threads=…` on the repacked `.pack` | `grit-bench drive index-pack` |
| Pack writing | `git pack-objects --all --stdout` | `grit-bench drive pack-build-all` |
| Rename diff | `git diff -M` (~2000 renames) | `grit-bench drive diff-rename-m` |

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench parallel --format json --output grit-utils/baselines/parallel-before.json
./target/release/grit-bench parallel --format json --output grit-utils/baselines/parallel-after.json
./target/release/grit-bench compare grit-utils/baselines/parallel-before.json grit-utils/baselines/parallel-after.json --tolerance 0.10
make docs
```

The committed **`parallel-before.json`** baseline captures **before** parallelism work (maintenance plan step 997). **`parallel-after.json`** (2026-10-12, roadmap item 15 close-out) captures the same scenarios after the full parallelism stack on **`origin/main`**, with **`git` 2.55.0**, release **`grit` 0.5.6**, hyperfine **1.19.0**, and **4** logical CPUs (see `machine` / `tools` in each JSON).

The suite reuses the repacked **100k** synthetic repo from **`GRIT_BENCH_ODB_CACHE`**, caches status and rename fixtures, and uses at least **7** hyperfine warmups and **10** timed runs per command (still overridable with global **`--warmup`** / **`--min-runs`**). Noisy scenarios warm **`git diff`** and **`pack-objects`** in **`--prepare`**. Fresh checkout warms on a **disposable** sibling clone, then re-clones **`--no-checkout`** into the timed destination (asserted empty) before each iteration. Generated site tables include **threads** in the scenario id suffix; compare **`-t1`** and **`-tauto`** rows for the same workload to see scaling. **`machine.logical_cores`** in the JSON records core count at capture time.

### Parallelism acceptance (100k synthetic repo, `-tauto`)

Roadmap item **15** bars **Grit median ÷ Git median ≤ 1.1×** on the large-repo scenarios below (auto threads for both tools). Ratios come from **`parallel-after.json`**.

| Scenario | Grit ÷ Git | vs 1.1× bar | Profile / notes |
| --- | ---: | --- | --- |
| **`parallel-status-dirty-100000-tauto`** | **2.67×** | miss | Down from ~**50×** in **`parallel-before.json`** after parallel stat/hash and untracked walks; remaining gap is mostly index refresh + untracked enumeration vs Git’s preload path on this fixture. |
| **`parallel-status-clean-100000-tauto`** | **3.28×** | miss | Clean worktrees still pay full index/untracked setup; Git **2.55** median ~**65 ms** vs Grit ~**213 ms**. |
| **`parallel-checkout-fresh-100000-tauto`** | **2.05×** | miss | Library checkout after **`clone --no-checkout`**; parallel file writers help but tree/blob materialization and index writes dominate. |
| **`parallel-clone-file-100000-tauto`** | **1.53×** | miss | **`file://`** clone including checkout; within **1.6×** but above the **1.1×** close-out bar. |
| **`parallel-index-pack-100000-tauto`** | **2.03×** | miss | In-memory **`drive index-pack`** on the repacked **100k** `.pack`; **~2.1×** Grit speedup **t1→tauto** but Git’s **`index-pack --threads=auto`** baseline is still faster. |

**Within bar elsewhere:** refreshed **`network.json`** **`clone-file-prod`** ≈ **0.95×** (deep-history **`file://`** clone). **`server-clone-http-compare-prod`** remains ≫ **1.2×** (upload-pack without reachability bitmaps — tracked under the bitmap/MIDX item, not parallelism).

### Thread scaling (`parallel-after.json`, Grit medians)

On **4** logical CPUs, pinning **`--threads=1`** vs **`--threads=0`** (auto) shows where pool policy pays off:

- **Status (dirty/clean, 100k):** ~**1.17–1.19×** faster at auto vs single-threaded Grit — parallel stat/hash helps, but the workload is still Git-limited on ratio.
- **`parallel-index-pack-100000`:** **~2.12×** faster **t1→tauto** (Grit **~713 ms** → **~337 ms**); largest win in the suite.
- **`parallel-switch-wide-100000`:** **~1.02×** **t1→tauto** — still **~13×** Git at auto (checkout/merge bound; not part of the 1.1× bar).
- **`parallel-pack-write-100000`:** **~3×** **t1→tauto** on Grit medians (delta encode + zlib pool); ratio vs Git at auto ~**75×** (Git streams a highly optimized **`pack-objects`**).

Refreshed companion baselines on the same VM capture **`status-*` / `add-*`** in **`suite-after-LH.json`**, **`switch-*`** in **`hot-paths-after.json`**, and **`clone-file-prod`** / **`server-clone-http-compare-prod`** in **`network.json`** (Git **2.55** for suite/network portions of the capture run).

Integration smoke (tiny fixtures, sixteen scenarios; requires **hyperfine** on `PATH`): `cargo test -p grit-utils parallel_scenario_smoke_end_to_end`.

## grit-bench: network (clone, fetch, push, ls-remote)

The **`grit-bench network`** suite compares **`grit`** and system **`git`** on the same remote URLs. Cached fixtures live under **`GRIT_BENCH_NETWORK_CACHE`** (default `/tmp/grit-bench-network-cache`):

| Fixture | Shape |
| --- | --- |
| **deep-history** | **≥50 000** commits and **≥20 000** tracked files (built once via `git fast-import`) |
| **many-refs** | deep-history plus **≥10 000** `refs/heads/bench-ref-*` branches |
| **large-blobs** | a small tree with multi‑MiB blobs for pack-heavy transfer |

Scenarios include **clone** over **`file://`**, **git http-backend** (local CGI), and **grit-http-server** smart HTTP; **incremental** and **no-op fetch** over all three transports; **push** over **`file://`** and **grit-http-server** (git http-backend push is omitted until the bench CGI harness supports receive-pack); **`git ls-remote`** vs **`grit remote refs`** on the many-refs fixture over all three transports; and a **server-side** comparison that times system **`git clone`** against **git http-backend** vs **grit-http-server** on the same published bare repo. Hermetic git config matches other suites (`GIT_CONFIG_NOSYSTEM`, empty global config).

```bash
cargo build --release -p grit-cli -p grit-http-server -p grit-utils
./target/release/grit-bench network --format json --output grit-utils/baselines/network.json
./target/release/grit-bench compare grit-utils/baselines/network.json /tmp/network-rerun.json --tolerance 0.10
make docs
```

Integration smoke (tiny fixtures, fifteen scenarios; requires **hyperfine** on `PATH`): `cargo test -p grit-utils network_scenario_smoke_end_to_end`.

Acceptance bars for the network performance pass are **≤1.2×** git wall time on client-side scenarios (clone, fetch, push, ls-remote over `file://` and smart HTTP). Compare the generated table ratios to that bar.

**Server-side clone (`server-clone-http-compare-*`):** system **`git clone`** against **grit-http-server** is expected to stay above **1.2×** until the upload-pack path uses **reachability bitmaps** during negotiation (same dependency as Git’s `upload-pack` on large, pack-served repos). Client-side grit and git both pay similar pack transfer cost; the gap is dominated by grit’s in-process upload-pack enumeration without bitmaps, not HTTP keep-alive or side-band framing. Track under the MIDX/bitmap roadmap item rather than masking with compatibility shortcuts.

Regenerate release binaries with **`cargo build --release -p grit-lib`** before **`grit-bench network`** so the timed **`grit`** binary includes library changes.

## grit-bench: history rewrite (`rewrite-history`)

The **`grit-bench rewrite-history`** suite times **`grit history`** reword, drop, fixup, and squash against system **`git history`** when the installed git is **≥2.54**, and always against a scripted **`git rebase`** / **`git rebase --onto`** / **`git reset --soft`** baseline on the same synthetic fixture. The fixture is generated once under **`GRIT_BENCH_SCRATCH`** (default `/tmp/grit-bench-scratch`): **~10 000** tracked files, **5 000** linear commits on `main`, and **50** local branches at varied depths (`bench/branch-*`). Each hyperfine iteration resets the worktree from a bare snapshot so refs stay isolated between runs.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench rewrite-history --format json --output grit-utils/baselines/history.json
./target/release/grit-bench rewrite-history --smoke   # CI-sized fixture, one run per scenario
```

**Acceptance bars (factory VM, committed `history.json`):** deep **reword** ≤**1.0×** `git history reword` and ≥**5×** faster than the **`git rebase -i`** equivalent; other scenarios ≤**1.1×** `git history` when available. The baseline records the system **`git`** version string in `tools.git`.

**2026-10-17 status:** On **git 2.43.0** (no `git history` subcommand), git-side timings use the rebase/reset scripts above. **Reword**, **drop**, and **squash** meet the bars (reword ≈**0.03×** vs rebase, well above the **5×** grit-faster-than-rebase bar). **`history-fixup-500`** remains ≈**3.4×** git (`git commit --fixup` + **`rebase --autosquash`**) because grit performs a full three-way replay for every descendant commit while Git’s autosquash path stays on the C merge fast paths on this fixture. **Follow-up:** factory performance work on **`grit-lib` history fixup** replay (merge reuse / commit-graph scoped walks) — tracked as a separate factory item; do not weaken merge semantics to green the ratio.

## Real-world campaign

The **campaign** scorecard runs a shared CLI matrix on real-shaped fixtures: upstream **git.git** (pinned tag, repacked with bitmaps), a **chromium-class synthetic** repo (`400 000` files, `40 000` dirs, `100 000` commits, `50 000` refs, single pack + commit-graph), and optionally **linux** (`--with-linux` or `GRIT_BENCH_LINUX=1`). Scenario ids look like `git-dot-git/status-dirty` and `chromium-synth/switch`; reachability **rev-list** / **serve-clone** rows are owned by **ROADMAP item 14** (reachability bitmaps) and are not scored in this pass.

**Methodology.** Each scenario uses **hyperfine** (3 warmup, ≥5 timed runs) on the system **`git`** binary and a release **`grit`** build with hermetic git config. Ratios are **grit_median_ms / git_median_ms**; the bar is **≤1.1×** unless documented in [`grit-utils/perf-targets.toml`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/perf-targets.toml). Grit-side OOM or unsupported commands are recorded once (git timing still captured) so the suite can finish on the factory VM.

**Reproduce on the factory VM** (reuse `GRIT_BENCH_ODB_CACHE` after the first run):

```bash
cargo build --release -p grit-cli -p grit-utils
export GRIT_BENCH_ODB_CACHE=/tmp/grit-bench-odb-cache
./target/release/grit-bench campaign --no-network --format json --output grit-utils/baselines/campaign-after.json
./target/release/grit-bench campaign --ledger --from grit-utils/baselines/campaign-after.json
```

Committed baselines: **`campaign-before.json`** (2026-10-14, pre-campaign stack; chromium smoke-sized fixture) and **`campaign-after.json`** (2026-10-17, post-campaign stack on production synthetic). Both are listed under `[benchmarks].baseline` in `content/docs/site.toml`; the site merges duplicate scenario ids with **after** winning for campaign files.

**Before / after (representative sample — 15 of 30 overlapping scenario ids).** Values compare grit/git ratios from the two JSON reports (not absolute wall times). The full overlap set is in the committed JSON baselines and the generated campaign rows below. Ratios **below 1×** mean grit improved relative to git; ratios **above 1×** mean the gap widened (the after capture uses the production synthetic worktree while the before capture used a smaller smoke fixture for chromium-class scenarios).

| Scenario | Before ratio | After ratio | After / before |
| --- | ---: | ---: | ---: |
| `git-dot-git/clone-file` | 3.17× | 0.05× | 0.02× |
| `git-dot-git/diff-clean` | 7.76× | 2.46× | 0.32× |
| `git-dot-git/merge` | 14.66× | 6.55× | 0.45× |
| `git-dot-git/resolve` | 18.38× | 15.99× | 0.87× |
| `git-dot-git/status-clean` | 4.15× | 4.15× | 1.00× |
| `chromium-synth/status-dirty` | 1.87× | 2.10× | 1.13× |
| `git-dot-git/commit` | 1.84× | 2.42× | 1.32× |
| `git-dot-git/switch` | 4.37× | 5.89× | 1.35× |
| `git-dot-git/add` | 2.32× | 4.48× | 1.93× |
| `chromium-synth/add` | 2.07× | 2.88× | 1.39× |
| `chromium-synth/switch` | 5.16× | 13.43× | 2.60× |
| `chromium-synth/diff-clean` | 2.45× | 7.82× | 3.18× |
| `git-dot-git/cat` | 12.56× | 63.36× | 5.04× |
| `chromium-synth/merge` | 2.47× | 17.95× | 7.26× |
| `chromium-synth/commit` | 0.90× | 11.84× | 13.15× |

Full ledger (51 scenarios): `grit-bench campaign --ledger --from grit-utils/baselines/campaign-after.json` or see **`campaign-after.ledger.md`** in `grit-utils/baselines/`.

**Gap ledger.** Every scenario above **1.1×** (or grit-side skip) has a `documented` entry in `perf-targets.toml` pointing at **ROADMAP items 7, 11, 14, 19, and 21–30**. Item 14 rows remain `owned-by-item-14`.

**Superseded micro-baselines.** Per-command JSON files (`hot-paths-after.json`, `commit-after-LH.json`, etc.) remain for focused regressions; the **campaign** pair is the authoritative before/after scorecard for real-repository CLI work. Prefer updating campaign baselines when a change targets end-to-end porcelain on git.git or large synthetic worktrees.

## grit-bench: repository fsck (`fsck-medium`)

Scenario **`fsck-medium`** (`grit-bench fsck`) runs **`grit fsck --no-dangling`** and **`git fsck --no-dangling`** on a cloned deep-history worktree (same fixture as other network-cache scenarios). Baseline: **`grit-utils/baselines/fsck-medium.json`**.

**Acceptance bar:** ≤**1.1×** git median wall time (same bar as other maintenance hot paths).

**2026-10-13 status:** After aligning workloads (`--no-dangling` on both sides), the committed baseline is still ~**9.4×** git (~**1840 ms** vs ~**196 ms** median on the factory VM). Grit pays full Rust-side loose/pack verification and connectivity walks without the mature C fsck fast paths Git uses on this fixture. **Follow-up:** [factory step #1475](https://maint.grit-scm.com/factory/1475) performance close-out (parallel pack verification, avoid redundant reads, bitmap-assisted reachability where applicable) — do not weaken checks to match the ratio.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench fsck --format json --output grit-utils/baselines/fsck-medium.json
```

### Results

<!-- grit:benchmark-tables -->

To refresh this page after updating a baseline JSON file, regenerate the static site:

```bash
make docs
```

The docs `--check` step fails if baseline numbers change without re-running `make docs`.

For other library surfaces under test, see the [grit-lib API on docs.rs](https://docs.rs/grit-lib).
